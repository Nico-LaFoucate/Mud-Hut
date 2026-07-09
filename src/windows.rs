// SPDX-License-Identifier: Apache-2.0
//! `--method windows`: acquire by copying from an existing Windows install.
//!
//! Discovers the target app(s) + the shared Adobe runtime in the source tree and
//! returns them as an [`Acquisition`]; the shared `install::run` pipeline does the
//! staging + provisioning. No scratch dir — it stages straight from the source.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::catalog::{self, App};
use crate::install::Acquisition;
use crate::source::Source;

pub fn acquire(source: Option<&Path>, app: Option<&str>, suite: bool) -> Result<Acquisition> {
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

    // The app dir(s) + the shared runtime, de-duplicated, relative to the source base.
    let base = src.root().to_path_buf();
    let mut item_srcs: Vec<PathBuf> = targets.iter().map(|(_, d)| d.clone()).collect();
    item_srcs.extend(src.shared_paths());
    item_srcs.sort();
    item_srcs.dedup();

    let app_ids = targets.iter().map(|(a, _)| a.id.to_string()).collect();

    Ok(Acquisition {
        source_desc: base.display().to_string(),
        base,
        item_srcs,
        app_ids,
        scratch: None,
    })
}
