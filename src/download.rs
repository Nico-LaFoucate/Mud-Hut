// SPDX-License-Identifier: Apache-2.0
//! `--method download`: fetch an app's packages straight from Adobe.
//!
//! The resolve half (feed -> buildGuid -> manifest -> [`DownloadPlan`]) lives in
//! [`crate::feed`]. This module is the fetch half: download each planned package
//! from the CDN into a destination tree (preserving Adobe's `Path` layout so
//! HDPIM finds them), streaming to disk while verifying integrity.
//!
//! **Verification.** Adobe's `packageHashKey` is NOT the hash of the downloaded
//! bytes (it's identical across the TYPE1/TYPE2 algorithms — a decrypted-content
//! identity). The real download-integrity check is the package's **ValidationURL**
//! (`?algorithm=TYPE2`): a per-`segmentSize` (2 MiB) list of SHA-256 segment
//! hashes. We verify each segment as it streams. Idempotent: a file already
//! present at the expected size is trusted and skipped.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use md5::Md5;
use sha2::digest::DynDigest;
use sha2::{Digest, Sha256};

use crate::feed::DownloadPlan;
use crate::ledger::Ledger;
use crate::output::Emitter;

/// `install --method download`: install a genuine app into `prefix` via the HDPIM
/// offline engine (decrypt, no Set-up.exe/WAM/CC-desktop). Without `source`, downloads
/// the product and its dependency components (resolved from the product feed) into the
/// download cache; with `source`, uses the packages already staged there and downloads only
/// what is missing. Then writes the product's DriverInfo beside the packages, runs the engine
/// and provisions.
#[allow(clippy::too_many_arguments)]
pub fn install(
    em: &Emitter,
    app: Option<&str>,
    source: Option<&Path>,
    prefix: &Path,
    dry_run: bool,
    minimal: bool,
    keep_download: bool,
    opts: &crate::feed::PlanOpts,
) -> Result<()> {
    let app_id = app.context("`--method download` needs an app id (e.g. `photoshop`)")?;
    let cat = crate::catalog::find(app_id)
        .with_context(|| format!("unknown app '{app_id}' (see `mudhut apps`)"))?;

    // Resolve the product + its downloadable dependencies (lightweight: no payloads).
    let ledger = Ledger::load(em)?;
    let build = crate::feed::resolve_build_version(em, &ledger, app_id, opts.major, opts.allow_unverified)?;
    let (manifest, raw_manifest) = crate::feed::fetch_manifest(em, &ledger, &build)?;
    let mut plan = crate::feed::plan_with(&build, &manifest, "en_US", opts);
    let dep_builds = crate::feed::resolve_dependencies(em, &ledger, &plan.dependencies, minimal)?;

    // The DriverInfo's dependency list must match what's actually staged: keep only
    // the deps we resolved + will download (drops the deferred/unresolvable ones).
    let staged: std::collections::HashSet<String> =
        dep_builds.iter().map(|b| b.sap.clone()).collect();
    plan.dependencies.retain(|d| staged.contains(&d.sap));

    // Staging: use a caller-supplied `--source` as-is, else download to a cache dir.
    let products = match source {
        Some(s) => {
            if !s.is_dir() {
                bail!("--source is not a directory: {}", s.display());
            }
            s.to_path_buf()
        }
        None => cache_products_dir(&plan)?,
    };

    em.note(&format!(
        "install {} {} ({}) + {} dep(s) -> {}",
        cat.name,
        plan.product_version,
        plan.sap,
        plan.dependencies.len(),
        products.display()
    ));

    // Already installed: re-running a finished install downloaded gigabytes and then failed in
    // Adobe's installer (HDPIM 130, "already installed").
    if already_installed(prefix, &plan) {
        em.note(&format!("{} {} is already installed in {}", cat.name, plan.product_version, prefix.display()));
        if em.is_json() {
            em.result(&serde_json::json!({
                "ok": true, "already_installed": true, "method": "download", "app": cat.id,
                "product_version": plan.product_version, "prefix": prefix.display().to_string(),
            }));
        }
        return Ok(());
    }

    if dry_run {
        let dep_saps: Vec<&str> = plan.dependencies.iter().map(|d| d.sap.as_str()).collect();
        em.note(&format!(
            "(dry run) would download {} package(s) ({:.2} GiB) + deps [{}], then HDPIM-install {} into {}",
            plan.packages.len(),
            plan.total_bytes as f64 / (1u64 << 30) as f64,
            dep_saps.join(", "),
            cat.name,
            prefix.display()
        ));
        if em.is_json() {
            em.result(&serde_json::json!({
                "ok": true, "dry_run": true, "method": "download", "app": cat.id,
                "product_version": plan.product_version, "base_version": plan.base_version,
                "verified": build.verified, "newest": build.newest,
                "label": crate::version::label(&plan.product_version),
                "newest_label": build.newest.as_deref().map(crate::version::label),
                "packages": plan.packages.len(), "download_bytes": plan.total_bytes,
                "modules": plan.modules, "dependencies": dep_saps,
                "prefix": prefix.display().to_string(),
            }));
        }
        return Ok(());
    }

    // Hold the prefix for the whole install: `neutron launch` refuses and `neutron update`
    // skips it meanwhile, and a second Mud Hut run waits its turn.
    let _lock = crate::state::lock(prefix, "install")?;
    // Refuse before downloading gigabytes into a prefix that is in use (the engine checks again
    // right before it starts).
    crate::hdpim::refuse_if_busy(prefix)?;
    check_space(prefix, source.is_none().then_some(products.as_path()), &plan)?;

    let roots = stage(em, &ledger, &plan, &raw_manifest, &dep_builds, &products, source.is_some())?;

    // Write the DriverInfo beside the packages (EsdDirectory is relative to it), or with
    // absolute paths when a component had to be staged elsewhere.
    let driver_xml = crate::driver::write_driver_xml_roots(&plan, &products, &roots, &crate::driver::Request::default())?;

    // Shared HDPIM tail (same engine `offline` uses).
    hdpim_install_and_provision_opts(em, &cat, &plan, &driver_xml, &products, prefix, "download", dry_run, opts.remove_previous)?;

    // The downloaded packages are tens of GB and are never read again once installed.
    // Only OUR cache is removed — never a caller-supplied --source.
    if !keep_download {
        let mut ours: Vec<PathBuf> = roots.values().filter(|r| source.is_none() || !r.starts_with(&products)).cloned().collect();
        ours.sort();
        ours.dedup();
        for r in ours {
            if let Some(cache_dir) = r.parent() {
                match fs::remove_dir_all(cache_dir) {
                    Ok(()) => em.note(&format!("removed the download cache {}", cache_dir.display())),
                    Err(e) => em.note(&format!("could not remove {}: {e}", cache_dir.display())),
                }
            }
        }
    }
    Ok(())
}

