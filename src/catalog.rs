// SPDX-License-Identifier: Apache-2.0
//! The Adobe app catalog: the set of apps Mud Hut knows how to install.
//!
//! P1 keeps this compiled-in. The plan (per mud_hut_suggestions.md) is to later
//! source it from a remote versioned JSON ledger so SAP codes / directory names
//! can be hotfixed without a rebuild — `apps()` is the single seam that would
//! swap to a ledger fetch.

use std::path::Path;

use serde::Serialize;

use crate::output::Emitter;
use crate::source;

/// One installable Adobe application.
#[derive(Clone, Serialize)]
pub struct App {
    /// Stable Mud Hut / Neutron / Collider id (matches neutron's APP_PROFILES).
    pub id: &'static str,
    /// Human name.
    pub name: &'static str,
    /// Adobe's internal 4-letter SAP tracking code (used by the P3 downloader).
    pub sap: &'static str,
    /// Prefix of the app's directory under `Program Files/Adobe/` — matched as a
    /// glob-ish prefix so a version bump (2024 vs 2025) still resolves.
    pub dir_prefix: &'static str,
}

/// The compiled-in catalog. Order = display order.
pub fn apps() -> Vec<App> {
    vec![
        App { id: "photoshop",    name: "Photoshop",         sap: "PHSP", dir_prefix: "Adobe Photoshop" },
        App { id: "premiere",     name: "Premiere Pro",      sap: "PPRO", dir_prefix: "Adobe Premiere Pro" },
        App { id: "aftereffects", name: "After Effects",     sap: "AEFT", dir_prefix: "Adobe After Effects" },
        App { id: "illustrator",  name: "Illustrator",       sap: "ILST", dir_prefix: "Adobe Illustrator" },
        App { id: "animate",      name: "Animate",           sap: "FLPR", dir_prefix: "Adobe Animate" },
        App { id: "lightroom",    name: "Lightroom Classic", sap: "LTRM", dir_prefix: "Adobe Lightroom Classic" },
        App { id: "mediaencoder", name: "Media Encoder",     sap: "AME",  dir_prefix: "Adobe Media Encoder" },
    ]
}

/// Look up one app by its id.
pub fn find(id: &str) -> Option<App> {
    apps().into_iter().find(|a| a.id == id)
}

/// `mudhut apps [--source DIR]` — list the catalog, and if a source is given,
/// mark which apps are actually present there (with the resolved directory).
pub fn cmd_apps(em: &Emitter, source: Option<&Path>) -> anyhow::Result<()> {
    let src = match source {
        Some(s) => Some(source::Source::discover(s)?),
        None => None,
    };

    #[derive(Serialize)]
    struct Row {
        id: &'static str,
        name: &'static str,
        sap: &'static str,
        present: bool,
        dir: Option<String>,
    }
    #[derive(Serialize)]
    struct Out {
        source: Option<String>,
        apps: Vec<Row>,
    }

    let rows: Vec<Row> = apps()
        .into_iter()
        .map(|a| {
            let dir = src.as_ref().and_then(|s| s.app_dir(&a));
            Row {
                id: a.id,
                name: a.name,
                sap: a.sap,
                present: dir.is_some(),
                dir: dir.map(|p| p.display().to_string()),
            }
        })
        .collect();

    let out = Out {
        source: src.as_ref().map(|s| s.root().display().to_string()),
        apps: rows,
    };

    if em.is_json() {
        em.result(&out);
    } else {
        match &out.source {
            Some(s) => println!("Adobe apps in {s}:"),
            None => println!("Installable Adobe apps:"),
        }
        for r in &out.apps {
            let mark = if source.is_none() {
                " "
            } else if r.present {
                "✓"
            } else {
                "·"
            };
            println!("  {mark} {:<16} {:<18} [{}]", r.id, r.name, r.sap);
        }
    }
    Ok(())
}
