// SPDX-License-Identifier: Apache-2.0
//! The remote download ledger.
//!
//! Adobe's endpoints, request headers, and per-app SAP code / version / platform
//! live in a versioned JSON file published alongside the repo — NOT compiled into
//! the binary — so an Adobe API shift is a ledger edit, not a Mud Hut release.
//! The binary carries a bundled copy of the same file as an offline fallback, so
//! `download`/`offline` still work if the remote is unreachable.
//!
//! Load order: `$MUDHUT_LEDGER_URL` (or the default published URL) -> bundled.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::output::Emitter;

/// Published ledger location. Overridable via `$MUDHUT_LEDGER_URL`. (Raw GitHub is
/// versioned and works today; can move to GitHub Pages without a code change.)
const DEFAULT_LEDGER_URL: &str =
    "https://raw.githubusercontent.com/Nico-LaFoucate/Mud-Hut/main/ledger/ledger.json";

/// Compiled-in fallback — the same file at `ledger/ledger.json`. Keep in sync.
const BUNDLED_LEDGER: &str = include_str!("../ledger/ledger.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ledger {
    pub schema: u32,
    #[serde(default)]
    pub updated: String,
    pub endpoints: Endpoints,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub default_platform: String,
    pub apps: BTreeMap<String, LedgerApp>,
    /// Where this instance came from ("remote:<url>" / "bundled"); set at load.
    #[serde(skip)]
    pub origin: String,
}

/// Mud Hut's honest User-Agent, used for every Adobe request that accepts it (the product
/// feed, application manifests, validation lists).
pub const MUDHUT_USER_AGENT: &str = concat!("MudHut/", env!("CARGO_PKG_VERSION"));

impl Ledger {
    /// The ledger headers with Mud Hut's own User-Agent. Adobe's CDN is the one endpoint that
    /// refuses it (403 unless the client says it is Adobe's installer), so only the package
    /// downloads use `headers` as-is.
    pub fn honest_headers(&self) -> BTreeMap<String, String> {
        let mut h = self.headers.clone();
        h.retain(|k, _| !k.eq_ignore_ascii_case("user-agent"));
        h.insert("User-Agent".into(), MUDHUT_USER_AGENT.into());
        h
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoints {
    /// Product feed listing all builds. Contains a `{platform}` placeholder.
    pub products_feed: String,
    /// Per-build manifest. Contains `{sap}` / `{version}` / `{platform}` placeholders.
    pub application_manifest: String,
    /// Fallback CDN base (manifest chunk paths are usually absolute).
    pub cdn_base: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerApp {
    /// Adobe's 4-letter SAP tracking code.
    pub sap: String,
    #[serde(default)]
    pub name: String,
    /// Default build version hint (the feed provides the authoritative list).
    #[serde(default)]
    pub version: String,
    /// Per-app platform/arch override; falls back to `default_platform`.
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub family: String,
}

impl Ledger {
    /// Load the ledger: try the remote URL, fall back to the bundled copy. Only
    /// fails if the bundled copy is itself invalid (a build bug). `em` gets a note
    /// about which source was used (and why, on fallback).
    pub fn load(em: &Emitter) -> Result<Ledger> {
        let url = std::env::var("MUDHUT_LEDGER_URL")
            .unwrap_or_else(|_| DEFAULT_LEDGER_URL.to_string());

        match fetch(&url) {
            Ok(body) => match serde_json::from_str::<Ledger>(&body) {
                Ok(mut l) => {
                    l.origin = format!("remote:{url}");
                    return Ok(l);
                }
                Err(e) => em.note(&format!(
                    "remote ledger parse failed ({e}); using bundled fallback"
                )),
            },
            Err(e) => em.note(&format!(
                "remote ledger unreachable ({e}); using bundled fallback"
            )),
        }

        let mut l: Ledger = serde_json::from_str(BUNDLED_LEDGER)
            .context("bundled ledger is invalid — this is a Mud Hut build bug")?;
        l.origin = "bundled".to_string();
        Ok(l)
    }

    /// Look up an app's ledger entry by Mud Hut id. (Used by `download` — 1.3.)
    #[allow(dead_code)]
    pub fn app(&self, id: &str) -> Option<&LedgerApp> {
        self.apps.get(id)
    }

    /// The platform/arch to request for an app (per-app override or the default).
    pub fn platform_for<'a>(&'a self, a: &'a LedgerApp) -> &'a str {
        if a.platform.is_empty() {
            &self.default_platform
        } else {
            &a.platform
        }
    }
}

fn fetch(url: &str) -> Result<String> {
    let body = ureq::get(url)
        .timeout(Duration::from_secs(15))
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("reading ledger body")?;
    Ok(body)
}

/// `mudhut ledger` — load and print the resolved ledger (origin + apps). Useful
/// for verifying the published file and for Collider to enumerate downloadables.
pub fn cmd_ledger(em: &Emitter) -> Result<()> {
    let l = Ledger::load(em)?;

    if em.is_json() {
        #[derive(Serialize)]
        struct Out<'a> {
            origin: &'a str,
            ledger: &'a Ledger,
        }
        em.result(&Out { origin: &l.origin, ledger: &l });
    } else {
        println!("Ledger (schema {}, updated {}) — {}", l.schema, l.updated, l.origin);
        println!("  products_feed: {}", l.endpoints.products_feed);
        println!("  manifest:      {}", l.endpoints.application_manifest);
        println!("  platform:      {}", l.default_platform);
        println!("  {} app(s):", l.apps.len());
        for (id, a) in &l.apps {
            println!("    {:<14} [{}] v{} {}", id, a.sap, a.version, l.platform_for(a));
        }
    }
    Ok(())
}