/// Make every component of `plan` present on disk and return where each one is (SAP -> the
/// directory holding `<SAP>/`).
///
/// Without a staged source everything goes to `products` (our cache). With `--source`, what is
/// already there is used, and a component that is missing or incomplete is downloaded: into the
/// source when it is writable, else beside it in the cache.
///
/// 🚨 `install --source` used to resolve the dependencies from the feed, name them all in the
/// DriverInfo, and download none of them; a source made by `mudhut download` has no dependency
/// folders, so HDPIM was told about components that were not there (lab, 2026-10-16).
fn stage(
    em: &Emitter,
    ledger: &Ledger,
    plan: &DownloadPlan,
    raw_manifest: &str,
    dep_builds: &[crate::feed::Build],
    products: &Path,
    from_source: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut roots = BTreeMap::new();
    let writable = !from_source || is_writable(products);
    let fallback = || -> Result<PathBuf> { cache_products_dir(plan) };

    // The product itself.
    let prod_root = if !from_source || writable { products.to_path_buf() } else { fallback()? };
    let staged_already = from_source && products.join(&plan.sap).join("Application.json").is_file()
        && missing_payloads(plan, products).is_empty();
    if staged_already {
        roots.insert(plan.sap.clone(), products.to_path_buf());
    } else {
        if from_source {
            em.note(&format!("{} is not complete in --source; downloading it into {}", plan.sap, prod_root.display()));
        }
        fs::create_dir_all(&prod_root).with_context(|| format!("creating {}", prod_root.display()))?;
        write_application_json(&prod_root, &plan.sap, raw_manifest)?;
        fetch_packages(em, ledger, plan, &prod_root)?;
        roots.insert(plan.sap.clone(), prod_root);
    }

    // Each dependency component.
    for db in dep_builds {
        let (manifest, raw) = crate::feed::fetch_manifest(em, ledger, db)?;
        let dplan = crate::feed::plan(db, &manifest, "en_US");
        if from_source && products.join(&db.sap).join("Application.json").is_file() && missing_payloads(&dplan, products).is_empty() {
            roots.insert(db.sap.clone(), products.to_path_buf());
            continue;
        }
        let target = if writable { products.to_path_buf() } else { fallback()? };
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
        write_application_json(&target, &db.sap, &raw)?;
        fetch_packages(em, ledger, &dplan, &target)?;
        roots.insert(db.sap.clone(), target);
    }
    Ok(roots)
}

/// The planned payload files NOT present (at their expected size) under `<products>/<SAP>/`.
pub(crate) fn missing_payloads(plan: &DownloadPlan, products: &Path) -> Vec<String> {
    plan.packages
        .iter()
        .filter_map(|p| {
            let file = p.path.rsplit('/').next().unwrap_or(p.path.as_str());
            let path = products.join(&p.sap).join(file);
            match fs::metadata(&path) {
                Ok(m) if m.len() == p.bytes || p.bytes == 0 => None,
                _ => Some(format!("{}/{}", p.sap, file)),
            }
        })
        .collect()
}

/// Whether `dir` can be written to (probe file, removed at once).
pub(crate) fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".mudhut-write-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The shared install tail for the HDPIM-engine methods (`download` + `offline`):
/// discover + run the decrypt-install engine, seed the app's UI fonts, provision
/// the prefix, install the menu launcher, and emit the terminal `result` event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hdpim_install_and_provision(
    em: &Emitter,
    cat: &crate::catalog::App,
    plan: &DownloadPlan,
    driver_xml: &Path,
    products: &Path,
    prefix: &Path,
    method: &str,
    dry_run: bool,
) -> Result<()> {
    hdpim_install_and_provision_opts(em, cat, plan, driver_xml, products, prefix, method, dry_run, false)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn hdpim_install_and_provision_opts(
    em: &Emitter,
    cat: &crate::catalog::App,
    plan: &DownloadPlan,
    driver_xml: &Path,
    products: &Path,
    prefix: &Path,
    method: &str,
    dry_run: bool,
    remove_previous: bool,
) -> Result<()> {
    let cfg = crate::hdpim::discover(&repo_tools_dir()?, accc_packages_dir(em)?, Some(prefix))?;
    let expect = crate::hdpim::Expect {
        sap: plan.sap.clone(),
        base: plan.base_version.clone(),
        build: Some(plan.product_version.clone()),
    };
    let exe = crate::hdpim::install(em, &cfg, prefix, cat, &expect, driver_xml, products, dry_run)?;

    if dry_run {
        return Ok(());
    }
    let removed = if remove_previous { remove_previous_majors(em, &cfg, prefix, cat, plan)? } else { vec![] };
    // Seed the app's shipped UI fonts (Adobe Clean) into windows/Fonts BEFORE the
    // provision below registers them. HDPIM leaves these only under the app's
    // Resources/ui-fonts; without this the provisioned Segoe UI->Adobe Clean font
    // replacement resolves to an unregistered family -> blank menu bar.
    seed_ui_fonts(em, &exe, prefix);
    em.progress("provision", 100, "neutron prefix provision");
    // Launchers, icons and MIME defaults are provision's (see install.rs for why Mud Hut no
    // longer writes its own `mudhut-<app>.desktop` after this call).
    crate::install::provision(prefix)?;
    // The note BEFORE the terminal result: the result must be the last line of the stream.
    em.note(&format!("installed: {}", exe.display()));
    if em.is_json() {
        em.result(&serde_json::json!({
            "ok": true,
            "method": method,
            "app": cat.id,
            "version": plan.product_version,
            "prefix": prefix.display().to_string(),
            "exe": exe.display().to_string(),
            "provisioned": true,
            "removed_previous": removed,
        }));
    }
    Ok(())
}

/// After a new major is installed: remove the older majors of the same app through HDPIM's own
/// uninstall (`hdpimUnInstallProduct`, `DeleteUserPreferences=false`), the way Creative Cloud's
/// "Remove previous version" does. Only versions Adobe's record lists can be removed this way; a
/// copied older version without a record is left in place and named in the result.
fn remove_previous_majors(
    em: &Emitter,
    cfg: &crate::hdpim::Config,
    prefix: &Path,
    cat: &crate::catalog::App,
    plan: &DownloadPlan,
) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    let name = cat.name.trim_end_matches(" (experimental)");
    let new_major = crate::version::major(&plan.product_version);
    let db = crate::hdpimdb::Db::read(prefix)?.unwrap_or(crate::hdpimdb::Db { products: vec![] });
    let olds: Vec<crate::hdpimdb::Product> = db
        .products
        .iter()
        .filter(|p| p.sap == plan.sap && p.base().map(|b| !crate::version::eq(b, &plan.base_version)).unwrap_or(false))
        .filter(|p| p.build().map(|b| crate::version::major(b) < new_major).unwrap_or(false))
        .cloned()
        .collect();
    for old in olds {
        let (codex, base) = (old.codex.clone(), old.base().unwrap_or_default().to_string());
        let platform = old.meta.get("Platform").cloned().unwrap_or_else(|| "win64".into());
        em.progress("remove-previous", 50, &format!("removing {name} {codex} (your settings stay)"));
        let dir = std::env::temp_dir().join(format!("mudhut-uninstall-{}-{}", std::process::id(), codex));
        let xml = crate::driver::write_uninstall_xml(&dir, &plan.name, &plan.sap, &codex, &base, &platform)?;
        let expect = crate::hdpim::Expect { sap: plan.sap.clone(), base: base.clone(), build: None };
        let r = crate::hdpim::run_hdpim(em, cfg, prefix, crate::hdpim::Op::Uninstall, &xml, &dir, &expect, cat);
        let _ = fs::remove_dir_all(&dir);
        match r {
            Ok(_) => out.push(serde_json::json!({"version": old.build(), "removed": true})),
            Err(e) => {
                em.note(&format!("could not remove {name} {codex}: {e:#}"));
                out.push(serde_json::json!({"version": old.build(), "removed": false, "message": format!("{e:#}")}));
            }
        }
    }
    // Older majors without Adobe's record (copied from Windows) cannot be removed by Adobe's
    // uninstaller; Mud Hut does not delete folders it cannot account for.
    if let Ok(inv) = crate::inventory::inventory(&prefix.display().to_string()) {
        for a in inv.apps.iter().filter(|a| a.id == cat.id && a.managed_by == crate::inventory::Managed::AdoptNeeded) {
            if a.version.as_deref().map(|v| crate::version::major(v) < new_major).unwrap_or(false) {
                out.push(serde_json::json!({"version": a.version, "removed": false, "dir": a.dir,
                    "message": "This older version was copied without Adobe's records, so Adobe's uninstaller cannot remove it. It was left in place."}));
            }
        }
    }
    Ok(out)
}

