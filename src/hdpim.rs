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
use std::process::Command;

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
/// `neutron runtime which` reports, else the newest numeric Neutron runtime, else the dev tree.
///
/// Pass the target prefix whenever it is known: installing into an existing prefix with
/// a mismatched wine wineboot-clobbers its patched natives.
pub fn discover(repo_tools: &Path, accc_packages: PathBuf, prefix: Option<&Path>) -> Result<Config> {
    let wine = resolve_wine(prefix).context(
        "no wine found — set $MUDHUT_WINE or install the Neutron runtime (`neutron runtime install`)",
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
/// shape ever changes this returns None and resolution falls through to the old behaviour, which
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
    //    Same rule as neutron's preserved-fixes/harnesses/runtime_for_prefix.sh.
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
    //    NEVER SHIP (RUNTIMES.md) to run against a prefix stamped for 11.10-45.
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
    // 3. Neutron dev build tree — mirrors neutron's own NEUTRON_WINE_CANDIDATES so a
    //    dev machine (no runtime release installed yet) resolves the same wine that
    //    neutron launches the app under. Superseded by (2) once a runtime is installed.
    if let Some(home) = &home {
        for rel in [
            "neutron/dist/wine/bin/wine",
            "wine-tkg-git/wine-tkg-git/src/wine-tkg-staging-ntsync-git-64-build/wine",
        ] {
            let p = home.join(rel);
            if p.is_file() {
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
    app_name: &str, // e.g. "Photoshop" -> dir "Adobe Photoshop <year>", exe "<App>.exe"
    driver_xml: &Path,
    packages_dir: &Path,
    dry_run: bool,
) -> Result<PathBuf> {
    if dry_run {
        em.note(&format!(
            "would install {app_name} into {} via HDPIM (prereqs + ACC seed + hdpim_host)",
            prefix.display()
        ));
        return Ok(prefix.join("drive_c/Program Files/Adobe").join(format!("Adobe {app_name} <year>")));
    }

    em.progress("prereqs", 5, "prefix init + Win11 spoof + VC++ runtime");
    setup_prefix(em, cfg, prefix)?;

    em.progress("seed", 25, "ACC runtime (HDBox/HDPIM + ESD engine)");
    seed_runtime(em, cfg, prefix)?;

    em.progress("install", 45, "HDPIM decrypt + install (this takes minutes)");
    let exe = run_hdpim(em, cfg, prefix, app_name, driver_xml, packages_dir)?;
    em.progress("verify", 95, "checking the decrypted binary");
    verify(&exe)?;
    em.progress("install", 100, "installed");
    Ok(exe)
}

/// Find the installed app exe under `Program Files\Adobe\Adobe <app_name> <year>\`.
/// Returns the highest-year match that exists.
fn find_installed_exe(prefix: &Path, app_name: &str) -> Option<PathBuf> {
    let adobe = prefix.join("drive_c/Program Files/Adobe");
    let dir_prefix = format!("Adobe {app_name}");
    let exe_name = format!("{}.exe", app_name.split_whitespace().last().unwrap_or(app_name));
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&adobe)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&dir_prefix))
                .unwrap_or(false)
        })
        .map(|d| d.join(&exe_name))
        .filter(|p| p.is_file())
        .collect();
    hits.sort();
    hits.pop()
}

/// wineboot (mono/gecko dialog suppressed) + Win11 24H2 spoof + VC++ redists.
fn setup_prefix(em: &Emitter, cfg: &Config, prefix: &Path) -> Result<()> {
    // Init: suppress the interactive Mono/Gecko installer dialog (it blocks headless).
    wine(cfg, prefix, &["wineboot", "--init"])
        .env("WINEDLLOVERRIDES", "mscoree,mshtml=d")
        .status_ok("wineboot --init")?;

    // Win11 24H2 spoof (HDPIM gates on the OS version; see methodology §4).
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

    // VC++ runtimes (Adobe's native C++ needs the real redists; wine builtins crash).
    if which("winetricks").is_some() {
        em.progress("prereqs", 15, "winetricks vcrun2022 + vcrun2013");
        let mut c = Command::new("winetricks");
        c.args(["-q", "-f", "vcrun2022", "vcrun2013"])
            .env("WINE", &cfg.wine)
            .env("WINEPREFIX", prefix)
            .env("WINEDEBUG", "-all")
            .env("W_OPT_UNATTENDED", "1");
        let _ = c.status(); // best-effort; a missing redist surfaces at launch
    } else {
        em.note("winetricks not found — VC++ redists not installed (app may crash at launch)");
    }
    // flush the registry cleanly
    let _ = wineserver(cfg, prefix, "-w");
    Ok(())
}

