// SPDX-License-Identifier: Apache-2.0
//! Rollback snapshots for updates: `<prefix>/.mudhut/snapshots/<id>/`.
//!
//! What a snapshot holds (taken right before the update changes anything, with the prefix's wine
//! session ended so the registry files are complete):
//! - TREES, put back whole by rename: the app folder; every folder Adobe's record says the
//!   app's packages install into (`TargetFolderList`), when it belongs to this app (not a shared
//!   root such as `Common Files\Adobe` itself); Adobe's installer records `caps\` and
//!   `Installers\` (hdpim.db and the per-package uninstall lists).
//! - FILES under shared roots, one by one, from Adobe's own uninstall lists (`.pimx` DeleteFile
//!   entries): e.g. Photoshop's Support package writes into `Common Files\Adobe` itself, which
//!   also holds every other app's files, so only Photoshop's files are kept.
//! - the registry: `system.reg`, `user.reg`, `userdef.reg`.
//! - the user's preference folders (Adobe marks them `isUserPreferences`): kept so they can be
//!   put back if a failed update removed them; never written over newer ones.
//! Not included: documents, projects, media caches, downloaded AI models outside the app's own
//! folders, and SIGN-IN. Adobe keeps the signed-in account in Wine's Credential Manager (inside
//! `user.reg`) and in its own files under `AppData\Local\Adobe` (NGL, OOBE, licflags). Those files
//! are never in a snapshot, and whenever `user.reg` is put back, the Credential Manager sections
//! currently in the prefix are kept (`keep_current_sections`), so a rollback can never sign the
//! user out or bring back an older account.
//!
//! How: on btrfs and XFS every file is a reflink (`FICLONE`), so a snapshot of a 10 GB app takes
//! seconds and almost no space until the update rewrites files. Elsewhere it is a real copy when
//! the disk has room (and the update is refused with `snapshot-space`, exit 6, when it does not).
//! Never `cp --reflink=auto`: it silently falls back to a full copy and can fill the disk.
//!
//! Rollback order: a snapshot can be put back only while no later update of the same prefix is
//! still in place (newest first), because `caps\`, `Installers\` and the registry are shared by
//! every app in the prefix.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::hdpimdb::{Db, Product};

pub const HIVES: &[&str] = &["system.reg", "user.reg", "userdef.reg"];

/// FICLONE from linux/fs.h: _IOW(0x94, 9, int).
const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Prefix-relative path ("drive_c/Program Files/Adobe/Adobe Photoshop 2026").
    pub rel: String,
    /// "tree" | "file" | "prefs".
    pub kind: String,
    /// It existed before the update (if not, rollback removes what the update created there).
    pub existed: bool,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema: u32,
    pub id: String,
    pub created: String,
    pub created_secs: u64,
    pub app: String,
    pub sap: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// "reflink" | "copy".
    pub mode: String,
    /// "pre-update" | "committed" | "rolled-back" | "rollback-failed".
    pub state: String,
    pub mudhut: String,
    pub entries: Vec<Entry>,
    pub hives: Vec<String>,
}

pub fn root(prefix: &Path) -> PathBuf {
    crate::state::dir(prefix).join("snapshots")
}

/// The snapshot tree spells `drive_c` as `C`, so nothing that looks for prefixes (a folder
/// holding `drive_c`) can mistake a snapshot for one.
fn stored(rel: &str) -> String {
    rel.strip_prefix("drive_c").map(|r| format!("C{r}")).unwrap_or_else(|| rel.to_string())
}

fn meta_path(dir: &Path) -> PathBuf {
    dir.join("snapshot.json")
}

pub fn write_meta(dir: &Path, m: &Meta) -> Result<()> {
    let tmp = dir.join("snapshot.json.part");
    fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    fs::rename(&tmp, meta_path(dir))?;
    Ok(())
}

pub fn list(prefix: &Path) -> Vec<(PathBuf, Meta)> {
    let mut out: Vec<(PathBuf, Meta)> = fs::read_dir(root(prefix))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            let m: Meta = serde_json::from_str(&fs::read_to_string(meta_path(&d)).ok()?).ok()?;
            Some((d, m))
        })
        .collect();
    out.sort_by_key(|(_, m)| m.created_secs);
    out
}

// ---------------------------------------------------------------- scope

/// The prefix's Windows user ("nuck", "steamuser"): the one folder under drive_c/users that is
/// not Public.
fn prefix_user(prefix: &Path) -> Option<String> {
    fs::read_dir(prefix.join("drive_c/users")).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().to_string();
        (n != "Public" && e.path().is_dir()).then_some(n)
    })
}

