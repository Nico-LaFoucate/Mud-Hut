// SPDX-License-Identifier: Apache-2.0
//! Mount a disk image (ISO) read-only, so an offline package can be installed
//! straight off it — no extraction, no multi-GiB copy.
//!
//! Uses `udisksctl` (udisks2), which sets up the loop device and mounts it as
//! the logged-in user: no root, no sudo, no `/etc/fstab` entry. Everything is
//! torn down again by [`Mounted`]'s `Drop`, including on the error paths.
//!
//! The mount is forced read-only (`loop-setup -r`). That is both correct for
//! read-only media and the case Mud Hut already handles: `offline.rs` sees a
//! non-writable package and writes its driver XML to a scratch dir with
//! absolute `<EsdDirectory>` paths.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// A mounted image. Unmounts and deletes the loop device when dropped.
pub struct Mounted {
    pub mountpoint: PathBuf,
    loop_dev: String,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Best-effort teardown: a failure here must not mask the real result.
        let _ = udisks(&["unmount", "-b", &self.loop_dev]);
        let _ = udisks(&["loop-delete", "-b", &self.loop_dev]);
        let _ = std::fs::remove_file(record_path(&self.loop_dev));
    }
}

/// Where mounts are recorded so they can be reclaimed after an abnormal exit.
fn records_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("mudhut-mounts")
}

fn record_path(loop_dev: &str) -> PathBuf {
    records_dir().join(loop_dev.replace('/', "_"))
}

/// Reclaim mounts left behind by a mudhut that died without running `Drop`.
///
/// Drop covers a normal exit and an error return, but NOTHING runs on SIGKILL,
/// and Rust does not run destructors on a default SIGTERM either — so a killed
/// or crashed run leaves the image mounted and the loop device allocated. A
/// signal handler cannot fix the SIGKILL case at all, which is why this is a
/// recorded-state sweep instead: it reclaims after any death, including a power
/// loss, and is async-signal-safe by construction because there is no handler.
///
/// Called at startup. Cheap: a directory read, and a liveness check per record.
pub fn sweep_stale() {
    let Ok(rd) = std::fs::read_dir(records_dir()) else { return };
    for e in rd.flatten() {
        let path = e.path();
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let mut lines = body.lines();
        let (Some(pid), Some(loop_dev)) = (lines.next(), lines.next()) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        // Still owned by a LIVE mudhut? Leave it alone. The comm check guards
        // against pid reuse handing us an unrelated process.
        let owner_alive = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| c.trim() == "mudhut")
            .unwrap_or(false);
        if owner_alive {
            continue;
        }
        let _ = udisks(&["unmount", "-b", loop_dev]);
        let _ = udisks(&["loop-delete", "-b", loop_dev]);
        let _ = std::fs::remove_file(&path);
    }
}

fn record(loop_dev: &str, mountpoint: &Path) {
    let dir = records_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return; // recording is best-effort; never block a mount on it
    }
    let _ = std::fs::write(
        record_path(loop_dev),
        format!("{}\n{}\n{}\n", std::process::id(), loop_dev, mountpoint.display()),
    );
}

/// Whether `p` looks like a disk image we can mount.
pub fn is_image(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("iso") | Some("img")
    )
}

fn udisks(args: &[&str]) -> Result<String> {
    let out = Command::new("udisksctl")
        .args(args)
        .output()
        .context("running `udisksctl` — install udisks2 to mount an ISO directly")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("udisksctl {} failed: {}", args.join(" "), err.trim());
    }
    Ok(stdout)
}

/// Text after the last `" <sep> "`, with a trailing period stripped. udisksctl
/// prints `Mapped file X as /dev/loop0.` / `Mounted /dev/loop0 at /run/media/...`.
fn after(s: &str, sep: &str) -> Option<String> {
    let i = s.rfind(sep)? + sep.len();
    Some(s[i..].trim().trim_end_matches('.').to_string())
}

/// Mount `image` read-only. The returned guard unmounts on drop.
pub fn mount(image: &Path) -> Result<Mounted> {
    let img = image.canonicalize().unwrap_or_else(|_| image.to_path_buf());
    let img = img.to_string_lossy().to_string();

    let out = udisks(&["loop-setup", "-r", "-f", &img])?;
    let loop_dev = after(&out, " as ")
        .filter(|d| d.starts_with("/dev/"))
        .with_context(|| format!("could not read the loop device from udisksctl: {out:?}"))?;

    // From here on the loop device exists, so any early return must clean it up.
    let mount_out = match udisks(&["mount", "-b", &loop_dev]) {
        Ok(o) => o,
        Err(e) => {
            // Some desktops auto-mount the moment the loop device appears; that
            // is a success, not a failure. Ask where it landed before giving up.
            match findmnt(&loop_dev) {
                Some(mp) => {
                    record(&loop_dev, &mp);
                    return Ok(Mounted { mountpoint: mp, loop_dev });
                }
                None => {
                    let _ = udisks(&["loop-delete", "-b", &loop_dev]);
                    return Err(e).with_context(|| format!("mounting {img}"));
                }
            }
        }
    };

    let mountpoint = after(&mount_out, " at ")
        .map(PathBuf::from)
        .or_else(|| findmnt(&loop_dev))
        .with_context(|| format!("could not read the mountpoint from udisksctl: {mount_out:?}"))?;

    record(&loop_dev, &mountpoint);
    Ok(Mounted { mountpoint, loop_dev })
}

/// Where `dev` is currently mounted, if anywhere.
fn findmnt(dev: &str) -> Option<PathBuf> {
    let out = Command::new("findmnt").args(["-n", "-o", "TARGET", "--source", dev]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let first = s.lines().next()?.trim();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// Resolve a `--source` that may be a disk image.
///
/// A directory passes straight through. An image is mounted read-only and its
/// mountpoint returned, together with a guard that MUST be held for as long as
/// the path is used — dropping it unmounts.
pub fn resolve_source(source: &Path) -> Result<(PathBuf, Option<Mounted>)> {
    if source.is_dir() {
        return Ok((source.to_path_buf(), None));
    }
    if !source.exists() {
        bail!("--source not found: {}", source.display());
    }
    if !is_image(source) {
        bail!(
            "--source is a file Mud Hut cannot mount: {}\n\
             Expected a directory, or a disk image (.iso/.img).",
            source.display()
        );
    }
    let m = mount(source)?;
    Ok((m.mountpoint.clone(), Some(m)))
}