/// Stage `<products>/<SAP>/Application.json` (the raw manifest) — HDPIM's ESD layout.
pub(crate) fn write_application_json(products: &Path, sap: &str, raw: &str) -> Result<()> {
    let dir = products.join(sap);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let p = dir.join("Application.json");
    fs::write(&p, raw).with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}

/// Default download-cache products dir: `~/.cache/mudhut/<SAP>-<ver>-<plat>/products`.
/// Adobe's own record (`hdpim.db`) has exactly this build of the app, and its program is on disk.
fn already_installed(prefix: &Path, plan: &DownloadPlan) -> bool {
    let Ok(Some(db)) = crate::hdpimdb::Db::read(prefix) else { return false };
    let Some(p) = db.product(&plan.sap, &plan.base_version) else { return false };
    if !p.build().is_some_and(|b| crate::version::eq(b, &plan.product_version)) {
        return false;
    }
    p.app_launch().and_then(|w| crate::hdpimdb::win_to_prefix(prefix, &w)).is_some_and(|f| f.is_file())
}

/// The nearest existing directory at or above `p` (the download cache may not exist yet).
fn existing(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    while !cur.exists() {
        match cur.parent() {
            Some(up) => cur = up.to_path_buf(),
            None => break,
        }
    }
    cur
}

/// Free space for an install, before anything is fetched (see `require_space`).
fn check_space(prefix: &Path, cache: Option<&Path>, plan: &DownloadPlan) -> Result<()> {
    let unpacked: u64 = plan.packages.iter().map(|x| if x.extract_bytes > 0 { x.extract_bytes } else { x.bytes * 2 }).sum();
    require_space(prefix, cache, plan.total_bytes, unpacked, &format!("installing {}", plan.name))
}

/// The packages land in the download cache (`cache`; None for a caller-supplied `--source`, which
/// needs no download) and the unpacked app in the prefix, which can be two filesystems. Each must
/// hold its share plus a 2 GB margin (the shared components, a few hundred MB, are not sized until
/// staging), or one sum when they share a filesystem. `what` names the job in the message.
pub(crate) fn require_space(prefix: &Path, cache: Option<&Path>, download: u64, unpacked: u64, what: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    const MARGIN: u64 = 2 << 30;
    let at_prefix = existing(prefix);
    let mut needs: Vec<(PathBuf, u64)> = vec![(at_prefix.clone(), unpacked + MARGIN)];
    if let Some(c) = cache {
        let at_cache = existing(c);
        let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
        if dev(&at_cache).is_some() && dev(&at_cache) == dev(&at_prefix) {
            needs[0].1 += download;
        } else {
            needs.push((at_cache, download + MARGIN));
        }
    }
    for (path, need) in needs {
        if let Some(free) = crate::snapshot::free_bytes(&path) {
            if free < need {
                return Err(crate::failure::Failure::new(
                    "disk-space",
                    crate::failure::EX_DISK,
                    format!(
                        "Not enough free disk space: {what} needs about {:.1} GB on {}, and {:.1} GB is free. Nothing was changed.",
                        need as f64 / 1e9, path.display(), free as f64 / 1e9
                    ),
                )
                .with("prefix_changed", false)
                .into());
            }
        }
    }
    Ok(())
}

/// Where Mud Hut keeps downloads (`$XDG_CACHE_HOME/mudhut`).
pub(crate) fn cache_root() -> Option<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache").ok().map(|b| b.join("mudhut"))
}

pub(crate) fn cache_products_dir(plan: &DownloadPlan) -> Result<PathBuf> {
    let base = xdg_dir("XDG_CACHE_HOME", ".cache").context("no HOME/XDG_CACHE_HOME for the download cache")?;
    Ok(base
        .join("mudhut")
        .join(format!("{}-{}-{}", plan.sap, plan.product_version, plan.platform))
        .join("products"))
}

