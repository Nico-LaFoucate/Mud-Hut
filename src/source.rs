// SPDX-License-Identifier: Apache-2.0
//! Windows-install source discovery for the `windows` ingestion method.
//!
//! A "source" is any tree that contains `Program Files/Adobe/...` — a Wine
//! `drive_c`, a mounted Windows `C:`, or a hand-copied dump. Discovery resolves
//! the base that holds `Program Files`, then locates per-app directories and the
//! shared Adobe runtime that every app needs.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::catalog::App;

pub struct Source {
    base: PathBuf, // directory that contains "Program Files" (+ "ProgramData")
}

impl Source {
    /// Resolve a source root. Accepts the tree itself, or one with a `drive_c`
    /// (or `c`) sub-dir, as long as `Program Files/Adobe` lives under it.
    pub fn discover(root: &Path) -> Result<Source> {
        if !root.exists() {
            bail!("source does not exist: {}", root.display());
        }
        let candidates = [
            root.to_path_buf(),
            root.join("drive_c"),
            root.join("c"),
            root.join("C:"),
        ];
        for base in candidates {
            if find_child(&base, "Program Files")
                .and_then(|pf| find_child(&pf, "Adobe"))
                .is_some()
            {
                return Ok(Source { base });
            }
        }
        bail!(
            "no 'Program Files/Adobe' found under {} (point --source at a Windows drive_c / C:)",
            root.display()
        );
    }

    pub fn root(&self) -> &Path {
        &self.base
    }

    fn program_files_adobe(&self) -> Option<PathBuf> {
        find_child(&self.base, "Program Files").and_then(|pf| find_child(&pf, "Adobe"))
    }

    /// The installed directory for `app`, if present (e.g. `.../Adobe Photoshop 2025`).
    pub fn app_dir(&self, app: &App) -> Option<PathBuf> {
        let pfa = self.program_files_adobe()?;
        let entries = std::fs::read_dir(&pfa).ok()?;
        for e in entries.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(app.dir_prefix) {
                return Some(e.path());
            }
        }
        None
    }

    /// The shared Adobe runtime + app-data directories that exist in this source.
    /// Every one that's present is required for the apps to run and gets staged.
    pub fn shared_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        // Common Files/Adobe under both Program Files trees.
        for pf in ["Program Files", "Program Files (x86)"] {
            if let Some(p) = find_child(&self.base, pf)
                .and_then(|p| find_child(&p, "Common Files"))
                .and_then(|p| find_child(&p, "Adobe"))
            {
                out.push(p);
            }
        }
        // Shared "Common" bundle that sits beside the apps.
        if let Some(p) = self.program_files_adobe().and_then(|p| find_child(&p, "Common")) {
            out.push(p);
        }
        // Machine-wide app data (Camera Raw, Lightroom presets, PINF, ...).
        if let Some(p) = find_child(&self.base, "ProgramData").and_then(|p| find_child(&p, "Adobe")) {
            out.push(p);
        }
        out
    }
}

/// Case-insensitive single-level child lookup (Windows trees copied onto a
/// case-sensitive Linux FS may vary in case).
fn find_child(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        return Some(exact);
    }
    let want = name.to_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase() == want)
                .unwrap_or(false)
        })
}
