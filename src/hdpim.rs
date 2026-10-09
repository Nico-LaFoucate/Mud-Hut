// SPDX-License-Identifier: Apache-2.0
//! The offline install engine: drive Adobe's own `HDPIM.dll` to install a genuine
//! app into a Neutron prefix, fully decrypted, with no Set-up.exe / WAM / CC-desktop.
//!
//! Orchestration only (no Adobe binary modified, no DRM reimplemented): we set the
//! prefix prerequisites, seed Adobe's Desktop-Common runtime from the public ACCCx
//! `.pima` archives, then run the small 32-bit `hdpim_host.exe`, which `LoadLibrary`s
//! Adobe's shipped `HDPIM.dll` and calls its exported `hdpimInstallProduct`. HDPIM
//! decrypts the payloads and builds `Media_db.db` itself. See
//! `docs/HDPIM_OFFLINE_INSTALL_METHODOLOGY.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::output::Emitter;

/// The in-prefix Windows path to the HDBox HDPIM.dll (seeded by the ACC runtime).
const HDPIM_WIN: &str =
    r"C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common\HDBox\HDPIM.dll";

/// Resolved external tools the engine drives.
pub struct Config {
    /// Neutron wine binary (64-bit build; runs the 32-bit hdpim_host via wow64).
    pub wine: PathBuf,
    /// The 32-bit orchestrator that calls HDPIM's install API.
    pub hdpim_host: PathBuf,
    /// The ACCCx `.pima` runtime extractor (Python 3, stdlib only).
    pub extractor: PathBuf,
    /// Staged public ACCCx runtime packages (AAM/ ADC/ ADC64/ …).
    pub accc_packages: PathBuf,
}

/// Discover the engine's tools. `repo_tools` is the dir holding the shipped
/// `hdpim_host.exe` + `extract_accc_runtime.py` (Mud Hut's `tools/`). Wine is taken
/// from `$MUDHUT_WINE`, else the runtime the target `prefix` is stamped for, else whatever
/// `neutron runtime which` reports, else the newest numeric Neutron runtime.
///
/// Pass the target prefix whenever it is known: installing into an existing prefix with
/// a mismatched wine wineboot-clobbers its patched natives.
pub fn discover(repo_tools: &Path, accc_packages: PathBuf, prefix: Option<&Path>) -> Result<Config> {
    let wine = resolve_wine(prefix).context(
        "neutron-wine isn't installed. Run `neutron setup` first (or set $MUDHUT_WINE).",
    )?;
    let hdpim_host = repo_tools.join("hdpim_host.exe");
    let extractor = repo_tools.join("extract_accc_runtime.py");
    for (what, p) in [("hdpim_host.exe", &hdpim_host), ("extract_accc_runtime.py", &extractor)] {
        if !p.is_file() {
            bail!("missing {what}: {}", p.display());
        }
    }
    if !accc_packages.is_dir() {
        bail!("ACCCx runtime packages dir not found: {}", accc_packages.display());
    }
    Ok(Config { wine, hdpim_host, extractor, accc_packages })
}