/// Seed the just-installed app's shipped UI fonts (e.g. Adobe Clean, under
/// `<install dir>/Resources/ui-fonts`) into the prefix's `windows/Fonts`, so the
/// subsequent `neutron prefix provision` registers them (neutron's
/// `register_prefix_fonts` only sees what is already in `windows/Fonts`). HDPIM
/// installs these fonts only under the app's Resources dir; leaving them there
/// makes the provisioned Segoe UI->Adobe Clean replacement resolve to an
/// unregistered family -> blank menu bar. Best-effort: a copy failure never fails
/// the install.
pub(crate) fn seed_ui_fonts(em: &Emitter, exe: &Path, prefix: &Path) {
    let Some(src) = exe.parent().map(|d| d.join("Resources").join("ui-fonts")) else {
        return;
    };
    if !src.is_dir() {
        return;
    }
    let dst = prefix.join("drive_c").join("windows").join("Fonts");
    if let Err(e) = fs::create_dir_all(&dst) {
        em.note(&format!("ui-fonts: could not create windows/Fonts: {e}"));
        return;
    }
    let mut n = 0u32;
    if let Ok(rd) = fs::read_dir(&src) {
        for ent in rd.flatten() {
            let p = ent.path();
            let is_font = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "otf" | "ttf" | "ttc"))
                .unwrap_or(false);
            if !is_font {
                continue;
            }
            let target = dst.join(ent.file_name());
            // Copy when absent OR different: an update can ship a newer Adobe Clean, and the
            // old "only when absent" rule kept the first one forever.
            let same = match (fs::metadata(&p), fs::metadata(&target)) {
                (Ok(a), Ok(b)) => a.len() == b.len(),
                _ => false,
            };
            if !same && fs::copy(&p, &target).is_ok() {
                n += 1;
            }
        }
    }
    if n > 0 {
        em.note(&format!("seeded {n} UI font(s) into windows/Fonts (Adobe Clean)"));
    }
}

/// Where the shipped `hdpim_host.exe`, `extract_accc_runtime.py` and `register_accc_pim.py` live.
/// `$MUDHUT_TOOLS`, else `<exe dir>/tools` (shipped layout), else the cargo
/// dev-tree `<repo>/tools` (binary at `<repo>/target/{debug,release}/mudhut`),
/// else `./tools`. The dev-tree probe is what lets Collider — which runs the
/// symlinked debug binary from its own CWD — find the tools.
pub(crate) fn repo_tools_dir() -> Result<PathBuf> {
    if let Ok(t) = std::env::var("MUDHUT_TOOLS") {
        return Ok(PathBuf::from(t));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Shipped: tools next to the binary.
            let shipped = dir.join("tools");
            if shipped.is_dir() {
                return Ok(shipped);
            }
            // Cargo dev-tree: target/{debug,release}/mudhut -> <repo>/tools.
            let dev = dir.join("../../tools");
            if dev.join("hdpim_host.exe").is_file() {
                return Ok(dev.canonicalize().unwrap_or(dev));
            }
        }
    }
    embedded_tools_dir()
}

/// The runtime tools, compiled into the binary so a release is one file (what
/// `neutron setup` downloads). Written out on first use; rewritten when they differ from the
/// copy this binary carries.
const EMBEDDED_TOOLS: &[(&str, &[u8])] = &[
    ("hdpim_host.exe", include_bytes!("../tools/hdpim_host.exe")),
    ("extract_accc_runtime.py", include_bytes!("../tools/extract_accc_runtime.py")),
    ("register_accc_pim.py", include_bytes!("../tools/register_accc_pim.py")),
];

fn embedded_tools_dir() -> Result<PathBuf> {
    let dir = xdg_dir("XDG_DATA_HOME", ".local/share")?.join("neutron/mudhut-tools");
    fs::create_dir_all(&dir)?;
    for (name, bytes) in EMBEDDED_TOOLS {
        let p = dir.join(name);
        if fs::read(&p).ok().as_deref() != Some(*bytes) {
            fs::write(&p, bytes)?;
        }
    }
    Ok(dir)
}

/// Adobe's public Creative Cloud package (ACCCx) that seeds HDBox/HDPIM into a prefix. Without
/// it NO install can run. Pinned like the runtime: a newer build is adopted only after a
/// clean-room test. Adobe's own download page links these zips; Neutron never redistributes it.
const ACCC_VERSION: &str = "6.5.0.348";
const ACCC_URL: &str = "https://ccmdls.adobe.com/AdobeProducts/StandaloneBuilds/ACCC/ESD/6.5.0/348/win64/ACCCx6_5_0_348.zip";
const ACCC_MD5: &str = "33a015138f2938690267a54e3a21f63e";

pub(crate) fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    xdg_from(std::env::var_os(var), std::env::var_os("HOME"), fallback).with_context(|| format!("no HOME/{var}"))
}

/// `value` when it is an absolute path, else `home/fallback`. The XDG Base Directory spec: unset OR
/// EMPTY means the default, and a relative value is invalid and ignored. An empty XDG_DATA_HOME used
/// to be taken as-is, so `mudhut accc` unpacked Adobe's package into the current directory (found in
/// the 11.10-138 fresh-HOME test, 2026-10-19).
pub(crate) fn xdg_from(value: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>, fallback: &str) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(fallback)))
}

/// The ACCCx `packages/` dir: `$MUDHUT_ACCC_PACKAGES`, else `accc-packages/` beside the binary
/// (a bundled copy), else the copy downloaded from Adobe into
/// `$XDG_DATA_HOME/neutron/accc/<version>/packages` (fetched + md5-verified on first use).
///
/// ⛔ Never a dev-box scratch path: `$HOME/mudhut-parent-stage/packages` used to be the answer
/// here and it exists on exactly one machine (build tester, 2026-09-04).
pub(crate) fn accc_packages_dir(em: &Emitter) -> Result<PathBuf> {
    if let Ok(p) = std::env::var("MUDHUT_ACCC_PACKAGES") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [dir.join("accc-packages"), dir.join("../../accc-packages")] {
                if cand.is_dir() {
                    return Ok(cand.canonicalize().unwrap_or(cand));
                }
            }
        }
    }
    let root = xdg_dir("XDG_DATA_HOME", ".local/share")?.join("neutron/accc").join(ACCC_VERSION);
    let packages = root.join("packages");
    let marker = root.join(".verified");
    if packages.is_dir() && marker.is_file() {
        return Ok(packages);
    }
    fetch_accc(em, &root)?;
    Ok(packages)
}