/// Resolve an Adobe path token (`[AdobeCommon]\...`, `[INSTALLDIR]\...`, or a plain `C:\...`) to
/// a prefix-relative path. None for tokens we do not know (left out of the snapshot).
pub fn resolve(token_path: &str, install_dir: Option<&str>, user: &str) -> Option<String> {
    let p = token_path.trim().replace('/', "\\");
    let (tok, rest) = match p.strip_prefix('[') {
        Some(t) => {
            let i = t.find(']')?;
            (Some(t[..i].to_ascii_lowercase()), t[i + 1..].trim_start_matches('\\').to_string())
        }
        None => (None, String::new()),
    };
    let win: String = match tok.as_deref() {
        None => p.clone(),
        Some(t) => {
            let base = match t {
                "installdir" => install_dir?.to_string(),
                "adobecommon" => "C:\\Program Files\\Common Files\\Adobe".into(),
                "adobeprogramfiles" => "C:\\Program Files\\Adobe".into(),
                "programfiles" => "C:\\Program Files".into(),
                "programfilesx86" => "C:\\Program Files (x86)".into(),
                "commonfiles" => "C:\\Program Files\\Common Files".into(),
                "commonfilesx86" => "C:\\Program Files (x86)\\Common Files".into(),
                "userroamingappdata" => format!("C:\\users\\{user}\\AppData\\Roaming"),
                "userlocalappdata" => format!("C:\\users\\{user}\\AppData\\Local"),
                "userdocuments" => format!("C:\\users\\{user}\\Documents"),
                "shareddocuments" => "C:\\users\\Public\\Documents".into(),
                "sharedapplicationdata" | "commonappdata" => "C:\\ProgramData".into(),
                _ => return None,
            };
            if rest.is_empty() { base } else { format!("{}\\{rest}", base.trim_end_matches('\\')) }
        }
    };
    // A path recorded on Windows names the Windows user; inside the prefix it is ours.
    let mut parts: Vec<String> = win.split('\\').filter(|c| !c.is_empty()).map(str::to_string).collect();
    if parts.first().map(|d| d.eq_ignore_ascii_case("C:")) != Some(true) {
        return None;
    }
    parts[0] = "drive_c".into();
    if parts.len() > 2 && parts[1].eq_ignore_ascii_case("users") && !parts[2].eq_ignore_ascii_case("Public") {
        parts[2] = user.to_string();
    }
    Some(parts.join("/"))
}