/// Ask `neutron runtime which` for the wine the CLI would launch with. None if the CLI is
/// missing, older than that subcommand, or answers with a path that is not there.
///
/// ⛔ Parsed without a JSON dependency: the field is a plain `"wine": "<path>"`. If that output
/// shape ever changes this returns None and resolution falls through to the old behavior, which
/// is the safe direction -- Mud Hut must never fail to install because a helper changed format.
fn neutron_runtime_which() -> Option<PathBuf> {
    let out = Command::new("neutron").args(["--json", "runtime", "which"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let key = "\"wine\": \"";
    let start = text.find(key)? + key.len();
    let end = text[start..].find('"')? + start;
    let p = PathBuf::from(&text[start..end]);
    p.is_file().then_some(p)
}

pub(crate) fn resolve_wine(prefix: Option<&Path>) -> Option<PathBuf> {
    // 1. Explicit override — MUDHUT_WINE, or NEUTRON_WINE (honor whatever the user
    //    already pointed neutron at, so the two agree on one wine).
    for var in ["MUDHUT_WINE", "NEUTRON_WINE"] {
        if let Ok(w) = std::env::var(var) {
            let p = PathBuf::from(w);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // 2. If we know the target prefix, the ONLY correct answer is the runtime that
    //    prefix is stamped for. Wine reruns its whole wine.inf install whenever the
    //    mtime of <wine>/share/wine/wine.inf differs from <prefix>/.update-timestamp,
    //    and setup_prefix() then runs `wineboot --init` unconditionally — so resolving
    //    any other build reinstalls system32 and reverts the prefix's patched natives.
    if let (Some(home), Some(prefix)) = (&home, prefix) {
        if let Some(p) = runtime_for_prefix(home, prefix) {
            return Some(p);
        }
    }
    // 2b. ⭐ ASK THE NEUTRON CLI. For a NEW prefix (no stamp yet) there is nothing to derive the
    //     runtime from, and "newest installed" is only right by coincidence: it agrees with the
    //     CLI on a machine where the newest runtime is also the pinned one, and disagrees the
    //     moment a newer runtime is installed than the drop pinned. Mud Hut would then stamp a
    //     brand-new prefix for the newer build, and every `neutron launch` would refuse it --
    //     the worst possible first-run experience, raised by the build tester 2026-08-26.
    //     One authority for "which wine does this machine use": the CLI. Ask it.
    //     Best-effort: if `neutron` is absent or too old for `runtime which`, fall through.
    if let Some(w) = neutron_runtime_which() {
        return Some(w);
    }
    // 3. Installed Neutron runtime, newest by NUMERIC version.
    //    ⚠️ This used to be `cands.sort(); cands.pop()` — a lexicographic sort over full
    //    paths, which ranked "neutron-wine-11.10-perf-test" above "neutron-wine-11.10-45"
    //    because "p" > "4". On the dev box that selected a July build marked TEST ONLY,
    //    NEVER SHIP to run against a prefix stamped for 11.10-45.
    //    Non-numeric suffixes (perf-test, gpufix, diag, overhang…) are experiment builds
    //    and are now excluded entirely rather than merely ranked.
    if let Some(home) = &home {
        let base = home.join(".local/share/neutron/runtimes");
        if let Ok(rd) = std::fs::read_dir(&base) {
            let mut cands: Vec<((u32, u32, u32), PathBuf)> = rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let v = parse_runtime_version(&name)?;
                    let wine = e.path().join("bin/wine");
                    wine.is_file().then_some((v, wine))
                })
                .collect();
            cands.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some((_, p)) = cands.pop() {
                return Some(p);
            }
        }
    }
    None
}

/// Parse `neutron-wine-<major>.<minor>-<rev>` into a comparable tuple. Returns None for
/// experiment builds whose revision is not purely numeric (`-perf-test`, `-gpufix`,
/// `-13-overhang2`, …) so they can never be auto-selected.
fn parse_runtime_version(dir_name: &str) -> Option<(u32, u32, u32)> {
    let rest = dir_name.strip_prefix("neutron-wine-")?;
    let (ver, rev) = rest.rsplit_once('-')?;
    let (major, minor) = ver.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?, rev.parse().ok()?))
}

/// The installed runtime whose `share/wine/wine.inf` mtime equals the integer in
/// `<prefix>/.update-timestamp` — i.e. the build this prefix was last booted on.
/// Running any other build against it triggers a full `wine.inf` reinstall.
fn runtime_for_prefix(home: &Path, prefix: &Path) -> Option<PathBuf> {
    // The stamp file is CRLF-terminated; keep digits only.
    let raw = std::fs::read_to_string(prefix.join(".update-timestamp")).ok()?;
    let stamp: u64 = raw.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;

    for entry in std::fs::read_dir(home.join(".local/share/neutron/runtimes")).ok()?.flatten() {
        let meta = match std::fs::metadata(entry.path().join("share/wine/wine.inf")) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if secs == Some(stamp) {
            let wine = entry.path().join("bin/wine");
            if wine.is_file() {
                return Some(wine);
            }
        }
    }
    None
}

/// Full install: prereqs -> seed runtime -> HDPIM install -> verify. `packages_dir`
/// is the staged product package dir (holds `<SAP>/`, `Driver_core.xml`, deps).
/// `driver_xml` is the DriverInfo to feed HDPIM. Returns the installed exe path.
pub fn install(
    em: &Emitter,
    cfg: &Config,
    prefix: &Path,
    cat: &crate::catalog::App,
    driver_xml: &Path,
    packages_dir: &Path,
    dry_run: bool,
) -> Result<PathBuf> {
    if dry_run {
        em.note(&format!(
            "would install {} into {} via HDPIM (prereqs + ACC seed + hdpim_host)",
            cat.name,
            prefix.display()
        ));
        return Ok(prefix.join("drive_c/Program Files/Adobe").join(format!("{} <year>", cat.dir_prefix)));
    }

    // ⛔ FIRST, before touching the prefix at all. This check used to live in
    // run_hdpim, which is after prefix setup -- so a second install into a busy
    // prefix got as far as the VC++ step, whose `wineserver -w` then blocked
    // on the first install's processes and looked like "hanging on vcrun2022".
    // Refusing here names the other install instead of deadlocking behind it.
    clear_orphan_hosts(em, prefix)?;

    em.progress("prereqs", 5, "prefix init + Win11 spoof + VC++ runtime");
    setup_prefix(em, cfg, prefix)?;

    em.progress("seed", 25, "ACC runtime (HDBox/HDPIM + ESD engine)");
    seed_runtime(em, cfg, prefix)?;

    em.progress("install", 45, "HDPIM decrypt + install (this takes minutes)");
    let exe = run_hdpim(em, cfg, prefix, cat, driver_xml, packages_dir)?;
    em.progress("verify", 95, "checking the decrypted binary");
    verify(&exe)?;
    em.progress("install", 100, "installed");
    Ok(exe)
}