/// `mudhut accc`: make sure the ACCCx packages are present (downloading + verifying if needed).
pub fn prefetch_accc(em: &Emitter) -> Result<()> {
    let dir = accc_packages_dir(em)?;
    em.note(&format!("Adobe Creative Cloud package {ACCC_VERSION}: {}", dir.display()));
    Ok(())
}

/// One line for `mudhut doctor`: where the ACCCx packages are, or that they'll be downloaded.
pub(crate) fn accc_status() -> String {
    if let Ok(p) = std::env::var("MUDHUT_ACCC_PACKAGES") {
        return format!("MUDHUT_ACCC_PACKAGES={p}");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("accc-packages").is_dir() {
                return format!("bundled beside the binary ({})", dir.join("accc-packages").display());
            }
        }
    }
    match xdg_dir("XDG_DATA_HOME", ".local/share") {
        Ok(d) if d.join("neutron/accc").join(ACCC_VERSION).join(".verified").is_file() =>
            format!("Adobe Creative Cloud package {ACCC_VERSION} ready"),
        _ => format!("Adobe Creative Cloud package {ACCC_VERSION} will be downloaded from Adobe on the first install"),
    }
}

/// Download Adobe's ACCCx zip, verify its MD5 against the pin, and extract `packages/` into
/// `root`. The zip is cached (`$XDG_CACHE_HOME/neutron/`) until the extraction succeeds.
fn fetch_accc(em: &Emitter, root: &Path) -> Result<()> {
    let cache = xdg_dir("XDG_CACHE_HOME", ".cache")?.join("neutron");
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let zip = cache.join(format!("ACCCx{}.zip", ACCC_VERSION.replace('.', "_")));

    let md5_of = |p: &Path| -> Result<String> {
        let mut f = File::open(p).with_context(|| format!("opening {}", p.display()))?;
        let mut h = Md5::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            Digest::update(&mut h, &buf[..n]);
        }
        Ok(format!("{:x}", h.finalize()))
    };

    if !(zip.is_file() && md5_of(&zip)? == ACCC_MD5) {
        em.progress("accc", 1, &format!("downloading Adobe's Creative Cloud package {ACCC_VERSION}"));
        let tmp = zip.with_extension("zip.part");
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(120))
            .build();
        crate::net::retry("Creative Cloud package", |m| em.note(m), || {
            let resp = agent.get(ACCC_URL).call().with_context(|| format!("GET {ACCC_URL}"))?;
            let total: u64 = resp.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            let mut rd = resp.into_reader();
            let mut buf = vec![0u8; 1 << 20];
            let (mut done, mut last) = (0u64, 0u8);
            loop {
                let n = rd.read(&mut buf).context("downloading the Creative Cloud package")?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                done += n as u64;
                if total > 0 {
                    let pct = (done * 100 / total) as u8;
                    if pct >= last + 5 {
                        last = pct;
                        em.progress("accc", pct, &format!("Creative Cloud package {}%", pct));
                    }
                }
            }
            out.flush()?;
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!(
            "could not download Adobe's Creative Cloud package ({e:#}).\n\
             If Adobe has removed version {ACCC_VERSION}, download the Creative Cloud desktop \
             app's direct-link zip from Adobe and point MUDHUT_ACCC_PACKAGES at its \
             extracted packages/ folder."))?;
        let got = md5_of(&tmp)?;
        if got != ACCC_MD5 {
            let _ = fs::remove_file(&tmp);
            bail!("the Creative Cloud package from Adobe failed verification (md5 {got}, expected {ACCC_MD5})");
        }
        fs::rename(&tmp, &zip)?;
    }

    // Extract only packages/ (python3's zipfile: the extractor already needs python3).
    em.progress("accc", 100, "extracting the Creative Cloud package");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let py = "import sys, zipfile\n\
              z = zipfile.ZipFile(sys.argv[1])\n\
              z.extractall(sys.argv[2], [n for n in z.namelist() if n.startswith('packages/')])\n";
    let st = std::process::Command::new("python3")
        .args(["-c", py])
        .arg(&zip)
        .arg(root)
        .status()
        .context("running python3 to extract the Creative Cloud package")?;
    if !st.success() || !root.join("packages/ApplicationInfo.xml").is_file() {
        bail!("extracting {} failed", zip.display());
    }
    fs::write(root.join(".verified"), format!("{ACCC_VERSION} {ACCC_MD5}\n"))?;
    let _ = fs::remove_file(&zip);
    em.note(&format!("Adobe Creative Cloud package {ACCC_VERSION} ready in {}", root.display()));
    Ok(())
}

/// Per-segment validation info from a package's ValidationURL. TYPE2 = SHA-256
/// (product payloads); TYPE1 = MD5 (small shared deps).
struct Validation {
    segment_size: u64,
    segments: Vec<String>, // hex digest, ordered by segmentNumber
    md5: bool,             // TYPE1 (MD5) vs TYPE2 (SHA-256)
}