/// Folders that hold more than one app's files: never put back whole.
fn shared_roots(user: &str) -> Vec<String> {
    let mut v: Vec<String> = [
        "drive_c",
        "drive_c/Program Files",
        "drive_c/Program Files (x86)",
        "drive_c/Program Files/Adobe",
        "drive_c/Program Files (x86)/Adobe",
        "drive_c/Program Files/Common Files",
        "drive_c/Program Files/Common Files/Adobe",
        "drive_c/Program Files (x86)/Common Files",
        "drive_c/Program Files (x86)/Common Files/Adobe",
        "drive_c/ProgramData",
        "drive_c/ProgramData/Adobe",
        "drive_c/users",
        "drive_c/users/Public",
        "drive_c/users/Public/Documents",
        "drive_c/users/Public/Documents/Adobe",
        "drive_c/windows",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for sub in ["", "/AppData", "/AppData/Roaming", "/AppData/Roaming/Adobe", "/AppData/Local", "/AppData/Local/Adobe", "/Documents", "/Documents/Adobe"] {
        v.push(format!("drive_c/users/{user}{sub}"));
    }
    v
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn inside(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path[..dir.len()].eq_ignore_ascii_case(dir) && path.as_bytes()[dir.len()] == b'/'
}

/// Parsed `.pimx` commands: (DeleteFile targets, recursive DeleteDirectory targets, user-pref dirs).
fn pimx_targets(path: &Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let Ok(text) = fs::read_to_string(path) else { return (vec![], vec![], vec![]) };
    let (mut files, mut dirs, mut prefs) = (vec![], vec![], vec![]);
    for el in text.split('<').skip(1) {
        let tag = el.split(|c: char| c.is_whitespace() || c == '/' || c == '>').next().unwrap_or("");
        let Some(i) = el.find("target=\"") else { continue };
        let t = &el[i + 8..];
        let Some(j) = t.find('"') else { continue };
        let target = t[..j].replace("&amp;", "&").replace("&apos;", "'").replace("&quot;", "\"");
        let user_pref = el.contains("isUserPreferences=\"true\"");
        match tag {
            "DeleteFile" if !user_pref => files.push(target),
            "DeleteDirectory" if user_pref => prefs.push(target),
            "DeleteDirectory" if el.contains("isRecursiveDelete=\"true\"") => dirs.push(target),
            _ => {}
        }
    }
    (files, dirs, prefs)
}

/// What to keep for an update of `product` (and of the dependency records in `deps`) in the
/// app folder `app_dir_rel`.
pub fn scope(prefix: &Path, app_dir_rel: &str, product: Option<&Product>, deps: &[&Product]) -> Vec<Entry> {
    let user = prefix_user(prefix).unwrap_or_else(|| "user".into());
    let shared = shared_roots(&user);
    let mut trees: BTreeSet<String> = BTreeSet::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut prefs: BTreeSet<String> = BTreeSet::new();
    trees.insert(app_dir_rel.to_string());
    trees.insert("drive_c/Program Files (x86)/Common Files/Adobe/caps".into());
    trees.insert("drive_c/Program Files (x86)/Common Files/Adobe/Installers".into());
    let mut add_product = |p: &Product, trees: &mut BTreeSet<String>| {
        let idir = p.install_dir();
        for meta in p.package_meta.values() {
            if let Some(list) = meta.get("TargetFolderList") {
                for t in list.split(',') {
                    if let Some(rel) = resolve(t, idir, &user) {
                        if !shared.iter().any(|s| eq_ci(s, &rel)) {
                            trees.insert(rel);
                        }
                    }
                }
            }
            if let Some(pimx) = meta.get("UninstallPIMX").and_then(|w| crate::hdpimdb::win_to_prefix(prefix, w)) {
                let (fs_, ds, ps) = pimx_targets(&pimx);
                for d in ds.iter().filter_map(|d| resolve(d, idir, &user)) {
                    if !shared.iter().any(|s| eq_ci(s, &d)) {
                        trees.insert(d);
                    }
                }
                for f in fs_.iter().filter_map(|f| resolve(f, idir, &user)) {
                    files.insert(f);
                }
                for d in ps.iter().filter_map(|d| resolve(d, idir, &user)) {
                    prefs.insert(d);
                }
            }
        }
    };
    if let Some(p) = product {
        add_product(p, &mut trees);
    }
    for d in deps {
        add_product(d, &mut trees);
    }
    // Preference folders are handled on their own (restored only if missing), never as trees.
    trees.retain(|t| !prefs.iter().any(|p| eq_ci(p, t) || inside(t, p) || inside(p, t)));
    // Keep only the outermost trees.
    let all: Vec<String> = trees.iter().cloned().collect();
    trees.retain(|t| !all.iter().any(|o| inside(t, o)));
    // Nested preference folders: the outermost one covers the rest.
    let pall: Vec<String> = prefs.iter().cloned().collect();
    prefs.retain(|p| !pall.iter().any(|o| inside(p, o)));
    // Files inside a tree are already covered.
    files.retain(|f| !trees.iter().any(|t| inside(f, t)) && !prefs.iter().any(|p| inside(f, p)));
    let mut out: Vec<Entry> = trees.into_iter().map(|rel| Entry { rel, kind: "tree".into(), existed: false, files: 0, bytes: 0 }).collect();
    out.extend(files.into_iter().map(|rel| Entry { rel, kind: "file".into(), existed: false, files: 0, bytes: 0 }));
    out.extend(prefs.into_iter().map(|rel| Entry { rel, kind: "prefs".into(), existed: false, files: 0, bytes: 0 }));
    out
}

// ---------------------------------------------------------------- copy / clone

/// Whether reflinks work between `a_dir` and files under it (probe with a scratch file).
pub fn reflink_works(dir: &Path) -> bool {
    let a = dir.join(".mudhut-reflink-probe-a");
    let b = dir.join(".mudhut-reflink-probe-b");
    let ok = (|| -> std::io::Result<bool> {
        fs::write(&a, b"probe")?;
        let src = fs::File::open(&a)?;
        let dst = fs::File::create(&b)?;
        Ok(unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0)
    })()
    .unwrap_or(false);
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    ok
}

fn clone_file(src: &Path, dst: &Path, reflink: bool) -> std::io::Result<u64> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let t = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(t, dst)?;
        return Ok(0);
    }
    if reflink {
        let s = fs::File::open(src)?;
        let d = fs::OpenOptions::new().write(true).create(true).truncate(true).open(dst)?;
        if unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        fs::copy(src, dst)?;
    }
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    let times = [
        libc::timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
        libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
    ];
    let c = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes()).unwrap_or_default();
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    Ok(meta.len())
}