/// Find the installed app exe under `Program Files\Adobe\Adobe <app_name> <year>\`.
/// Returns the highest-year match that exists.
/// An `hdpim_host` already running against `prefix`, as (pid, owner pid).
///
/// A second decrypt into the same prefix corrupts the install, and an orphan
/// left by a killed run silently blocks or breaks the next attempt. Neither is
/// detectable from inside our own process, so read /proc.
fn hosts_in_prefix(prefix: &Path) -> Vec<(i32, i32)> {
    let want = format!("WINEPREFIX={}", prefix.display());
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let name = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Name:")).map(|l| l.to_string()))
            .unwrap_or_default();
        if !name.contains("hdpim_host") {
            continue;
        }
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        if !env.split(|b| *b == 0).any(|v| v == want.as_bytes()) {
            continue;
        }
        let ppid = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("PPid:"))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
            })
            .unwrap_or(1);
        out.push((pid, ppid));
    }
    out
}

/// Refuse to start a second decrypt into a prefix, and clear orphaned ones.
///
/// If the host's owner is a live mudhut, another install is genuinely in flight
/// and starting ours would interleave two decrypts into the same directories —
/// that is refused. If the owner is gone, the host is an orphan from a killed
/// run: it is killed here so it cannot corrupt this install or be mistaken by
/// the user for the one they should not touch.
fn clear_orphan_hosts(em: &Emitter, prefix: &Path) -> Result<()> {
    // Another mudhut install already working on this prefix? Refuse before we
    // touch anything. An hdpim_host only exists during the decrypt, so keying on
    // that alone misses a rival that is still in prereqs -- which is exactly when
    // the collision bites, because the VC++ step's `wineserver -w` then
    // blocks on the other install's wine processes and reports itself as a hung
    // "vcrun2022" step.
    //
    // ⛔ Skip our OWN pid: a cmdline match for the prefix necessarily matches this
    // very process.
    let me = std::process::id() as i32;
    let want = format!("--prefix{}{}", '\0', prefix.display());
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            if pid == me {
                continue;
            }
            let is_mudhut = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|c| c.trim() == "mudhut")
                .unwrap_or(false);
            if !is_mudhut {
                continue;
            }
            let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
            let cmd = String::from_utf8_lossy(&raw);
            if cmd.contains("install") && cmd.contains(&want) {
                bail!(
                    "another Mud Hut install (pid {pid}) is already working on this prefix.\n\
                     Two installs into one prefix deadlock each other — each waits for \
                     every process in the prefix to exit, and the other install's will not.\n\
                     Wait for it to finish, or stop it with: kill {pid}"
                );
            }
        }
    }

    for (pid, ppid) in hosts_in_prefix(prefix) {
        let owner_alive = std::fs::read_to_string(format!("/proc/{ppid}/comm"))
            .map(|c| c.trim() == "mudhut")
            .unwrap_or(false);
        if owner_alive {
            bail!(
                "another Mud Hut install is already running into this prefix \
                 (hdpim_host pid {pid}, started by mudhut pid {ppid}).\n\
                 Two decrypts into one prefix corrupt each other. Wait for it to \
                 finish, or stop that install first."
            );
        }
        em.note(&format!(
            "clearing an orphaned installer host from a previous run (pid {pid})"
        ));
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    Ok(())
}