/// Download every package in `plan` into `dest`, preserving Adobe's `Path` layout,
/// verifying each 2 MiB segment's SHA-256 against the ValidationURL. Idempotent
/// (size-matched files skipped). Emits progress only: callers emit their own terminal
/// `result` (this used to emit one per component, so an install printed several).
pub fn fetch_packages(em: &Emitter, ledger: &Ledger, plan: &DownloadPlan, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let cdn = ledger.endpoints.cdn_base.trim_end_matches('/');
    let agent = ureq::builder()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .build();

    let total = plan.total_bytes;
    let n = plan.packages.len();
    let mut done = 0u64;
    let mut last_pct = u8::MAX;

    for (i, p) in plan.packages.iter().enumerate() {
        // HyperDrive layout: <dest>/<SAP>/<packagefile> (driver.xml EsdDirectory=./SAP).
        let file = p.path.rsplit('/').next().unwrap_or(p.path.as_str());
        let out = dest.join(&p.sap).join(file);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }

        // Idempotent skip: present at the expected size -> trust the prior verify.
        if fs::metadata(&out).map(|m| m.len() == p.bytes).unwrap_or(false) {
            done += p.bytes;
            em.note(&format!("[{}/{n}] present, skip — {}", i + 1, p.name));
            last_pct = emit_pct(em, done, total, &p.name, last_pct);
            continue;
        }

        // The same package may already be on disk under another build's folder: a package
        // Adobe did not change keeps its version, its CDN path and its bytes across builds
        // (Photoshop 27.8 -> 27.11: SuperCafModels 27.5.0.13 is one file at one path in both
        // manifests). HDPIM's update needs every package present, even unchanged ones, so this
        // saves re-downloading gigabytes. Reused only after it passes Adobe's own segment hashes.
        if !p.validation_url.is_empty() {
            if let Some(src) = find_reusable(&p.sap, file, p.bytes, &out) {
                let v = crate::net::retry(&p.name, |m| em.note(m), || fetch_validation(&agent, &ledger.honest_headers(), &p.validation_url));
                if let Ok(v) = v {
                    if verify_file(&src, &v).is_ok() && clone_or_copy(&src, &out).is_ok() {
                        done += p.bytes;
                        em.note(&format!("[{}/{n}] reused (verified) from {} — {}", i + 1, src.display(), p.name));
                        last_pct = emit_pct(em, done, total, &p.name, last_pct);
                        continue;
                    }
                }
            }
        }

        em.note(&format!(
            "[{}/{n}] {} — {:.1} MiB",
            i + 1,
            p.name,
            p.bytes as f64 / (1u64 << 20) as f64
        ));

        // Per-segment SHA-256 list for integrity (empty URL -> size-only trust).
        let validation = if p.validation_url.is_empty() {
            em.note(&format!("  no ValidationURL for {} — verifying by size only", p.name));
            None
        } else {
            Some(
                crate::net::retry(&p.name, |m| em.note(m), || {
                    fetch_validation(&agent, &ledger.honest_headers(), &p.validation_url)
                })
                .with_context(|| format!("fetching validation for {}", p.name))?,
            )
        };

        let url = format!("{cdn}{}", p.path);
        let start = done;
        let res = crate::net::retry(&p.name, |m| em.note(m), || {
            done = start; // download_one counts what it keeps of an earlier try (it resumes)
            download_one(
                // Adobe's CDN serves packages only to its own installer's User-Agent (tested
                // 2026-10-05: "MudHut/0.2" -> 403), so package downloads keep the ledger's.
                em, &agent, &url, &ledger.headers, &out, validation.as_ref(), p.bytes,
                &mut done, total, &mut last_pct,
            )
        });
        if let Err(e) = res {
            // With Adobe's segment hashes the partial file is kept: the next try or run resumes
            // after its last verified segment. Without them it cannot be trusted.
            if validation.is_none() {
                let _ = fs::remove_file(out.with_extension("part"));
            }
            let _ = fs::remove_file(&out);
            return Err(e).with_context(|| format!("downloading {}", p.name));
        }
    }

    em.progress("download", 100, "downloaded + verified");
    Ok(())
}

/// Fetch + parse a package's TYPE2 validation (per-segment SHA-256 list).
fn fetch_validation(
    agent: &ureq::Agent,
    headers: &BTreeMap<String, String>,
    base_url: &str,
) -> Result<Validation> {
    // Prefer TYPE2 (SHA-256, the product payloads). Some shared deps only publish
    // TYPE1 (MD5) — TYPE2 404s for them, so fall back to the base URL.
    let type2 = if base_url.contains('?') {
        format!("{base_url}&algorithm=TYPE2")
    } else {
        format!("{base_url}?algorithm=TYPE2")
    };
    // Either answer verifies the download. When TYPE2 fails for any reason, try the base URL
    // at once (a different file, possibly already cached); only when both fail is the error
    // returned, the transient one if there was one, so the caller retries.
    let xml = match get_text(agent, headers, &type2) {
        Ok(x) => x,
        Err(e2) => match get_text(agent, headers, base_url) {
            Ok(x) => x,
            Err(e1) if crate::net::transient(&e2) && !crate::net::transient(&e1) => return Err(e2),
            Err(e1) => return Err(e1.context(format!("GET {base_url} (validation)"))),
        },
    };
    let doc = roxmltree::Document::parse(&xml).context("parsing validation XML")?;

    let md5 = doc
        .descendants()
        .find(|n| n.has_tag_name("algorithm"))
        .and_then(|n| n.text())
        .map(|a| a.trim().eq_ignore_ascii_case("TYPE1"))
        .unwrap_or(false);

    let segment_size = doc
        .descendants()
        .find(|n| n.has_tag_name("segmentSize"))
        .and_then(|n| n.text())
        .and_then(|t| t.trim().parse::<u64>().ok())
        .context("validation XML missing segmentSize")?;

    let mut segs: Vec<(usize, String)> = doc
        .descendants()
        .filter(|n| n.has_tag_name("segment"))
        .filter_map(|n| {
            let num = n.attribute("segmentNumber")?.parse::<usize>().ok()?;
            Some((num, n.text()?.trim().to_string()))
        })
        .collect();
    segs.sort_by_key(|(num, _)| *num);
    let segments = segs.into_iter().map(|(_, h)| h).collect::<Vec<_>>();

    if segments.is_empty() {
        bail!("validation XML listed no segments");
    }
    Ok(Validation { segment_size, segments, md5 })
}

/// GET a URL with the Adobe headers, returning the body text (error on non-2xx).
fn get_text(agent: &ureq::Agent, headers: &BTreeMap<String, String>, url: &str) -> Result<String> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    Ok(req.call().with_context(|| format!("GET {url}"))?.into_string()?)
}

