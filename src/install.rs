// SPDX-License-Identifier: Apache-2.0
//! The install pipeline: **acquire -> stage -> provision**.
//!
//! Every ingestion method (`windows`, `download`, `offline`) produces the same
//! thing — an [`Acquisition`]: a base directory plus the set of source paths to
//! copy into `<prefix>/drive_c/...`, preserving the Windows layout. This module
//! owns the shared half — planning, staging (idempotent copy), and the handoff
//! to `neutron prefix provision` — so the methods only differ in how they get
//! the bits onto disk. Idempotent (files already present with the same size are
//! skipped) so a re-run repairs a partial install rather than corrupting it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use walkdir::WalkDir;

use crate::output::Emitter;

/// The product of the `acquire` step: bits on disk, ready to stage into a prefix.
/// Methods build this differently (copy discovery / download+unpack / extract)
/// but the [`run`] pipeline treats them identically.
pub struct Acquisition {
    /// Directory the item paths are relative to; `item_srcs` map to
    /// `<prefix>/drive_c/<path relative to base>`.
    pub base: PathBuf,
    /// Absolute source paths (dirs or files) to stage under `drive_c`.
    pub item_srcs: Vec<PathBuf>,
    /// App ids being installed (for reporting / the result event).
    pub app_ids: Vec<String>,
    /// Human/JSON description of where the bits came from (source path, "Adobe
    /// download", offline package path, ...).
    pub source_desc: String,
    /// A scratch dir (download/unpack or offline extraction) to remove once
    /// staging is done. `None` for `windows` (it copies straight from the source).
    pub scratch: Option<PathBuf>,
}

#[derive(Serialize)]
struct PlanItem {
    rel: String,
    bytes: u64,
    #[serde(skip)]
    src: PathBuf,
    #[serde(skip)]
    dst: PathBuf,
}

#[derive(Serialize)]
struct InstallResult {
    ok: bool,
    dry_run: bool,
    prefix: String,
    source: String,
    apps: Vec<String>,
    items: Vec<PlanItem>,
    total_bytes: u64,
    provisioned: bool,
}

/// Stage an [`Acquisition`] into `prefix` and provision it. Shared by every
/// ingestion method. Always removes the acquisition's scratch dir on the way
/// out (success or failure).
pub fn run(em: &Emitter, prefix: &Path, acq: Acquisition, dry_run: bool) -> Result<()> {
    let scratch = acq.scratch.clone();
    let r = run_inner(em, prefix, acq, dry_run);
    if let Some(s) = scratch {
        let _ = fs::remove_dir_all(&s);
    }
    r
}

fn run_inner(em: &Emitter, prefix: &Path, acq: Acquisition, dry_run: bool) -> Result<()> {
    if acq.item_srcs.is_empty() {
        bail!("nothing to install (no items acquired)");
    }

    // Build the staging plan: each source path relative to the acquisition base.
    let mut items = Vec::new();
    let mut total_bytes = 0u64;
    for s in &acq.item_srcs {
        let rel = s.strip_prefix(&acq.base).unwrap_or(s);
        let dst = prefix.join("drive_c").join(rel);
        let bytes = dir_size(s);
        total_bytes += bytes;
        items.push(PlanItem { rel: rel.display().to_string(), bytes, src: s.clone(), dst });
    }

    em.note(&format!(
        "{} app(s): {} — {} item(s), {:.1} GiB from {}",
        acq.app_ids.len(),
        acq.app_ids.join(", "),
        items.len(),
        total_bytes as f64 / (1u64 << 30) as f64,
        acq.source_desc,
    ));

    if dry_run {
        for it in &items {
            em.note(&format!(
                "  would copy {:.2} GiB  {}",
                it.bytes as f64 / (1u64 << 30) as f64,
                it.rel
            ));
        }
        return finish(em, InstallResult {
            ok: true, dry_run: true, prefix: prefix.display().to_string(),
            source: acq.source_desc, apps: acq.app_ids, items, total_bytes,
            provisioned: false,
        });
    }

    // Stage into drive_c preserving the Windows layout.
    fs::create_dir_all(prefix.join("drive_c"))
        .with_context(|| format!("cannot create prefix at {}", prefix.display()))?;

    let mut copied = 0u64;
    let mut last_pct = u8::MAX;
    for it in &items {
        em.progress("copy", pct(copied, total_bytes), &it.rel);
        copy_tree(&it.src, &it.dst, &it.rel, &mut copied, total_bytes, &mut last_pct, em)
            .with_context(|| format!("copying {}", it.rel))?;
    }
    em.progress("copy", 100, "staged");

    // Hand off to Neutron to make the prefix runnable (single source of truth).
    em.progress("provision", 100, "neutron prefix provision");
    provision(prefix).context("neutron prefix provision failed")?;

    finish(em, InstallResult {
        ok: true, dry_run: false, prefix: prefix.display().to_string(),
        source: acq.source_desc, apps: acq.app_ids, items, total_bytes,
        provisioned: true,
    })
}

fn finish(em: &Emitter, r: InstallResult) -> Result<()> {
    if em.is_json() {
        em.result(&r);
    } else if r.dry_run {
        println!("Dry run: {} app(s), {:.1} GiB planned into {}",
            r.apps.len(), r.total_bytes as f64 / (1u64 << 30) as f64, r.prefix);
    } else {
        println!("Installed {} into {} ({}).",
            r.apps.join(", "), r.prefix,
            if r.provisioned { "provisioned" } else { "NOT provisioned" });
    }
    Ok(())
}

/// Run `neutron prefix provision <prefix>`, inheriting output.
fn provision(prefix: &Path) -> Result<()> {
    let status = Command::new("neutron")
        .arg("prefix")
        .arg("provision")
        .arg(prefix)
        .status()
        .context("could not run `neutron` (is the Neutron runtime installed?)")?;
    if !status.success() {
        bail!("`neutron prefix provision` exited with {status}");
    }
    Ok(())
}

fn pct(done: u64, total: u64) -> u8 {
    if total == 0 {
        100
    } else {
        ((done.min(total) as f64 / total as f64) * 100.0) as u8
    }
}

fn dir_size(root: &Path) -> u64 {
    WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Recursively copy `src` -> `dst`, skipping files that already exist with the
/// same size (idempotent), streaming byte-based progress under `label`. Handles
/// `src` being a single file as well as a directory tree.
fn copy_tree(
    src: &Path,
    dst: &Path,
    label: &str,
    copied: &mut u64,
    total: u64,
    last_pct: &mut u8,
    em: &Emitter,
) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = if rel.as_os_str().is_empty() { dst.to_path_buf() } else { dst.join(rel) };
        let ft = entry.file_type();
        if ft.is_dir() {
            fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            let len = entry.metadata()?.len();
            let already = fs::metadata(&target).map(|m| m.len() == len).unwrap_or(false);
            if !already {
                if let Some(p) = target.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(entry.path(), &target)
                    .with_context(|| format!("copy {}", entry.path().display()))?;
            }
            *copied += len;
            let p = pct(*copied, total);
            if p != *last_pct {
                *last_pct = p;
                em.progress("copy", p, label);
            }
        }
        // symlinks and other node types are ignored (Adobe Windows trees have none)
    }
    Ok(())
}
