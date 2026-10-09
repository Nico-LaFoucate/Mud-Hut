// SPDX-License-Identifier: Apache-2.0
//! The Adobe app catalog: the set of apps Mud Hut knows how to install.
//!
//! Compiled in. `apps()` is the single seam a remote ledger fetch would replace,
//! if SAP codes / directory names ever need fixing without a rebuild.

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
    /// Adobe's 4-letter SAP code (the product feed and the package layout key on it).
    pub sap: &'static str,
    /// Prefix of the app's directory under `Program Files/Adobe/` — matched as a
    /// glob-ish prefix so a version bump (2024 vs 2025) still resolves.
    pub dir_prefix: &'static str,
    /// The application's main executable, relative to its directory under
    /// `Program Files/Adobe/`.
    ///
    /// ⛔ NOT derivable from the name, and not always at the top level. After
    /// Effects ships `Support Files/AfterFX.exe`; Illustrator is three levels down
    /// in `Support Files/Contents/Windows/`. Guessing this from the app name is
    /// what made a finished After Effects install look like a decrypt that hung.
    pub exe: &'static str,
    /// Freedesktop `Categories=` — informational, exported in `mudhut apps` JSON. Mud Hut
    /// writes no `.desktop` launcher of its own (2026-09-18): `neutron prefix provision` owns
    /// launchers, icons and file associations, one entry per app per prefix.
    pub categories: &'static str,
    /// MIME types the app opens — informational, exported in `mudhut apps` JSON. The engine's
    /// own `APP_MIME_TYPES` (and its `neutron-adobe.xml` package) decide what a launcher claims.
    pub mime: &'static [&'static str],
}

/// The compiled-in catalog. Order = display order.
pub fn apps() -> Vec<App> {
    vec![
        App { id: "photoshop",    name: "Photoshop",         sap: "PHSP", dir_prefix: "Adobe Photoshop",
              exe: "Photoshop.exe", categories: "Graphics;Photography;RasterGraphics;2DGraphics;",
              mime: &["image/vnd.adobe.photoshop"] },
        App { id: "premiere",     name: "Premiere Pro",      sap: "PPRO", dir_prefix: "Adobe Premiere Pro",
              exe: "Adobe Premiere Pro.exe", categories: "AudioVideo;Video;AudioVideoEditing;", mime: &[] },
        App { id: "aftereffects", name: "After Effects",     sap: "AEFT", dir_prefix: "Adobe After Effects",
              exe: "Support Files/AfterFX.exe", categories: "AudioVideo;Video;AudioVideoEditing;", mime: &[] },
        App { id: "illustrator",  name: "Illustrator",       sap: "ILST", dir_prefix: "Adobe Illustrator",
              exe: "Support Files/Contents/Windows/Illustrator.exe", categories: "Graphics;VectorGraphics;2DGraphics;", mime: &["application/illustrator"] },
        App { id: "animate",      name: "Animate",           sap: "FLPR", dir_prefix: "Adobe Animate",
              exe: "Animate.exe", categories: "Graphics;2DGraphics;", mime: &[] },
        App { id: "lightroom",    name: "Lightroom Classic", sap: "LTRM", dir_prefix: "Adobe Lightroom Classic",
              exe: "Lightroom.exe", categories: "Graphics;Photography;", mime: &[] },
        App { id: "mediaencoder", name: "Media Encoder",     sap: "AME",  dir_prefix: "Adobe Media Encoder",
              exe: "Adobe Media Encoder.exe", categories: "AudioVideo;Video;AudioVideoEditing;", mime: &[] },
        // EXPERIMENTAL: Lightroom (the cloud app) is planned, not supported yet. Listed so people
        // can try it; the engine has a matching `lightroomcc` launch profile.
        App { id: "lightroomcc",  name: "Lightroom (experimental)", sap: "LRCC", dir_prefix: "Adobe Lightroom CC",
              exe: "lightroom.exe", categories: "Graphics;Photography;", mime: &[] },
    ]
}

/// Look up one app by its id.
pub fn find(id: &str) -> Option<App> {
    apps().into_iter().find(|a| a.id == id)
}

/// What a `--source` dir turned out to be: a Windows install tree, or an
/// offline package (Adobe's ESD `<SAP>/Application.json` products layout).
enum Found {
    Windows(source::Source),
    Offline(std::path::PathBuf),
}

/// `mudhut apps [--source DIR]` — list the catalog, and if a source is given,
/// mark which apps are actually present there (with the resolved directory).
/// The source may be a Windows install root OR an offline package dir; the
/// result carries `source_kind` so a UI can tell which one it found.
pub fn cmd_apps(em: &Emitter, source: Option<&Path>) -> anyhow::Result<()> {
    // An .iso/.img source is mounted read-only here, so the scan Collider runs
    // when you pick a source accepts an ISO exactly like a folder. `_mount` is
    // the teardown guard and must outlive every use of the resolved path below.
    let (resolved, _mount) = match source {
        Some(s) => {
            let (root, m) = crate::iso::resolve_source(s)?;
            (Some(root), m)
        }
        None => (None, None),
    };

    let src = match resolved.as_deref() {
        Some(s) => Some(match source::Source::discover(s) {
            Ok(w) => Found::Windows(w),
            Err(_) => match crate::offline::products_dir(s) {
                Some(p) => Found::Offline(p),
                None => anyhow::bail!(
                    "{} is neither a Windows install root (no Program Files/Adobe) \
                     nor an offline package (no <SAP>/Application.json)",
                    source.unwrap_or(s).display()
                ),
            },
        }),
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
        source_kind: Option<&'static str>,
        apps: Vec<Row>,
    }

    let rows: Vec<Row> = apps()
        .into_iter()
        .map(|a| {
            let dir = match &src {
                Some(Found::Windows(s)) => s.app_dir(&a),
                Some(Found::Offline(p)) => {
                    let d = p.join(a.sap);
                    d.join("Application.json").is_file().then_some(d)
                }
                None => None,
            };
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
        source: src.as_ref().map(|s| match s {
            Found::Windows(w) => w.root().display().to_string(),
            Found::Offline(p) => p.display().to_string(),
        }),
        source_kind: src.as_ref().map(|s| match s {
            Found::Windows(_) => "windows",
            Found::Offline(_) => "offline",
        }),
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