fn find_installed_exe(prefix: &Path, cat: &crate::catalog::App) -> Option<PathBuf> {
    let adobe = prefix.join("drive_c/Program Files/Adobe");
    // The catalog's folder name, never one built from the display name: "Lightroom
    // (experimental)" installs to "Adobe Lightroom CC", so a name-built prefix made a
    // finished install fail with "no installed exe was found" and skip provisioning.
    let dir_prefix = cat.dir_prefix;

    // The catalog carries the exact executable, because it is NOT derivable from
    // the app name and is not always at the top level: After Effects ships
    // "Support Files/AfterFX.exe", Illustrator sits three levels down. Guessing it
    // (the old code took the last word of the app name) made a finished Premiere
    // look like a timeout, and no depth of top-level search would ever find AE.
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&adobe)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(dir_prefix))
                .unwrap_or(false)
        })
        .collect();
    dirs.sort();

    for dir in dirs.iter().rev() {          // newest install dir first ("… 2026")
        let p = dir.join(cat.exe);
        if p.is_file() {
            return Some(p);
        }
        // Adobe has moved this between releases before, so fall back to a bounded
        // search for the same FILE NAME rather than failing on a moved directory.
        if let Some(name) = Path::new(cat.exe).file_name() {
            if let Some(found) = find_named(dir, name, 4) {
                return Some(found);
            }
        }
    }
    None
}

/// Breadth-first search for `name` under `root`, at most `max_depth` levels down.
/// Megabytes written into the app's install directory so far (best-effort).
fn dir_size_mb(prefix: &Path, cat: &crate::catalog::App) -> u64 {
    fn walk(d: &Path, budget: &mut u32) -> u64 {
        if *budget == 0 {
            return 0;
        }
        let mut total = 0;
        let Ok(rd) = std::fs::read_dir(d) else { return 0 };
        for e in rd.flatten() {
            *budget = budget.saturating_sub(1);
            if *budget == 0 {
                break;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => total += walk(&e.path(), budget),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
        total
    }
    let adobe = prefix.join("drive_c/Program Files/Adobe");
    let want = cat.dir_prefix;
    let mut budget = 60_000u32;      // bounded: this runs every 10s during install
    std::fs::read_dir(&adobe)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| {
            d.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with(want)).unwrap_or(false)
        })
        .map(|d| walk(&d, &mut budget))
        .sum::<u64>()
        / 1_048_576
}

fn find_named(root: &Path, name: &std::ffi::OsStr, max_depth: u32) -> Option<PathBuf> {
    let mut q = std::collections::VecDeque::from([(root.to_path_buf(), 0u32)]);
    let mut seen = 0u32;
    while let Some((dir, depth)) = q.pop_front() {
        let hit = dir.join(name);
        if hit.is_file() {
            return Some(hit);
        }
        if depth >= max_depth || seen > 4096 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 4096 {
                break;
            }
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                q.push_back((e.path(), depth + 1));
            }
        }
    }
    None
}

/// wineboot (mono/gecko dialog suppressed) + Win11 24H2 spoof + Microsoft's components.
fn setup_prefix(em: &Emitter, cfg: &Config, prefix: &Path) -> Result<()> {
    // Init: suppress the interactive Mono/Gecko installer dialog (it blocks headless).
    wine(cfg, prefix, &["wineboot", "--init"])
        .env("WINEDLLOVERRIDES", "mscoree,mshtml=d;winemenubuilder.exe=d")
        .status_ok("wineboot --init")?;

    // Win11 24H2 spoof (HDPIM gates on the OS version; see the methodology doc, step 3).
    let cv = r"HKLM\Software\Microsoft\Windows NT\CurrentVersion";
    let regs: &[(&str, &str, &str)] = &[
        ("CurrentBuild", "REG_SZ", "26100"),
        ("CurrentBuildNumber", "REG_SZ", "26100"),
        ("CurrentVersion", "REG_SZ", "10.0"),
        ("DisplayVersion", "REG_SZ", "24H2"),
        ("ProductName", "REG_SZ", "Windows 11 Pro"),
        ("CurrentMajorVersionNumber", "REG_DWORD", "10"),
    ];
    for (name, ty, data) in regs {
        wine(cfg, prefix, &["reg", "add", cv, "/v", name, "/t", ty, "/d", data, "/f"])
            .status_ok("reg add (Win11 spoof)")?;
    }
    // Remove the HKCU Wine version override (else Wine forces build 22000).
    let _ = wine(cfg, prefix, &["reg", "delete", r"HKCU\Software\Wine", "/v", "Version", "/f"])
        .status(); // ok if absent

    // Microsoft's components BEFORE Adobe's installer, which looks for the VC++ runtimes: VC++ 2013
    // and 2015-2022 (Adobe's native C++ needs the real redists; Wine's builtins crash), the UCRT,
    // d3dcompiler_47, GDI+ and the Microsoft core fonts. `neutron prefix provision
    // --microsoft-only` downloads them once from Microsoft, checks them against pinned checksums
    // and runs Microsoft's own VC++ installers, so Mud Hut needs no winetricks.
    //
    // corefonts is NOT cosmetic: CoolType picks the Roman default by PostScript name and throws
    // on Wine's substitutes, so Premiere, After Effects and Media Encoder fail at STARTUP without
    // genuine georgia/verdana/impact/trebuc/times/arial.
    em.progress("prereqs", 15, "Microsoft VC++ runtimes, UCRT, GDI+ and core fonts");
    microsoft_components(cfg, prefix)?;
    // Flush the registry cleanly — but BOUNDED.
    //
    // `wineserver -w` waits for EVERY process in the prefix to exit, and an app the
    // user still has open (or an orphan that outlived its window — Photoshop leaves
    // one, with AdobeIPCBroker beside it) holds it open forever. Measured on a real
    // hang: mudhut in do_wait on `wineserver -w`, blocked on a Photoshop.exe that
    // had been sleeping for 27 minutes after the user closed and force-quit it.
    //
    // The wait is a courtesy (the registry also flushes on its own), so a busy
    // prefix must not stall the install. Time it out and carry on.
    wineserver_wait_bounded(cfg, prefix, Duration::from_secs(20), em);
    Ok(())
}

