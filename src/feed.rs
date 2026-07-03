// SPDX-License-Identifier: Apache-2.0
//! Adobe product-feed + application-manifest resolution for `--method download`.
//!
//! Two live, unauthenticated Adobe endpoints (validated 2026-07-02):
//!   1. **product feed** (XML) — every product/build; we read the `buildGuid` for
//!      an app + platform out of `<product id=SAP><platform id=win64><languageSet>`.
//!   2. **application manifest** (JSON, keyed by the `x-adobe-build-guid` header) —
//!      the package list (`Packages.Package[]`): path, size, SHA-256, language.
//!
//! From those we compute a [`DownloadPlan`]: the subset of packages to fetch for a
//! given install language (core packages + the chosen language's, skipping every
//! other language). The actual chunk download + hash-verify + HyperDrive install
//! build on this (roadmap 1.3 next increments); this module is the resolve half.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::ledger::Ledger;
use crate::output::Emitter;

/// A build resolved from the product feed — what the manifest fetch keys off.
#[derive(Debug, Clone, Serialize)]
pub struct Build {
    pub app_id: String,
    pub sap: String,
    /// Product feed version, e.g. `27.8`.
    pub feed_version: String,
    /// Full product version from the languageSet, e.g. `27.8.0.13`.
    pub product_version: String,
    pub platform: String,
    pub build_guid: String,
}

/// The Adobe application manifest (JSON). Only the fields we use are declared;
/// serde ignores the rest.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "SAPCode", default)]
    pub sap_code: String,
    #[serde(rename = "ProductVersion", default)]
    pub product_version: String,
    #[serde(rename = "Packages", default)]
    pub packages: Packages,
    /// Shared components the app also needs (ACR, CCXP, …). Shape varies; kept raw
    /// so a schema tweak never breaks the parse. Resolved in a later increment.
    #[serde(rename = "Dependencies", default)]
    pub dependencies: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
pub struct Packages {
    #[serde(rename = "Package", default)]
    pub package: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    #[serde(rename = "Type", default)]
    pub kind: String,
    #[serde(rename = "PackageName", default)]
    pub name: String,
    #[serde(rename = "DownloadSize", default)]
    pub download_size: u64,
    #[serde(rename = "Path", default)]
    pub path: String,
    #[serde(rename = "packageHashKey", default)]
    pub hash: String,
    /// e.g. `[installLanguage]==en_US`; empty = required for every install.
    #[serde(rename = "Condition", default)]
    pub condition: String,
}

/// The packages we'll actually fetch for a given language, and the total bytes.
#[derive(Debug, Serialize)]
pub struct DownloadPlan {
    pub app: String,
    pub sap: String,
    pub product_version: String,
    pub build_guid: String,
    pub language: String,
    pub packages: Vec<PlannedPackage>,
    pub total_bytes: u64,
    /// Count of shared dependencies the manifest lists (resolved later).
    pub dependency_count: usize,
}

#[derive(Debug, Serialize)]
pub struct PlannedPackage {
    pub name: String,
    pub kind: String,
    pub bytes: u64,
    pub path: String,
    pub sha256: String,
}

/// Resolve the latest build for `app_id` from the live product feed.
pub fn resolve_build(em: &Emitter, ledger: &Ledger, app_id: &str) -> Result<Build> {
    let app = ledger
        .app(app_id)
        .with_context(|| format!("unknown app id '{app_id}' (see `mudhut ledger`)"))?;
    let platform = ledger.platform_for(app).to_string();
    let url = ledger.endpoints.products_feed.replace("{platform}", &platform);

    em.progress("resolve", 10, &format!("product feed · {} · {}", app.sap, platform));
    let xml = http_get_text(&url, &ledger.headers, None)
        .with_context(|| format!("fetching product feed {url}"))?;
    let doc = roxmltree::Document::parse(&xml).context("parsing product feed XML")?;

    // Highest-versioned <product id=SAP>.
    let mut best: Option<(Vec<u32>, roxmltree::Node)> = None;
    for product in doc.descendants().filter(|n| n.has_tag_name("product")) {
        if product.attribute("id") != Some(app.sap.as_str()) {
            continue;
        }
        let key = version_key(product.attribute("version").unwrap_or("0"));
        if best.as_ref().map(|(b, _)| key > *b).unwrap_or(true) {
            best = Some((key, product));
        }
    }
    let (_, product) = best.with_context(|| {
        format!("app {} ({}) not found in the product feed", app.name, app.sap)
    })?;
    let feed_version = product.attribute("version").unwrap_or_default().to_string();

    // <platform id=platform> -> <languageSet buildGuid=… productVersion=…>
    let langset = product
        .descendants()
        .filter(|n| n.has_tag_name("platform"))
        .find(|p| p.attribute("id") == Some(platform.as_str()))
        .and_then(|p| p.descendants().find(|n| n.has_tag_name("languageSet")))
        .with_context(|| format!("no {platform} build for {} in the feed", app.sap))?;
    let build_guid = langset
        .attribute("buildGuid")
        .context("languageSet has no buildGuid")?
        .to_string();
    let product_version = langset
        .attribute("productVersion")
        .unwrap_or(&feed_version)
        .to_string();

    Ok(Build {
        app_id: app_id.to_string(),
        sap: app.sap.clone(),
        feed_version,
        product_version,
        platform,
        build_guid,
    })
}

