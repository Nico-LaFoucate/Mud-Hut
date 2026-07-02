// SPDX-License-Identifier: Apache-2.0
//! `--method download`: fetch an offline payload straight from Adobe.
//!
//! Roadmap 1.3 — a port of `ccdl.py` (Drovosek01/adobe-packager): read the remote
//! ledger for endpoints + the app's SAP code / version / arch, hit the
//! unauthenticated product feed, parse the build's `Driver.xml`, download the
//! chunks in parallel with SHA-256 verification, stitch/unpack into a scratch dir,
//! and return it as an [`Acquisition`] for the shared pipeline to stage.
//!
//! Not implemented yet — this is the seam it will fill.

use anyhow::{bail, Result};

use crate::install::Acquisition;
use crate::output::Emitter;

pub fn acquire(_em: &Emitter, _app: Option<&str>, _suite: bool) -> Result<Acquisition> {
    bail!("--method download is not implemented yet (roadmap 1.3: ledger + ccdl.py port)")
}