/// Seed Adobe Desktop Common (HDBox/HDPIM + ESD engine + ADS/IPCBroker) from the
/// public ACCCx `.pima`.
///
/// ⭐ ALL FIVE SETS. This used to extract only `ADC,ADC64` — Adobe Desktop Common,
/// enough to run the HDPIM install engine but NOT enough to leave a working
/// desktop behind. The result was a prefix with no `AdobeApplicationManager` and
/// no Creative Cloud Desktop, which a comparison against a known-good 2025 prefix
/// shows it should have:
///   AAM        -> AdobeApplicationManager (IPC)            688 KB
///   ACC/ACC64  -> the Creative Cloud Desktop app itself     77 MB
///   ADC/ADC64  -> Adobe Desktop Common (HDBox/HDPIM, ADS, IPCBox)
/// The extractor is idempotent — it writes only files that are absent and reports
/// the rest as "already present" — so this re-runs safely on a populated prefix.
fn seed_runtime(em: &Emitter, cfg: &Config, prefix: &Path) -> Result<()> {
    let out = Command::new("python3")
        .arg(&cfg.extractor)
        .arg("--packages")
        .arg(&cfg.accc_packages)
        .arg("--prefix")
        .arg(prefix)
        .arg("--sets")
        .arg("AAM,ACC,ACC64,ADC,ADC64")
        .output()
        .context("running extract_accc_runtime.py")?;
    if !out.status.success() {
        bail!("ACC runtime extraction failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    em.progress("seed", 40, "runtime staged");
    Ok(())
}

/// Run the 32-bit host that drives HDPIM's install API. mshtml disabled so HDPIM's
/// optional HD web-view UI can't crash Wine's mshtml (headless install proceeds).
fn run_hdpim(
    em: &Emitter,
    cfg: &Config,
    prefix: &Path,
    cat: &crate::catalog::App,
    driver_xml: &Path,
    packages_dir: &Path,
) -> Result<PathBuf> {
    use std::time::{Duration, Instant};
    // hdpim_host resolves EsdDirectory relative to its CWD (the driver.xml dir).
    // HDPIM's install runs async and the host otherwise pumps its full wait; we stop
    // it EARLY once the decrypted PE appears, so the CLI doesn't idle for ~30 min.
    let driver_win = to_z_path(driver_xml);
    // Refuse a second decrypt into this prefix; clear an orphan from a killed run.
    clear_orphan_hosts(em, prefix)?;

    let mut cmd = wine(cfg, prefix, &[]);
    cmd.arg(&cfg.hdpim_host)
        .arg(HDPIM_WIN)
        .arg(&driver_win)
        .arg("2400") // pump-second ceiling
        .current_dir(packages_dir)
        .env("WINEDLLOVERRIDES", "mshtml=d;winemenubuilder.exe=d");
    // Tie the host's life to ours: if Mud Hut is killed, the decrypt must not keep
    // running against a prefix nobody is supervising. Covers SIGKILL of the parent,
    // which no cleanup code of ours could handle.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    // Capture the host's output to a FILE so we can watch it for HDPIM's own
    // completion signal — and so its chatter never lands in the stdout pipe
    // Collider reads as NDJSON.
    let mut host_log_path = std::env::temp_dir();
    host_log_path.push(format!("mudhut-hdpim-{}.log", std::process::id()));
    let host_log = std::fs::File::create(&host_log_path)
        .with_context(|| format!("creating {}", host_log_path.display()))?;
    let host_log2 = host_log.try_clone().context("duplicating the host log fd")?;
    cmd.stdout(Stdio::from(host_log)).stderr(Stdio::from(host_log2));

    let mut child = cmd.spawn().context("spawning hdpim_host")?;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(2400);
    let exe = loop {
        if let Some(status) = child.try_wait().context("polling hdpim_host")? {
            // Host exited on its own — the install must have produced the exe.
            match find_installed_exe(prefix, cat) {
                Some(exe) => break exe,
                None => bail!(
                    "hdpim_host exited ({status}) but no installed {} exe found.{}",
                    cat.name,
                    kept_log(&host_log_path)
                ),
            }
        }
        // ⛔ DO NOT stop just because the main executable exists.
        //
        // That is what this did, and it shipped a broken install: Photoshop.exe is
        // ONE payload of ~40 and lands early, so killing the host on sight of it
        // terminated HDPIM while it was still laying down the rest of the
        // application directory, Camera Raw, Color, CoreSync and the CC pieces.
        // Photoshop then refuses to start with "Some of the Application components
        // are missing from the Application directory" and "Adobe Creative Cloud …
        // is missing or damaged". It survived on the dev box only because that
        // prefix had been installed into repeatedly; a FRESH prefix does not.
        //
        // HDPIM says when it is finished — "All N tasks completed." from its
        // ProgressManager. Use that.
        let log = std::fs::read_to_string(&host_log_path).unwrap_or_default();

        // HDPIM refuses to install over an existing install: it fails the workflow
        // (error 130) and releases its locks. The old code could not see that — it
        // stopped as soon as the app's exe existed, which on a reinstall is
        // IMMEDIATELY, so it reported "installed" having done nothing at all. A
        // user trying to repair a damaged install got a success message and an
        // unchanged prefix. Surface it instead.
        if let Some(i) = log.find("Error occurred in install product workflow with error code") {
            let code: String = log[i..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let _ = child.kill();
            let _ = child.wait();
            let kept = kept_log(&host_log_path);
            let already = find_installed_exe(prefix, cat).is_some();
            let hint = if already {
                format!(
                    "\n\n{} is ALREADY installed in this prefix, and HDPIM will not install over \
                     it — so a reinstall cannot repair a damaged copy. Install into a fresh prefix, \
                     or remove the existing application directory under\n  {}",
                    cat.name,
                    prefix.join("drive_c/Program Files/Adobe").display()
                )
            } else {
                String::new()
            };
            bail!("HDPIM refused the install (workflow error {code}).{hint}{kept}");
        }

        if log.contains("tasks completed.") {
            let exe = find_installed_exe(prefix, cat)
                .context("HDPIM reported all tasks completed but no installed exe was found")?;
            verify(&exe)?;
            em.progress("install", 90, "all HDPIM tasks completed — stopping host");
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&host_log_path);
            break exe;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let tail: Vec<&str> = log.lines().rev().take(5).collect();
            bail!(
                "HDPIM install timed out after 2400s without reporting completion.\nLast lines:\n{}{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
                kept_log(&host_log_path)
            );
        }
        // Report progress DURING the decrypt. Without this the CLI emitted nothing
        // between "seed" (25%) and "installed" (90%), so Collider's bar sat frozen
        // at 25% for the entire install -- 12+ minutes for After Effects -- and the
        // only reasonable reading was that it had hung. It had not.
        //
        // There is no honest completion percentage available (HDPIM reports none,
        // and expanded size is not derivable from the compressed payloads), so the
        // bar ramps asymptotically toward 85% with elapsed time and never claims to
        // finish, while the MESSAGE carries the real, checkable signal: megabytes
        // written so far. A stalled install shows a frozen MB count with a bar that
        // is still moving, which is the honest way round.
        let secs = Instant::now().saturating_duration_since(started).as_secs();
        let mb = dir_size_mb(prefix, cat);
        // HDPIM logs a line per finished task; counting them is a real signal the
        // user can watch, unlike an elapsed-time ramp.
        let tasks = log.matches("Completed 'INSTALL' task").count();
        let pct = 25u8 + (60.0 * (1.0 - (-(secs as f64) / 420.0).exp())) as u8;
        em.progress(
            "install",
            pct.min(85),
            &format!(
                "HDPIM decrypt · {tasks} tasks done · {mb} MB written · {}m{:02}s elapsed",
                secs / 60,
                secs % 60
            ),
        );
        std::thread::sleep(Duration::from_secs(10));
    };
    let _ = wineserver(cfg, prefix, "-k");
    Ok(exe)
}

/// On a failed install, keep HDPIM's log (it names the component and the reason) under
/// `$XDG_CACHE_HOME/mudhut/logs/` and return a message with its path and its error lines.
/// The log used to be deleted on every failure, so "workflow error 182" arrived with no way
/// to see which package HDPIM was missing.
fn kept_log(host_log: &Path) -> String {
    let log = std::fs::read_to_string(host_log).unwrap_or_default();
    let errors: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("rror") || l.contains("not present") || l.contains("failed"))
        .collect();
    let shown = errors[errors.len().saturating_sub(8)..].join("\n");
    let dir = crate::download::xdg_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join("mudhut/logs"));
    let kept = dir.ok().and_then(|d| {
        std::fs::create_dir_all(&d).ok()?;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_secs())
            .unwrap_or(0);
        let dest = d.join(format!("hdpim-{secs}.log"));
        std::fs::copy(host_log, &dest).ok()?;
        let _ = std::fs::remove_file(host_log);
        Some(dest)
    });
    let at = kept.as_deref().unwrap_or(host_log);
    format!("\nHDPIM's log: {}\n{}", at.display(), shown)
}