/// Stream one package to `<out>.part`, verifying each segment's SHA-256 against
/// `validation` as it arrives, then atomically rename on full success. With `validation`, a
/// `.part` left by an earlier try is resumed after its last segment that still matches Adobe's
/// hashes (an HTTP Range request; a server that ignores it sends the whole file and the download
/// starts over). Without it, the finished file must be `expected` bytes.
#[allow(clippy::too_many_arguments)]
fn download_one(
    em: &Emitter,
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    out: &Path,
    validation: Option<&Validation>,
    expected: u64,
    done: &mut u64,
    total: u64,
    last_pct: &mut u8,
) -> Result<()> {
    let tmp = out.with_extension("part");
    let label = out.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let seg_size = validation.map(|v| v.segment_size.max(1)).unwrap_or(u64::MAX);

    let kept = validation.map(|v| verified_segments(&tmp, v)).unwrap_or(0);
    if let Some(v) = validation {
        if kept > 0 && kept == v.segments.len() {
            // Every segment is there and verified: only the rename was missing.
            *done += fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            fs::rename(&tmp, out).with_context(|| format!("finalizing {}", out.display()))?;
            return Ok(());
        }
    }
    let resume_at = kept as u64 * if validation.is_some() { seg_size } else { 0 };

    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    if resume_at > 0 {
        req = req.set("Range", &format!("bytes={resume_at}-"));
    }
    let resp = req.call().with_context(|| format!("GET {url}"))?;
    let resumed = resume_at > 0 && resp.status() == 206;
    let mut reader = resp.into_reader();

    let mut f = if resumed {
        let mut f = fs::OpenOptions::new().write(true).open(&tmp).with_context(|| format!("opening {}", tmp.display()))?;
        f.set_len(resume_at)?;
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(resume_at))?;
        em.note(&format!("  resuming {label} at {:.1} MiB (verified)", resume_at as f64 / (1u64 << 20) as f64));
        *done += resume_at;
        f
    } else {
        File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?
    };
    let mut written = if resumed { resume_at } else { 0 };
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB

    // TYPE2 (product) = SHA-256; TYPE1 (small deps) = MD5.
    let mut hasher: Box<dyn DynDigest> = match validation {
        Some(v) if v.md5 => Box::new(Md5::new()),
        _ => Box::new(Sha256::new()),
    };
    let mut seg_filled = 0u64;
    let mut seg_idx = if resumed { kept } else { 0 };

    loop {
        let read = reader.read(&mut buf).context("reading from CDN")?;
        if read == 0 {
            break;
        }
        f.write_all(&buf[..read]).context("writing to disk")?;
        *done += read as u64;
        written += read as u64;

        if let Some(v) = validation {
            // Feed the chunk into the segment hasher, splitting at 2 MiB boundaries.
            let mut off = 0usize;
            while off < read {
                let take = ((seg_size - seg_filled) as usize).min(read - off);
                hasher.update(&buf[off..off + take]);
                seg_filled += take as u64;
                off += take;
                if seg_filled == seg_size {
                    let got = to_hex(&hasher.finalize_reset());
                    check_segment(v, seg_idx, &got, &label)?;
                    seg_idx += 1;
                    seg_filled = 0;
                }
            }
        }
        *last_pct = emit_pct(em, *done, total, &label, *last_pct);
    }

    if let Some(v) = validation {
        if seg_filled > 0 {
            let got = to_hex(&hasher.finalize_reset());
            check_segment(v, seg_idx, &got, &label)?;
            seg_idx += 1;
        }
        if seg_idx != v.segments.len() {
            bail!("{label}: verified {seg_idx} segment(s) but validation lists {}", v.segments.len());
        }
    } else if expected > 0 && written != expected {
        bail!("{label}: received {written} bytes, the manifest says {expected}");
    }

    f.flush()?;
    drop(f);
    fs::rename(&tmp, out).with_context(|| format!("finalizing {}", out.display()))?;
    Ok(())
}

/// How many whole segments at the start of a kept `.part` file still match Adobe's hashes (0 when
/// there is no such file). Only these are kept when a download resumes.
fn verified_segments(tmp: &Path, v: &Validation) -> usize {
    let Ok(mut f) = File::open(tmp) else { return 0 };
    let seg = v.segment_size.max(1) as usize;
    let mut buf = vec![0u8; seg];
    let mut good = 0usize;
    while good < v.segments.len() {
        let mut filled = 0usize;
        while filled < seg {
            match f.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        // A short read is the end of the file: a whole last segment of the package, or a partial
        // segment that is not kept.
        let last = good + 1 == v.segments.len();
        if filled == 0 || (filled < seg && !last) {
            break;
        }
        let mut h: Box<dyn DynDigest> = if v.md5 { Box::new(Md5::new()) } else { Box::new(Sha256::new()) };
        h.update(&buf[..filled]);
        if check_segment(v, good, &to_hex(&h.finalize_reset()), "").is_err() {
            break;
        }
        good += 1;
        if filled < seg {
            break;
        }
    }
    good
}

/// A file named `file` of exactly `bytes` under another products dir: `$MUDHUT_PACKAGE_SOURCES`
/// (colon-separated products dirs, e.g. an offline package or a kept download), then every
/// Mud Hut download-cache folder.
fn find_reusable(sap: &str, file: &str, bytes: u64, not: &Path) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("MUDHUT_PACKAGE_SOURCES")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    if let Ok(cache) = xdg_dir("XDG_CACHE_HOME", ".cache") {
        if let Ok(rd) = fs::read_dir(cache.join("mudhut")) {
            roots.extend(rd.flatten().map(|e| e.path().join("products")));
        }
    }
    roots.into_iter().map(|r| r.join(sap).join(file)).find(|c| {
        c != not && fs::metadata(c).map(|m| m.is_file() && m.len() == bytes).unwrap_or(false)
    })
}

/// Hash `path` segment by segment against Adobe's validation list.
fn verify_file(path: &Path, v: &Validation) -> Result<()> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher: Box<dyn DynDigest> = if v.md5 { Box::new(Md5::new()) } else { Box::new(Sha256::new()) };
    let seg = v.segment_size.max(1) as usize;
    let mut buf = vec![0u8; seg];
    let label = path.display().to_string();
    let mut idx = 0usize;
    loop {
        let mut filled = 0usize;
        while filled < seg {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        hasher.update(&buf[..filled]);
        check_segment(v, idx, &to_hex(&hasher.finalize_reset()), &label)?;
        idx += 1;
        if filled < seg {
            break;
        }
    }
    if idx != v.segments.len() {
        bail!("{label}: {idx} segment(s), validation lists {}", v.segments.len());
    }
    Ok(())
}

/// Reflink when the filesystem can (btrfs/XFS: instant, no space), else copy; via a .part file.
fn clone_or_copy(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let tmp = dst.with_extension("part");
    {
        let s = File::open(src)?;
        let d = File::create(&tmp)?;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        if unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) } != 0 {
            drop(d);
            fs::copy(src, &tmp)?;
        }
    }
    fs::rename(&tmp, dst)?;
    Ok(())
}

fn check_segment(v: &Validation, idx: usize, got: &str, label: &str) -> Result<()> {
    let want = v
        .segments
        .get(idx)
        .with_context(|| format!("{label}: segment {idx} beyond the validation list"))?;
    if !got.eq_ignore_ascii_case(want) {
        bail!("{label}: segment {idx} hash mismatch (got {got}, want {want})");
    }
    Ok(())
}

