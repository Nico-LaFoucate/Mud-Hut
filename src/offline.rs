// SPDX-License-Identifier: Apache-2.0
//! `--method offline`: extract an Adobe offline package / ISO.
//!
//! Roadmap 1.4 — mount/extract a pre-downloaded Adobe offline installer (the same
//! payload shape `download` produces, or an ISO), unpack into a scratch dir, and
//! return it as an [`Acquisition`]. Shares the unpack+stage backend with
//! `download` (only acquisition differs: local package vs network fetch). The
//! package path comes from `--source`.
//!
//! Not implemented yet — this is the seam it will fill.

use std::path::Path;

use anyhow::{bail, Result};

use crate::install::Acquisition;
use crate::output::Emitter;

pub fn acquire(
    _em: &Emitter,
    _package: Option<&Path>,
    _app: Option<&str>,
    _suite: bool,
) -> Result<Acquisition> {
    bail!("--method offline is not implemented yet (roadmap 1.4: extract offline package/ISO)")
}