/// Copy (or clone) `src` -> `dst`, a file or a whole tree. Returns (files, bytes).
fn copy_any(src: &Path, dst: &Path, reflink: bool) -> Result<(u64, u64)> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if !meta.is_dir() {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let b = clone_file(src, dst, reflink).with_context(|| format!("copying {}", src.display()))?;
        return Ok((1, b));
    }
    let (mut n, mut bytes) = (0u64, 0u64);
    for e in walkdir::WalkDir::new(src).follow_links(false) {
        let e = e?;
        let rel = e.path().strip_prefix(src).unwrap_or(e.path());
        let to = dst.join(rel);
        if e.file_type().is_dir() {
            fs::create_dir_all(&to)?;
            let _ = fs::set_permissions(&to, fs::Permissions::from_mode(e.metadata()?.mode() & 0o7777));
        } else {
            if let Some(p) = to.parent() {
                fs::create_dir_all(p)?;
            }
            bytes += clone_file(e.path(), &to, reflink).with_context(|| format!("copying {}", e.path().display()))?;
            n += 1;
        }
    }
    Ok((n, bytes))
}

/// Logical size of a file or tree (for the copy-mode space check).
pub fn size_of(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => walkdir::WalkDir::new(path).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

// ---------------------------------------------------------------- create / restore

/// Take a snapshot of `entries` + the hives. The prefix's wine session must be ended (the
/// caller quiesces it). `mode`: None = reflink if possible, else copy if space allows.
pub fn create(prefix: &Path, app: &str, sap: &str, from: Option<&str>, to: Option<&str>, entries: Vec<Entry>) -> Result<(PathBuf, Meta)> {
    let now = crate::clock::now_secs();
    let id = format!("{}-{}-{}", crate::clock::stamp(now), sap, from.unwrap_or("unknown"));
    let dir = root(prefix).join(&id);
    fs::create_dir_all(dir.join("files")).with_context(|| format!("creating {}", dir.display()))?;
    let reflink = reflink_works(&dir);
    if !reflink {
        let need: u64 = entries.iter().map(|e| size_of(&prefix.join(&e.rel))).sum::<u64>()
            + HIVES.iter().map(|h| size_of(&prefix.join(h))).sum::<u64>();
        let free = free_bytes(&dir).unwrap_or(0);
        let margin = 2u64 << 30;
        if free < need + margin {
            let _ = fs::remove_dir_all(&dir);
            return Err(crate::failure::Failure::new(
                "snapshot-space",
                crate::failure::EX_DISK,
                format!(
                    "There is not enough free disk space to keep a copy for rolling back ({:.1} GB needed, {:.1} GB free). \
                     Free some space, or turn off the rollback copy in the update settings. Nothing was changed.",
                    (need + margin) as f64 / 1e9,
                    free as f64 / 1e9
                ),
            )
            .with("prefix_changed", false)
            .into());
        }
    }
    let mut meta = Meta {
        schema: 1,
        id: id.clone(),
        created: crate::clock::rfc3339(now),
        created_secs: now,
        app: app.into(),
        sap: sap.into(),
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        mode: if reflink { "reflink" } else { "copy" }.into(),
        state: "creating".into(),
        mudhut: env!("CARGO_PKG_VERSION").into(),
        entries: vec![],
        hives: vec![],
    };
    write_meta(&dir, &meta)?;
    for mut e in entries {
        let src = prefix.join(&e.rel);
        e.existed = fs::symlink_metadata(&src).is_ok();
        if e.existed {
            let (n, b) = copy_any(&src, &dir.join("files").join(stored(&e.rel)), reflink)?;
            e.files = n;
            e.bytes = b;
        }
        meta.entries.push(e);
    }
    fs::create_dir_all(dir.join("hives"))?;
    for h in HIVES {
        let src = prefix.join(h);
        if src.is_file() {
            copy_any(&src, &dir.join("hives").join(h), false)?;
            meta.hives.push(h.to_string());
        }
    }
    meta.state = "pre-update".into();
    write_meta(&dir, &meta)?;
    Ok((dir, meta))
}

/// Where Wine keeps Windows Credential Manager entries (Adobe's signed-in account among them),
/// as a section header prefix in `user.reg`.
pub const CREDENTIAL_MANAGER: &[u8] = b"[Software\\\\Wine\\\\Credential Manager";

/// A Wine registry file with every section whose header starts with `prefix` taken from
/// `current` instead of `saved`. Sections are a header line `[Key\\Path] <time>` plus the lines
/// up to the next header; Wine reads them in any order. Bytes are kept as they are.
pub fn keep_current_sections(saved: &[u8], current: &[u8], prefix: &[u8]) -> Vec<u8> {
    fn sections(text: &[u8]) -> (Vec<u8>, Vec<(bool, Vec<u8>)>) {
        // (preamble before the first section, [(is_match, section bytes)])
        let mut pre = Vec::new();
        let mut out: Vec<(bool, Vec<u8>)> = Vec::new();
        for line in text.split_inclusive(|&b| b == b'\n') {
            if line.first() == Some(&b'[') {
                out.push((false, Vec::new()));
            }
            match out.last_mut() {
                Some((_, sec)) => sec.extend_from_slice(line),
                None => pre.extend_from_slice(line),
            }
        }
        (pre, out)
    }
    let (pre, saved_secs) = sections(saved);
    let (_, cur_secs) = sections(current);
    let is_match = |sec: &[u8]| sec.starts_with(prefix);
    let mut out = pre;
    for (_, sec) in saved_secs.iter() {
        if !is_match(sec) {
            out.extend_from_slice(sec);
        }
    }
    for (_, sec) in cur_secs.iter() {
        if is_match(sec) {
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            out.extend_from_slice(sec);
        }
    }
    out
}

/// Put a snapshot back. `hives`: which registry files to restore (an automatic rollback restores
/// all three; a later, manual one keeps `user.reg`, the user's own settings since the update).
/// The prefix's wine session must be ended. Consumes the snapshot's trees (they are renamed
/// into place).
pub fn restore(prefix: &Path, dir: &Path, meta: &mut Meta, hives: &[&str]) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let trash = crate::state::dir(prefix).join("trash").join(&meta.id);
    fs::create_dir_all(&trash)?;
    let mut n = 0usize;
    for e in &meta.entries {
        let cur = prefix.join(&e.rel);
        let snap = dir.join("files").join(stored(&e.rel));
        match e.kind.as_str() {
            "prefs" => {
                // Only when the failed update removed it: never over newer settings.
                if e.existed && fs::symlink_metadata(&cur).is_err() && fs::symlink_metadata(&snap).is_ok() {
                    if let Some(p) = cur.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&snap, &cur).with_context(|| format!("restoring {}", e.rel))?;
                    notes.push(format!("restored the preference folder {}", e.rel));
                }
            }
            _ => {
                if fs::symlink_metadata(&cur).is_ok() {
                    n += 1;
                    let t = trash.join(format!("{n}"));
                    fs::rename(&cur, &t).with_context(|| format!("moving aside {}", e.rel))?;
                }
                if e.existed {
                    if let Some(p) = cur.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&snap, &cur).with_context(|| format!("restoring {}", e.rel))?;
                }
            }
        }
    }
    for h in hives {
        let s = dir.join("hives").join(h);
        if !s.is_file() {
            continue;
        }
        if *h == "user.reg" {
            // Sign-in is never rolled back: keep the Credential Manager sections that are in the
            // prefix now (an account signed in after the snapshot stays signed in).
            let snap = fs::read(&s).with_context(|| format!("reading the saved {h}"))?;
            let cur = fs::read(prefix.join(h)).unwrap_or_default();
            let merged = keep_current_sections(&snap, &cur, CREDENTIAL_MANAGER);
            fs::write(prefix.join(h), merged).with_context(|| format!("restoring {h}"))?;
        } else {
            fs::copy(&s, prefix.join(h)).with_context(|| format!("restoring {h}"))?;
        }
    }
    let _ = fs::remove_dir_all(&trash);
    meta.state = "rolled-back".into();
    write_meta(dir, meta)?;
    Ok(notes)
}