fn emit_pct(em: &Emitter, done: u64, total: u64, label: &str, last: u8) -> u8 {
    let p = if total == 0 {
        100
    } else {
        ((done.min(total) as f64 / total as f64) * 100.0) as u8
    };
    if p != last {
        em.progress("download", p, label);
    }
    p
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod xdg_tests {
    use super::xdg_from;
    use std::path::PathBuf;

    #[test]
    fn empty_or_relative_means_the_default() {
        let home = Some("/home/u".into());
        let want = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(xdg_from(Some("".into()), home.clone(), ".local/share"), want);
        assert_eq!(xdg_from(Some("data".into()), home.clone(), ".local/share"), want);
        assert_eq!(xdg_from(None, home.clone(), ".local/share"), want);
        assert_eq!(xdg_from(Some("/srv/data".into()), home, ".local/share"), Some(PathBuf::from("/srv/data")));
        assert_eq!(xdg_from(Some("".into()), None, ".local/share"), None);
    }
}

#[cfg(test)]
mod install_checks {
    use super::*;

    fn plan(build: &str, bytes: u64) -> DownloadPlan {
        DownloadPlan {
            app: "premiere".into(), sap: "PPRO".into(), name: "Adobe Premiere Pro".into(),
            product_version: build.into(), base_version: "26.0".into(), platform: "win64".into(),
            build_guid: "g".into(), language: "en_US".into(),
            packages: vec![crate::feed::PlannedPackage {
                sap: "PPRO".into(), name: "AdobePremierePro26AllTrial".into(), version: build.into(), module: None,
                extract_bytes: 0, kind: "core".into(), bytes, path: "/a.zip".into(),
                package_hash_key: String::new(), validation_url: String::new(),
            }],
            total_bytes: bytes, dependencies: vec![], modules: vec![], skipped_modules: vec![], skipped_bytes: 0,
        }
    }

    #[test]
    fn already_installed_needs_the_same_build_and_its_program_on_disk() {
        let dir = std::env::temp_dir().join(format!("mudhut-already-{}", std::process::id()));
        crate::hdpimdb::tests::make_db(
            &dir.join(crate::hdpimdb::DB_REL),
            &[("PPRO", "26.3", &[
                ("BaseVersion", "26.0"), ("BuildVersion", "26.3.0.93"), ("InstallDir", "C:\\Program Files\\Adobe"),
                ("AppLaunch", "[INSTALLDIR]\\Adobe Premiere Pro 2026\\Adobe Premiere Pro.exe"),
            ], &[])],
        );
        assert!(!already_installed(&dir, &plan("26.3.0.93", 1)), "a record without the program is not an install");
        let exe = dir.join("drive_c/Program Files/Adobe/Adobe Premiere Pro 2026/Adobe Premiere Pro.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"MZ").unwrap();
        assert!(already_installed(&dir, &plan("26.3.0.93", 1)));
        assert!(!already_installed(&dir, &plan("26.3.1.1", 1)), "another build is a real install");
        assert!(!already_installed(&dir.join("nothing-here"), &plan("26.3.0.93", 1)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_space_check_refuses_an_install_that_cannot_fit() {
        let dir = std::env::temp_dir().join(format!("mudhut-space-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("cache/mudhut/PPRO-26.3.0.93-win64/products");   // not created yet
        assert!(check_space(&dir, Some(&cache), &plan("26.3.0.93", 1)).is_ok());
        let e = check_space(&dir, Some(&cache), &plan("26.3.0.93", u64::MAX / 8)).unwrap_err();
        assert!(format!("{e:#}").contains("Not enough free disk space"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const SEG: usize = 2 << 20;

    fn data() -> Vec<u8> {
        (0..(5 << 20)).map(|i: usize| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn validation(d: &[u8]) -> Validation {
        let segments = d.chunks(SEG).map(|c| to_hex(&Sha256::digest(c))).collect();
        Validation { segment_size: SEG as u64, segments, md5: false }
    }

    /// Serve `body` on 127.0.0.1; honor `Range: bytes=N-` when `ranges`. Returns the URL and the
    /// Range header of every request ("" when none).
    fn serve(body: Vec<u8>, ranges: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pkg.zip", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut range = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim().trim_end_matches('-').to_string();
                    }
                }
                log.lock().unwrap().push(range.clone());
                let mut s = s;
                let from: usize = if ranges { range.parse().unwrap_or(0) } else { 0 };
                let part = &body[from..];
                let head = if ranges && from > 0 {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n", part.len(), body.len() - 1, body.len())
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", part.len())
                };
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(part);
            }
        });
        (url, seen)
    }

    fn run(url: &str, out: &Path, v: Option<&Validation>, expected: u64) -> Result<()> {
        let em = Emitter::new(true);
        let agent = ureq::AgentBuilder::new().build();
        let (mut done, mut pct) = (0u64, u8::MAX);
        download_one(&em, &agent, url, &BTreeMap::new(), out, v, expected, &mut done, 5 << 20, &mut pct)
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mudhut-resume-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_kept_part_resumes_after_its_last_verified_segment() {
        let d = data();
        let (url, seen) = serve(d.clone(), true);
        let out = dir("a").join("pkg.zip");
        let mut part = d[..SEG].to_vec();
        part.extend(vec![0xAAu8; 512 << 10]); // a torn second segment
        fs::write(out.with_extension("part"), &part).unwrap();
        run(&url, &out, Some(&validation(&d)), 0).unwrap();
        assert_eq!(fs::read(&out).unwrap(), d);
        assert_eq!(*seen.lock().unwrap(), vec![SEG.to_string()], "one request, from the 2nd segment");
    }

    #[test]
    fn a_server_that_ignores_range_restarts_cleanly() {
        let d = data();
        let (url, seen) = serve(d.clone(), false);
        let out = dir("b").join("pkg.zip");
        fs::write(out.with_extension("part"), &d[..SEG + 100]).unwrap();
        run(&url, &out, Some(&validation(&d)), 0).unwrap();
        assert_eq!(fs::read(&out).unwrap(), d);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_complete_part_is_finalized_without_a_request() {
        let d = data();
        let (url, seen) = serve(d.clone(), true);
        let out = dir("c").join("pkg.zip");
        fs::write(out.with_extension("part"), &d).unwrap();
        run(&url, &out, Some(&validation(&d)), 0).unwrap();
        assert_eq!(fs::read(&out).unwrap(), d);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn without_hashes_the_size_is_checked() {
        let d = data();
        let (url, _) = serve(d.clone(), true);
        let out = dir("d").join("pkg.zip");
        let e = run(&url, &out, None, (6 << 20) as u64).unwrap_err();
        assert!(format!("{e:#}").contains("the manifest says"), "{e:#}");
        assert!(!out.exists());
        run(&url, &out, None, d.len() as u64).unwrap();
        assert_eq!(fs::read(&out).unwrap(), d);
    }
}