/// Fetch the application manifest for a resolved build (JSON, build-guid header).
pub fn fetch_manifest(em: &Emitter, ledger: &Ledger, build: &Build) -> Result<Manifest> {
    let url = &ledger.endpoints.application_manifest;
    em.progress("resolve", 45, &format!("manifest · {} {}", build.sap, build.product_version));
    let body = http_get_text(url, &ledger.headers, Some(&build.build_guid))
        .with_context(|| format!("fetching application manifest {url}"))?;
    serde_json::from_str(&body).context("parsing application manifest JSON")
}

/// Compute the download plan for `language` (BCP-ish, e.g. `en_US`). Includes
/// unconditional (core) packages + those whose Condition matches the language;
/// non-language conditions (OS, etc.) are assumed applicable for the platform.
pub fn plan(build: &Build, manifest: &Manifest, language: &str) -> DownloadPlan {
    let mut packages = Vec::new();
    let mut total_bytes = 0u64;
    for p in &manifest.packages.package {
        if !condition_matches(&p.condition, language) {
            continue;
        }
        total_bytes += p.download_size;
        packages.push(PlannedPackage {
            name: p.name.clone(),
            kind: p.kind.clone(),
            bytes: p.download_size,
            path: p.path.clone(),
            sha256: p.hash.clone(),
        });
    }
    let dependency_count = manifest
        .dependencies
        .get("Dependency")
        .and_then(|d| d.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    DownloadPlan {
        app: build.app_id.clone(),
        sap: build.sap.clone(),
        product_version: manifest.product_version.clone(),
        build_guid: build.build_guid.clone(),
        language: language.to_string(),
        packages,
        total_bytes,
        dependency_count,
    }
}

/// `mudhut download <app> --plan` — resolve + fetch the manifest + report the plan
/// WITHOUT downloading. The actual fetch/verify/install is the next increment.
pub fn cmd_plan(em: &Emitter, app_id: &str, language: &str) -> Result<()> {
    let ledger = Ledger::load(em)?;
    let build = resolve_build(em, &ledger, app_id)?;
    let manifest = fetch_manifest(em, &ledger, &build)?;
    let plan = plan(&build, &manifest, language);
    em.progress("resolve", 100, "planned");

    if plan.packages.is_empty() {
        bail!(
            "no packages matched language '{}' for {} — try a different --lang (e.g. en_US)",
            language, plan.sap
        );
    }

    if em.is_json() {
        em.result(&plan);
    } else {
        println!(
            "{} {} ({}) — build {}",
            plan.app, plan.product_version, plan.language, plan.build_guid
        );
        println!(
            "  {} package(s), {:.2} GiB{}:",
            plan.packages.len(),
            plan.total_bytes as f64 / (1u64 << 30) as f64,
            if plan.dependency_count > 0 {
                format!(" (+{} shared dependencies, resolved later)", plan.dependency_count)
            } else {
                String::new()
            }
        );
        for p in &plan.packages {
            println!(
                "    {:>8.1} MiB  [{}]  {}",
                p.bytes as f64 / (1u64 << 20) as f64,
                p.kind,
                p.name
            );
        }
    }
    Ok(())
}

fn condition_matches(cond: &str, lang: &str) -> bool {
    if cond.is_empty() {
        return true;
    }
    if cond.contains("installLanguage") {
        cond.contains(&format!("=={lang}"))
    } else {
        // OS/other conditions — assume applicable for the resolved platform.
        true
    }
}

/// Split a dotted version ("27.8" / "27.8.0.13") into a comparable component vec.
fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|c| c.parse::<u32>().unwrap_or(0)).collect()
}

fn http_get_text(
    url: &str,
    headers: &BTreeMap<String, String>,
    build_guid: Option<&str>,
) -> Result<String> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(90));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    if let Some(g) = build_guid {
        req = req.set("x-adobe-build-guid", g);
    }
    // NB: ureq's `into_string()` caps at 10 MiB; the product feed is larger, so
    // read the raw reader with no cap.
    let resp = req.call().with_context(|| format!("GET {url}"))?;
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .context("reading response body")?;
    Ok(body)
}