/// Whether `id` may be rolled back now: no later snapshot of this prefix is still in place.
pub fn check_order(prefix: &Path, meta: &Meta) -> Result<()> {
    for (_, m) in list(prefix) {
        if m.created_secs > meta.created_secs && (m.state == "committed" || m.state == "pre-update") {
            bail!(crate::failure::Failure::new(
                "rollback-order",
                crate::failure::EX_FAIL,
                format!(
                    "{} was updated after this ({} {}). Roll that update back first, then this one.",
                    crate::catalog::find(&m.app).map(|a| a.name).unwrap_or(&m.app),
                    m.from.clone().unwrap_or_default(),
                    m.to.clone().map(|t| format!("-> {t}")).unwrap_or_default()
                ),
            )
            .with("prefix_changed", false));
        }
    }
    Ok(())
}

/// Delete snapshots: of `app`'s older committed ones (after a successful update), or all
/// committed/rolled-back ones beyond the newest `keep` per app (`snapshots --prune`). A
/// "pre-update" snapshot (an update was cut off) is never pruned. Returns what was removed.
pub fn prune(prefix: &Path, app: Option<&str>, keep: usize, except: Option<&str>) -> Vec<String> {
    let mut removed = Vec::new();
    let all = list(prefix);
    let mut apps: Vec<String> = all.iter().map(|(_, m)| m.app.clone()).collect();
    apps.sort();
    apps.dedup();
    for a in apps.iter().filter(|a| app.map(|x| x == a.as_str()).unwrap_or(true)) {
        let mine: Vec<&(PathBuf, Meta)> = all.iter().filter(|(_, m)| &m.app == a && m.state != "pre-update" && m.state != "creating").collect();
        let n = mine.len();
        for (d, m) in mine.into_iter().take(n.saturating_sub(keep)) {
            if Some(m.id.as_str()) == except {
                continue;
            }
            if fs::remove_dir_all(d).is_ok() {
                removed.push(m.id.clone());
            }
        }
    }
    removed
}