/// The install succeeded only if the app exe is a real decrypted PE (not the ~1 MB
/// encrypted stub) — judge by size + the `MZ` header, per the methodology.
fn verify(exe: &Path) -> Result<()> {
    let meta = std::fs::metadata(exe)
        .with_context(|| format!("installed exe not found: {}", exe.display()))?;
    // ⛔ Do NOT gate on a big size. This floor was 50 MB, calibrated on Photoshop
    // (269 MB) and Premiere (836 MB) -- but After Effects' AfterFX.exe is 5.7 MB
    // (AE is mostly DLLs and plugins), so a perfectly good install would have been
    // rejected as "decrypt likely failed" even once it was found. What actually
    // distinguishes a decrypted binary from an encrypted or truncated payload is
    // that it PARSES as a PE, which is checked below; the floor only needs to
    // exclude a stub.
    if meta.len() < 1_000_000 {
        bail!(
            "installed exe suspiciously small ({} bytes) — decrypt likely failed: {}",
            meta.len(),
            exe.display()
        );
    }
    let mut buf = [0u8; 2];
    use std::io::Read;
    std::fs::File::open(exe)?.read_exact(&mut buf)?;
    if &buf != b"MZ" {
        bail!("installed exe is not a PE (no MZ header): {}", exe.display());
    }
    Ok(())
}

