// SPDX-License-Identifier: Apache-2.0
//! P1 ingestion: copy from an existing Windows install.
//!
//! Discovers the app(s) + shared Adobe runtime in the source, stages them into
//! `<prefix>/drive_c/...` preserving the Windows layout, then hands off to
//! `neutron prefix provision` to make the prefix Neutron-ready. Idempotent
//! (files already present with the same size are skipped) so a re-run repairs a
//! partial copy rather than corrupting it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use walkdir::WalkDir;

use crate::catalog::{self, App};
use crate::output::Emitter;
use crate::source::Source;

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

pub fn from_windows(
    em: &Emitter,
    prefix: &Path,
    source: Option<&Path>,
    app: Option<&str>,
    suite: bool,
    dry_run: bool,
) -> Result<()> {
    let source = source.context("--source is required for --method windows")?;
    let src = Source::discover(source)?;

    // Resolve the target apps.
    let targets: Vec<(App, PathBuf)> = if suite {
        catalog::apps()
            .into_iter()
            .filter_map(|a| src.app_dir(&a).map(|d| (a, d)))
            .collect()
    } else {
        let id = app.context("provide an app id (e.g. photoshop) or pass --suite")?;
        let a = catalog::find(id).with_context(|| format!("unknown app id: {id}"))?;
        let dir = src
            .app_dir(&a)
            .with_context(|| format!("{} not found in {}", a.name, src.root().display()))?;
        vec![(a, dir)]
    };
    if targets.is_empty() {
        bail!("no matching Adobe apps found in {}", src.root().display());
    }

    // Build the staging plan: the app dir(s) + the shared runtime, de-duplicated.
    let base = src.root().to_path_buf();
    let mut item_srcs: Vec<PathBuf> = targets.iter().map(|(_, d)| d.clone()).collect();
    item_srcs.extend(src.shared_paths());
    item_srcs.sort();
    item_srcs.dedup();

    let mut items = Vec::new();
    let mut total_bytes = 0u64;
    for s in &item_srcs {
        let rel = s.strip_prefix(&base).unwrap_or(s);
        let dst = prefix.join("drive_c").join(rel);
        let bytes = dir_size(s);
        total_bytes += bytes;
        items.push(PlanItem { rel: rel.display().to_string(), bytes, src: s.clone(), dst });
    }

    let app_ids: Vec<String> = targets.iter().map(|(a, _)| a.id.to_string()).collect();
    em.note(&format!(
        "{} app(s): {} — {} item(s), {:.1} GiB from {}",
        targets.len(),
        app_ids.join(", "),
        items.len(),
        total_bytes as f64 / (1u64 << 30) as f64,
        base.display()
    ));

    if dry_run {
        for it in &items {
            em.note(&format!("  would copy {:.2} GiB  {}", it.bytes as f64 / (1u64 << 30) as f64, it.rel));
        }
        return finish(em, InstallResult {
            ok: true, dry_run: true, prefix: prefix.display().to_string(),
            source: base.display().to_string(), apps: app_ids, items, total_bytes,
            provisioned: false,
        });
    }

    // Real install. Stage into drive_c preserving the Windows layout.
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
        source: base.display().to_string(), apps: app_ids, items, total_bytes,
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
/// same size (idempotent), streaming byte-based progress under `label`.
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
        let target = dst.join(rel);
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