/// Seed Adobe Desktop Common (HDBox/HDPIM + ESD engine + ADS/IPCBroker) from the
/// public ACCCx `.pima`. `--sets ADC,ADC64` is the install-leg runtime.
fn seed_runtime(em: &Emitter, cfg: &Config, prefix: &Path) -> Result<()> {
    let out = Command::new("python3")
        .arg(&cfg.extractor)
        .arg("--packages")
        .arg(&cfg.accc_packages)
        .arg("--prefix")
        .arg(prefix)
        .arg("--sets")
        .arg("ADC,ADC64")
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
    app_name: &str,
    driver_xml: &Path,
    packages_dir: &Path,
) -> Result<PathBuf> {
    use std::time::{Duration, Instant};
    // hdpim_host resolves EsdDirectory relative to its CWD (the driver.xml dir).
    // HDPIM's install runs async and the host otherwise pumps its full wait; we stop
    // it EARLY once the decrypted PE appears, so the CLI doesn't idle for ~30 min.
    let driver_win = to_z_path(driver_xml);
    let mut child = wine(cfg, prefix, &[])
        .arg(&cfg.hdpim_host)
        .arg(HDPIM_WIN)
        .arg(&driver_win)
        .arg("2400") // pump-second ceiling
        .current_dir(packages_dir)
        .env("WINEDLLOVERRIDES", "mshtml=d")
        .spawn()
        .context("spawning hdpim_host")?;

    let deadline = Instant::now() + Duration::from_secs(2400);
    let exe = loop {
        if let Some(status) = child.try_wait().context("polling hdpim_host")? {
            // Host exited on its own — the install must have produced the exe.
            break find_installed_exe(prefix, app_name).with_context(|| {
                format!("hdpim_host exited ({status}) but no installed '{app_name}' exe found")
            })?;
        }
        if let Some(exe) = find_installed_exe(prefix, app_name) {
            if verify(&exe).is_ok() {
                em.progress("install", 90, "installed — stopping host");
                let _ = child.kill();
                let _ = child.wait();
                break exe;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("HDPIM install timed out (no decrypted exe after 2400s)");
        }
        std::thread::sleep(Duration::from_secs(10));
    };
    let _ = wineserver(cfg, prefix, "-k");
    Ok(exe)
}

/// The install succeeded only if the app exe is a real decrypted PE (not the ~1 MB
/// encrypted stub) — judge by size + the `MZ` header, per the methodology.
fn verify(exe: &Path) -> Result<()> {
    let meta = std::fs::metadata(exe)
        .with_context(|| format!("installed exe not found: {}", exe.display()))?;
    if meta.len() < 50_000_000 {
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

fn wineserver(cfg: &Config, prefix: &Path, flag: &str) -> std::io::Result<std::process::ExitStatus> {
    // wineserver sits next to wine.
    let ws = cfg.wine.with_file_name("wineserver");
    let ws = if ws.is_file() { ws } else { cfg.wine.with_file_name("server").join("wineserver") };
    Command::new(ws).arg(flag).env("WINEPREFIX", prefix).status()
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| p.is_file())
    })
}

/// Map an absolute Linux path to a Wine `Z:` Windows path.
fn to_z_path(p: &Path) -> String {
    format!("Z:{}", p.to_string_lossy().replace('/', "\\"))
}

// Small extension trait so the call sites read cleanly.
trait CommandExt {
    fn status_ok(&mut self, what: &str) -> Result<()>;
}
impl CommandExt for Command {
    fn status_ok(&mut self, what: &str) -> Result<()> {
        let st = self.status().with_context(|| format!("spawning {what}"))?;
        if !st.success() {
            bail!("{what} failed ({st})");
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
}