// --- helpers ---------------------------------------------------------------

/// A `wine` Command pre-seeded with the prefix + a quiet debug channel.
fn wine(cfg: &Config, prefix: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(&cfg.wine);
    c.env("WINEPREFIX", prefix)
        // ⛔ Kill winemenubuilder. Left enabled it mirrors the prefix's Start Menu
        // into ~/.local/share/applications/wine/Programs/, and those launchers run
        // BARE `wine` from PATH -- distro wine, not the pinned Neutron runtime. A
        // menu launch through one of them then fires `wineboot -u` and reverts the
        // prefix's patched natives. They also carry
        // the same StartupWMClass as ours, so KDE cannot tell them apart and one can
        // silently take over a dock pin -- which is how a user's pinned Premiere 2025
        // became 2026 after installing the 2026 suite.
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d")
        .env("WINEDEBUG", "err-all,fixme-all")
        .env("WAYLAND_DISPLAY", std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into()));
    // args may be a placeholder for the hdpim path (see cmd_hdpim); skip empties.
    for a in args {
        if !a.is_empty() {
            c.arg(a);
        }
    }
    c
}

/// `wineserver -w`, abandoned after `limit`. Best-effort by design: see the call
/// site for why an unbounded wait is a hang waiting to happen.
/// Run `cmd` to completion or kill it after `limit`. Ok(true) = it exited on its
/// own, Ok(false) = it was stopped. For steps that are best-effort and must never
/// be able to hang the install.
/// `neutron prefix provision --microsoft-only <prefix>` with Mud Hut's wine, bounded. Fails the
/// install with the CLI's own reason when a component could not be downloaded or installed.
fn microsoft_components(cfg: &Config, prefix: &Path) -> Result<()> {
    let mut child = Command::new("neutron")
        .args(["--json", "prefix", "provision", "--microsoft-only"])
        .arg(prefix)
        .env("NEUTRON_WINE", &cfg.wine)
        .env("WINEDEBUG", "-all")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run `neutron` (is Neutron set up? run `neutron setup`)")?;
    // ⛔ BOUNDED: Microsoft's installers wait on the prefix's wineserver, and anything else live
    // in the prefix can hold that forever. The JSON result is a few KB, well inside a pipe buffer.
    let deadline = Instant::now() + Duration::from_secs(900);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("installing the Microsoft components did not finish in 15 minutes. Check that \
                   nothing else is running in this prefix, then try again.");
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        use std::io::Read;
        let _ = so.read_to_string(&mut out);
    }
    if status.success() {
        return Ok(());
    }
    let detail = serde_json::from_str::<serde_json::Value>(&out).ok()
        .and_then(|v| v["steps"].as_array().cloned())
        .and_then(|steps| steps.iter()
            .find(|st| st["ok"] == serde_json::Value::Bool(false))
            .map(|st| st["detail"].as_str().unwrap_or("").to_string()))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("`neutron prefix provision --microsoft-only` exited with {status}"));
    bail!("could not install the Microsoft components: {detail}")
}

