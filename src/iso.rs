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
    }
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
                Some(mp) => return Ok(Mounted { mountpoint: mp, loop_dev }),
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