/// Is `dir` (an entry) unchanged since the snapshot? Used by tests.
#[cfg(test)]
fn same_inode(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.ino() == y.ino() && x.dev() == y.dev(),
        _ => false,
    }
}

/// The records an update of `sap`/`base` should keep: the product and its recorded
/// dependencies (by `product_reference_info` is not read; the caller names the deps).
pub fn records<'a>(db: Option<&'a Db>, sap: &str, base: &str) -> Option<&'a Product> {
    db?.product(sap, base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_resolve_into_the_prefix_and_take_our_user() {
        let u = "steamuser";
        assert_eq!(resolve("[AdobeCommon]\\Adobe Photoshop 2026\\AddOnModules", None, u).unwrap(), "drive_c/Program Files/Common Files/Adobe/Adobe Photoshop 2026/AddOnModules");
        assert_eq!(resolve("[INSTALLDIR]\\Locales\\en_US", Some("C:\\Program Files\\Adobe\\Adobe Photoshop 2026"), u).unwrap(), "drive_c/Program Files/Adobe/Adobe Photoshop 2026/Locales/en_US");
        assert_eq!(resolve("[InstallDir]\\Support Files", Some("C:\\Program Files\\Adobe\\Adobe Illustrator 2026"), u).unwrap(), "drive_c/Program Files/Adobe/Adobe Illustrator 2026/Support Files");
        assert_eq!(resolve("[UserRoamingAppData]\\Adobe\\Premiere Pro\\26.0", None, u).unwrap(), "drive_c/users/steamuser/AppData/Roaming/Adobe/Premiere Pro/26.0");
        // A Windows path recorded on another machine names that machine's user.
        assert_eq!(resolve("C:\\users\\nuck\\AppData\\Roaming\\Adobe\\x.log", None, u).unwrap(), "drive_c/users/steamuser/AppData/Roaming/Adobe/x.log");
        assert_eq!(resolve("[SharedDocuments]\\Adobe\\Premiere Pro\\26.0\\Tutorial", None, u).unwrap(), "drive_c/users/Public/Documents/Adobe/Premiere Pro/26.0/Tutorial");
        assert!(resolve("[SomethingNew]\\x", None, u).is_none());
        assert!(resolve("D:\\Adobe\\x", None, u).is_none());
    }

    fn fixture() -> PathBuf {
        let p = std::env::temp_dir().join(format!("mudhut-snap-{}-{}", std::process::id(), rand_suffix()));
        let _ = fs::remove_dir_all(&p);
        let app = p.join("drive_c/Program Files/Adobe/Adobe Audition 2026");
        fs::create_dir_all(app.join("Plug-ins")).unwrap();
        fs::write(app.join("Adobe Audition.exe"), b"old exe").unwrap();
        fs::write(app.join("Plug-ins/third-party.aex"), b"user plugin").unwrap();
        fs::create_dir_all(p.join("drive_c/Program Files (x86)/Common Files/Adobe/caps")).unwrap();
        fs::write(p.join("drive_c/Program Files (x86)/Common Files/Adobe/caps/hdpim.db"), b"old db").unwrap();
        fs::create_dir_all(p.join("drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0")).unwrap();
        fs::write(p.join("drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0/prefs.txt"), b"prefs").unwrap();
        fs::write(p.join("system.reg"), b"old system").unwrap();
        fs::write(p.join("user.reg"), b"old user").unwrap();
        p
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn snapshot_then_failed_update_then_restore() {
        let p = fixture();
        let app_rel = "drive_c/Program Files/Adobe/Adobe Audition 2026";
        let mut entries = scope(&p, app_rel, None, &[]);
        entries.push(Entry { rel: "drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0".into(), kind: "prefs".into(), existed: false, files: 0, bytes: 0 });
        entries.push(Entry { rel: "drive_c/Program Files/Common Files/Adobe/Startup Scripts CC/au.dat".into(), kind: "file".into(), existed: false, files: 0, bytes: 0 });
        let (dir, mut meta) = create(&p, "audition", "AUDT", Some("26.3.0.79"), Some("26.5.0.82"), entries).unwrap();
        assert_eq!(meta.state, "pre-update");
        assert!(meta.entries.iter().find(|e| e.rel == app_rel).unwrap().existed);
        // The "update" rewrites the exe, adds a file, deletes the prefs, writes a shared file.
        let app = p.join(app_rel);
        fs::write(app.join("Adobe Audition.exe"), b"NEW exe").unwrap();
        fs::write(app.join("new-in-26.5.dll"), b"x").unwrap();
        fs::write(p.join("drive_c/Program Files (x86)/Common Files/Adobe/caps/hdpim.db"), b"new db").unwrap();
        fs::remove_dir_all(p.join("drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0")).unwrap();
        fs::create_dir_all(p.join("drive_c/Program Files/Common Files/Adobe/Startup Scripts CC")).unwrap();
        fs::write(p.join("drive_c/Program Files/Common Files/Adobe/Startup Scripts CC/au.dat"), b"new").unwrap();
        fs::write(p.join("system.reg"), b"new system").unwrap();
        fs::write(p.join("user.reg"), b"new user").unwrap();
        let notes = restore(&p, &dir, &mut meta, HIVES).unwrap();
        assert_eq!(fs::read(app.join("Adobe Audition.exe")).unwrap(), b"old exe");
        assert!(!app.join("new-in-26.5.dll").exists(), "files the update added are gone");
        assert_eq!(fs::read(app.join("Plug-ins/third-party.aex")).unwrap(), b"user plugin");
        assert_eq!(fs::read(p.join("drive_c/Program Files (x86)/Common Files/Adobe/caps/hdpim.db")).unwrap(), b"old db");
        assert_eq!(fs::read(p.join("drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0/prefs.txt")).unwrap(), b"prefs");
        assert!(!p.join("drive_c/Program Files/Common Files/Adobe/Startup Scripts CC/au.dat").exists(), "a shared file the update created is removed");
        assert_eq!(fs::read(p.join("system.reg")).unwrap(), b"old system");
        assert_eq!(fs::read(p.join("user.reg")).unwrap(), b"old user");
        assert!(notes.iter().any(|n| n.contains("preference folder")));
        assert_eq!(meta.state, "rolled-back");
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn manual_rollback_keeps_the_users_registry_and_newer_prefs() {
        let p = fixture();
        let prefs = "drive_c/users/me/AppData/Roaming/Adobe/Audition/26.0";
        let entries = vec![
            Entry { rel: "drive_c/Program Files/Adobe/Adobe Audition 2026".into(), kind: "tree".into(), existed: false, files: 0, bytes: 0 },
            Entry { rel: prefs.into(), kind: "prefs".into(), existed: false, files: 0, bytes: 0 },
        ];
        let (dir, mut meta) = create(&p, "audition", "AUDT", Some("a"), Some("b"), entries).unwrap();
        fs::write(p.join(prefs).join("prefs.txt"), b"changed after the update").unwrap();
        fs::write(p.join("user.reg"), b"user settings since").unwrap();
        restore(&p, &dir, &mut meta, &["system.reg", "userdef.reg"]).unwrap();
        assert_eq!(fs::read(p.join(prefs).join("prefs.txt")).unwrap(), b"changed after the update");
        assert_eq!(fs::read(p.join("user.reg")).unwrap(), b"user settings since");
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn hive_restore_keeps_the_current_sign_in() {
        let saved = b"WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1\n\n#arch=win64\n\n\
[Software\\\\Adobe\\\\Photoshop] 1\n\"Pref\"=\"old\"\n\n\
[Software\\\\Wine\\\\Credential Manager] 1\n\"EncryptionKey\"=hex:01\n\n";
        let current = b"WINE REGISTRY Version 2\n\n\
[Software\\\\Adobe\\\\Photoshop] 2\n\"Pref\"=\"new\"\n\n\
[Software\\\\Wine\\\\Credential Manager] 2\n\"EncryptionKey\"=hex:01\n\n\
[Software\\\\Wine\\\\Credential Manager\\\\Generic: Adobe User Info(Part1)] 2\n\"Blob\"=hex:aa\n\n";
        let m = super::keep_current_sections(saved, current, super::CREDENTIAL_MANAGER);
        let t = String::from_utf8(m).unwrap();
        // the saved app preference comes back...
        assert!(t.contains("\"Pref\"=\"old\""), "{t}");
        assert!(!t.contains("\"Pref\"=\"new\""), "{t}");
        // ...but the sign-in is the current one, and is there exactly once
        assert!(t.contains("Generic: Adobe User Info(Part1)] 2"), "{t}");
        assert_eq!(t.matches("[Software\\\\Wine\\\\Credential Manager] ").count(), 1, "{t}");
        assert!(t.starts_with("WINE REGISTRY Version 2\n;; All keys"), "{t}");
    }

    #[test]
    fn rollback_is_newest_first_and_prune_keeps_pre_update() {
        let p = fixture();
        let e = || vec![Entry { rel: "drive_c/Program Files/Adobe/Adobe Audition 2026".into(), kind: "tree".into(), existed: false, files: 0, bytes: 0 }];
        let (d1, mut m1) = create(&p, "audition", "AUDT", Some("1"), Some("2"), e()).unwrap();
        m1.state = "committed".into();
        write_meta(&d1, &m1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let (d2, mut m2) = create(&p, "photoshop", "PHSP", Some("3"), Some("4"), e()).unwrap();
        m2.state = "committed".into();
        write_meta(&d2, &m2).unwrap();
        let err = check_order(&p, &m1).unwrap_err();
        assert!(format!("{err:#}").contains("Photoshop was updated after this"), "{err:#}");
        assert!(check_order(&p, &m2).is_ok());
        let (_d3, _m3) = create(&p, "audition", "AUDT", Some("2"), Some("5"), e()).unwrap(); // pre-update
        let removed = prune(&p, None, 0, None);
        assert_eq!(removed.len(), 2, "both committed pruned: {removed:?}");
        assert_eq!(list(&p).len(), 1, "the pre-update one stays");
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn scope_skips_shared_roots_and_nests() {
        let p = fixture();
        let mut prod = crate::hdpimdb::Product {
            sap: "PHSP".into(), codex: "27.8".into(), family: "64Bit".into(), status: 1,
            meta: Default::default(), packages: Default::default(), package_meta: Default::default(),
        };
        prod.meta.insert("InstallDir".into(), "C:\\Program Files\\Adobe\\Adobe Photoshop 2026".into());
        let mut core = std::collections::BTreeMap::new();
        core.insert("TargetFolderList".into(), "[INSTALLDIR],[INSTALLDIR]\\Locales\\en_US,[AdobeCommon]\\Adobe Photoshop 2026\\AddOnModules".to_string());
        prod.package_meta.insert("Core".into(), core);
        let mut support = std::collections::BTreeMap::new();
        support.insert("TargetFolderList".into(), "[AdobeCommon]".to_string());
        prod.package_meta.insert("Support".into(), support);
        let s = scope(&p, "drive_c/Program Files/Adobe/Adobe Photoshop 2026", Some(&prod), &[]);
        let trees: Vec<&str> = s.iter().filter(|e| e.kind == "tree").map(|e| e.rel.as_str()).collect();
        assert!(trees.contains(&"drive_c/Program Files/Common Files/Adobe/Adobe Photoshop 2026/AddOnModules"));
        assert!(!trees.contains(&"drive_c/Program Files/Common Files/Adobe"), "a shared root is never a tree: {trees:?}");
        assert!(!trees.iter().any(|t| t.ends_with("Locales/en_US")), "nested in the app folder: {trees:?}");
        let _ = fs::remove_dir_all(&p);
        let _ = same_inode(&p, &p);
    }
}