fn wineserver_wait_bounded(cfg: &Config, prefix: &Path, limit: Duration, em: &Emitter) {
    let ws = cfg.wine.with_file_name("wineserver");
    let ws = if ws.is_file() { ws } else { cfg.wine.with_file_name("server").join("wineserver") };
    let mut child = match Command::new(ws)
        .arg("-w")
        .env("WINEPREFIX", prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Err(_) => return,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            em.note(
                "something is still running in this prefix, so the registry flush was skipped \
                 (harmless). If an app is open in it, closing it first avoids this.",
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wineserver(cfg: &Config, prefix: &Path, flag: &str) -> std::io::Result<std::process::ExitStatus> {
    // wineserver sits next to wine.
    let ws = cfg.wine.with_file_name("wineserver");
    let ws = if ws.is_file() { ws } else { cfg.wine.with_file_name("server").join("wineserver") };
    Command::new(ws).arg(flag).env("WINEPREFIX", prefix).status()
}

/// Map an absolute Linux path to a Wine `Z:` Windows path.
pub(crate) fn to_z_path(p: &Path) -> String {
    format!("Z:{}", p.to_string_lossy().replace('/', "\\"))
}

// Small extension trait so the call sites read cleanly.
trait CommandExt {
    fn status_ok(&mut self, what: &str) -> Result<()>;
}
impl CommandExt for Command {
    fn status_ok(&mut self, what: &str) -> Result<()> {
        // Capture to a FILE, never a pipe.
        //
        // ⛔ `Command::output()` waits for EOF on the child's stdout/stderr, NOT
        // for the child to exit. wineserver is a daemon that outlives the command
        // that started it and inherits its handles, so those pipes never reach EOF
        // and output() blocks forever with the child already reaped as a zombie.
        // Measured: mudhut asleep in poll() on two pipes whose write ends were held
        // by a wineserver alive for 14 minutes. It only bites on the SECOND install
        // into a prefix, where a wineserver from the first one is still up, which is
        // exactly the case a single end-to-end install never reaches.
        //
        // A file has no EOF dependency, `status()` waits only for the child, and as
        // a bonus wine's chatter no longer flows into whatever pipe our own stdout
        // is attached to (Collider reads it as NDJSON).
        let mut log_path = std::env::temp_dir();
        log_path.push(format!(
            "mudhut-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("creating a log file for {what}"))?;
        let log2 = log.try_clone().with_context(|| format!("duplicating the log fd for {what}"))?;

        let st = self
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .status()
            .with_context(|| format!("spawning {what}"))?;

        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(&log_path);

        if !st.success() {
            // Wine's real complaint is at the END, after the driver noise.
            let tail: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| {
                    !l.is_empty()
                        // emitted on healthy runs too
                        && !l.contains("MESA-EGL")
                        && !l.starts_with("pci id for fd")
                })
                .rev()
                .take(8)
                .collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            if tail.is_empty() {
                bail!("{what} failed ({}) — no error output", st);
            }
            bail!("{what} failed ({}):\n{}", st, tail.join("\n"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn z_path_maps_slashes() {
        assert_eq!(to_z_path(Path::new("/home/x/d.xml")), r"Z:\home\x\d.xml");
    }

    // Every catalog app's exe is found where Adobe puts it. Lightroom (experimental) is the
    // case that broke: its folder is "Adobe Lightroom CC", not "Adobe " + its display name.
    #[test]
    fn finds_every_catalog_exe_in_its_own_folder() {
        for cat in crate::catalog::apps() {
            let prefix = std::env::temp_dir().join(format!("mudhut-exe-{}-{}", cat.id, std::process::id()));
            let exe = prefix
                .join("drive_c/Program Files/Adobe")
                .join(format!("{} 2026", cat.dir_prefix))
                .join(cat.exe);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"MZ").unwrap();
            assert_eq!(find_installed_exe(&prefix, &cat), Some(exe.clone()), "{}", cat.id);
            let _ = std::fs::remove_dir_all(&prefix);
        }
    }
}
