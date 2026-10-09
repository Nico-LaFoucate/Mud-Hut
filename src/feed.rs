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
//! other language). The chunk download + hash-verify (`crate::download`) and the
//! HDPIM install (`crate::hdpim`) build on this; this module is the resolve half.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
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
    /// Product base version, e.g. `27.0` for PHSP 27.8 — required by the DriverInfo
    /// the HDPIM installer consumes. Adobe's manifest usually carries it; if absent
    /// we derive `<major>.0` from the product version (see `plan`).
    #[serde(rename = "BaseVersion", default)]
    pub base_version: String,
    #[serde(rename = "Packages", default)]
    pub packages: Packages,
    /// Shared components the app also needs (ACR, CCXP, …). Shape varies; kept raw
    /// so a schema tweak never breaks the parse. Read by [`plan`].
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
    /// Base validation endpoint; append `?algorithm=TYPE2` for the per-segment
    /// SHA-256 list used to verify the downloaded bytes.
    #[serde(rename = "ValidationURL", default)]
    pub validation_url: String,
    /// e.g. `[installLanguage]==en_US`; empty = required for every install.
    #[serde(rename = "Condition", default)]
    pub condition: String,
}

/// The packages we'll actually fetch for a given language, and the total bytes.
#[derive(Debug, Serialize)]
pub struct DownloadPlan {
    pub app: String,
    pub sap: String,
    pub name: String,
    pub product_version: String,
    /// Product base version (e.g. `27.0`) for the DriverInfo `<BaseVersion>`.
    pub base_version: String,
    pub platform: String,
    pub build_guid: String,
    pub language: String,
    pub packages: Vec<PlannedPackage>,
    pub total_bytes: u64,
    /// Shared components the app needs (from the manifest), also listed in the
    /// generated driver.xml. `install --method download` resolves them from the product
    /// feed ([`resolve_dependencies`]) and downloads them; `mudhut download` does not.
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub sap: String,
    pub base_version: String,
}

#[derive(Debug, Serialize)]
pub struct PlannedPackage {
    /// Which component (SAP) this package belongs to — the per-SAP install dir.
    pub sap: String,
    pub name: String,
    pub kind: String,
    pub bytes: u64,
    pub path: String,
    /// Adobe's `packageHashKey` (decrypted-content identity — NOT the download-byte
    /// hash; kept for reference/reporting only).
    pub package_hash_key: String,
    /// Validation endpoint base (append `?algorithm=TYPE2` for per-segment SHA-256).
    pub validation_url: String,
}

/// Add-on components skipped only by `--minimal` (a smaller download for testing):
/// Libraries (CCXP), Camera Raw (ACR), CoreSync (COSY). The default installs every
/// component the app's manifest lists.
pub const MINIMAL_SKIP_DEPS: &[&str] = &["CCXP", "ACR", "COSY"];

/// Find the highest-versioned `<product id=sap>` that has a `<languageSet>` for
/// `platform`, returning (build_guid, product_version, feed_version).
fn resolve_from_doc(
    doc: &roxmltree::Document,
    sap: &str,
    platform: &str,
) -> Option<(String, String, String)> {
    let mut best: Option<(Vec<u32>, roxmltree::Node)> = None;
    for product in doc.descendants().filter(|n| n.has_tag_name("product")) {
        if product.attribute("id") != Some(sap) {
            continue;
        }
        // must actually have a build for this platform
        let has = product
            .descendants()
            .filter(|n| n.has_tag_name("platform"))
            .any(|p| {
                p.attribute("id") == Some(platform)
                    && p.descendants().any(|n| n.has_tag_name("languageSet"))
            });
        if !has {
            continue;
        }
        let key = version_key(product.attribute("version").unwrap_or("0"));
        if best.as_ref().map(|(b, _)| key > *b).unwrap_or(true) {
            best = Some((key, product));
        }
    }
    let (_, product) = best?;
    let feed_version = product.attribute("version").unwrap_or_default().to_string();
    let langset = product
        .descendants()
        .filter(|n| n.has_tag_name("platform"))
        .find(|p| p.attribute("id") == Some(platform))
        .and_then(|p| p.descendants().find(|n| n.has_tag_name("languageSet")))?;
    let build_guid = langset.attribute("buildGuid")?.to_string();
    let product_version = langset
        .attribute("productVersion")
        .unwrap_or(&feed_version)
        .to_string();
    Some((build_guid, product_version, feed_version))
}

/// Resolve the latest build for `app_id` from the live product feed.
pub fn resolve_build(em: &Emitter, ledger: &Ledger, app_id: &str) -> Result<Build> {
    let app = ledger
        .app(app_id)
        .with_context(|| format!("unknown app id '{app_id}' (see `mudhut ledger`)"))?;
    let platform = ledger.platform_for(app).to_string();
    let url = ledger.endpoints.products_feed.replace("{platform}", &platform);

    em.progress("resolve", 10, &format!("product feed · {} · {}", app.sap, platform));
    let xml = http_get_text(&url, &ledger.honest_headers(), None)
        .with_context(|| format!("fetching product feed {url}"))?;
    let doc = roxmltree::Document::parse(&xml).context("parsing product feed XML")?;

    let (build_guid, product_version, feed_version) = resolve_from_doc(&doc, &app.sap, &platform)
        .with_context(|| format!("app {} ({}) not found in the product feed", app.name, app.sap))?;

    Ok(Build {
        app_id: app_id.to_string(),
        sap: app.sap.clone(),
        feed_version,
        product_version,
        platform,
        build_guid,
    })
}

/// Resolve the downloadable dependency components for `deps` from the product feed.
/// The feed is queried for BOTH platforms (deps often ship win32, not win64); each
/// dep is tried win64 first, then win32. With `minimal`, deps in [`MINIMAL_SKIP_DEPS`]
/// are skipped; a dep not found in the feed is skipped with a note (never fatal).
/// Fetches the feed once and resolves all deps against it.
pub fn resolve_dependencies(
    em: &Emitter,
    ledger: &Ledger,
    deps: &[Dependency],
    minimal: bool,
) -> Result<Vec<Build>> {
    let wanted: Vec<&Dependency> = deps
        .iter()
        .filter(|d| !(minimal && MINIMAL_SKIP_DEPS.contains(&d.sap.as_str())))
        .collect();
    if wanted.is_empty() {
        return Ok(vec![]);
    }
    let url = ledger.endpoints.products_feed.replace("{platform}", "win32,win64");
    em.progress("resolve", 20, &format!("dependency feed · {} components", wanted.len()));
    let xml = http_get_text(&url, &ledger.honest_headers(), None)
        .with_context(|| format!("fetching dependency feed {url}"))?;
    let doc = roxmltree::Document::parse(&xml).context("parsing dependency feed XML")?;

    let mut builds = Vec::new();
    for dep in wanted {
        let mut resolved = None;
        for platform in ["win64", "win32"] {
            if let Some((build_guid, product_version, feed_version)) =
                resolve_from_doc(&doc, &dep.sap, platform)
            {
                resolved = Some(Build {
                    app_id: dep.sap.to_lowercase(),
                    sap: dep.sap.clone(),
                    feed_version,
                    product_version,
                    platform: platform.to_string(),
                    build_guid,
                });
                break;
            }
        }
        match resolved {
            Some(b) => {
                em.progress(
                    "resolve",
                    30,
                    &format!("dep {} {} ({})", b.sap, b.product_version, b.platform),
                );
                builds.push(b);
            }
            None => em.note(&format!(
                "dependency {} not in the product feed — skipping",
                dep.sap
            )),
        }
    }
    Ok(builds)
}

/// Fetch the application manifest for a resolved build (JSON, build-guid header).
/// Returns the parsed manifest AND its raw JSON (Adobe's per-SAP `Application.json`,
/// which HDPIM's ESD layout expects staged next to the packages).
pub fn fetch_manifest(em: &Emitter, ledger: &Ledger, build: &Build) -> Result<(Manifest, String)> {
    let url = &ledger.endpoints.application_manifest;
    em.progress("resolve", 45, &format!("manifest · {} {}", build.sap, build.product_version));
    let body = http_get_text(url, &ledger.honest_headers(), Some(&build.build_guid))
        .with_context(|| format!("fetching application manifest {url}"))?;
    let manifest = serde_json::from_str(&body).context("parsing application manifest JSON")?;
    Ok((manifest, body))
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
            sap: build.sap.clone(),
            name: p.name.clone(),
            kind: p.kind.clone(),
            bytes: p.download_size,
            path: p.path.clone(),
            package_hash_key: p.hash.clone(),
            validation_url: p.validation_url.clone(),
        });
    }
    let dependencies = manifest
        .dependencies
        .get("Dependency")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    Some(Dependency {
                        sap: d.get("SAPCode")?.as_str()?.to_string(),
                        base_version: d.get("BaseVersion").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Product base version for the DriverInfo: prefer the manifest's BaseVersion;
    // else derive `<major>.0` from the product version (e.g. 27.8.0.13 -> 27.0).
    let base_version = if !manifest.base_version.is_empty() {
        manifest.base_version.clone()
    } else {
        let major = manifest
            .product_version
            .split('.')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("0");
        format!("{major}.0")
    };

    DownloadPlan {
        app: build.app_id.clone(),
        sap: build.sap.clone(),
        name: manifest.name.clone(),
        product_version: manifest.product_version.clone(),
        base_version,
        platform: build.platform.clone(),
        build_guid: build.build_guid.clone(),
        language: language.to_string(),
        packages,
        total_bytes,
        dependencies,
    }
}

/// `mudhut download <app>` — resolve + plan, then (with `--dest`) download+verify.
/// `--core-only` drops non-core packages (skips the big AI models); `--only <sub>`
/// keeps only packages whose name contains `<sub>` (selective / testing).
/// Without `--dest`, reports the plan and downloads nothing.
pub fn cmd_download(
    em: &Emitter,
    app_id: &str,
    language: &str,
    dest: Option<&Path>,
    core_only: bool,
    only: Option<&str>,
) -> Result<()> {
    let ledger = Ledger::load(em)?;
    let build = resolve_build(em, &ledger, app_id)?;
    let (manifest, _raw) = fetch_manifest(em, &ledger, &build)?;
    let mut plan = plan(&build, &manifest, language);

    if core_only {
        plan.packages.retain(|p| p.kind == "core");
    }
    if let Some(sub) = only {
        let s = sub.to_lowercase();
        plan.packages.retain(|p| p.name.to_lowercase().contains(&s));
    }
    plan.total_bytes = plan.packages.iter().map(|p| p.bytes).sum();
    em.progress("resolve", 100, "planned");

    if plan.packages.is_empty() {
        bail!(
            "no packages matched for {} (language '{}'{})",
            plan.sap,
            language,
            only.map(|o| format!(", filter '{o}'")).unwrap_or_default()
        );
    }

    match dest {
        None => {
            if em.is_json() {
                em.result(&plan);
            } else {
                print_plan_human(&plan);
            }
        }
        Some(dest) => {
            if !em.is_json() {
                print_plan_human(&plan);
            }
            crate::download::fetch_plan(em, &ledger, &plan, dest)?;
            let driver = crate::driver::write_driver_xml(&plan, dest)?;
            em.note(&format!("wrote install descriptor {}", driver.display()));
        }
    }
    Ok(())
}

fn print_plan_human(plan: &DownloadPlan) {
    println!(
        "{} {} ({}) — build {}",
        plan.app, plan.product_version, plan.language, plan.build_guid
    );
    println!(
        "  {} package(s), {:.2} GiB{}:",
        plan.packages.len(),
        plan.total_bytes as f64 / (1u64 << 30) as f64,
        if plan.dependencies.is_empty() {
            String::new()
        } else {
            format!(" (+{} shared dependencies, resolved later)", plan.dependencies.len())
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

/// Whether a package's `Condition` applies to the install we are performing.
///
/// ⛔ This used to return TRUE for every non-language condition ("assume
/// applicable"), which meant we demanded packages for platforms we are not
/// installing: a win64 install asked for the win7 build (`[OSVersion]<=6.3`) and
/// the 32-bit build (`[OSProcessorFamily]==32-bit`). On the download path that is
/// merely wasteful. On the OFFLINE path it is fatal — the completeness check sees
/// payloads that were never meant to be staged, declares the component
/// incomplete, and SKIPS IT. That is why Creative Cloud Experience (CCXP) was
/// dropped from every offline install even though its x64 payload was present.
///
/// Grammar, taken from the real manifests, `&&`-joined:
///   [OSProcessorFamily]==64-bit          [OSVersion]>=10.0
///   [OSProcessorFamily]==32-bit          [OSVersion]<=6.3
///   [installLanguage]==en_US
///
/// Anything not understood still returns true, so an unrecognized condition can
/// never silently drop a package we would previously have installed.
///
/// ⛔ `[OSProcessorFamily]` is the MACHINE's, never the build's. Every Neutron prefix is
/// 64-bit Windows, and HDPIM evaluates the conditions against it. This used to take
/// "64-bit" from the build's platform, and Adobe labels most add-ons `win32`, so for
/// Camera Raw, CCXP and Libraries we downloaded the 32-bit packages while HDPIM
/// wanted the 64-bit ones: the release test of 2026-10-07 died with "workflow error
/// 182" after downloading everything, and Camera Raw's x64 plug-in was never fetched.
fn condition_matches(cond: &str, lang: &str) -> bool {
    if cond.is_empty() {
        return true;
    }
    // We spoof Windows 11 24H2 on x64 (see hdpim::setup_prefix): an installer sees
    // OS version 10.0 and a 64-bit processor family.
    let os_version: f64 = 10.0;
    let is_64 = true;

    cond.split("&&").all(|clause| {
        let c = clause.trim();
        if c.contains("installLanguage") {
            return c.contains(&format!("=={lang}"));
        }
        if c.contains("OSProcessorFamily") {
            if c.contains("64-bit") {
                return is_64 == c.contains("==");
            }
            if c.contains("32-bit") {
                return (!is_64) == c.contains("==");
            }
            return true;
        }
        if let Some(rest) = c.split("[OSVersion]").nth(1) {
            let rest = rest.trim();
            let (op, num) = if let Some(n) = rest.strip_prefix(">=") {
                (">=", n)
            } else if let Some(n) = rest.strip_prefix("<=") {
                ("<=", n)
            } else if let Some(n) = rest.strip_prefix("==") {
                ("==", n)
            } else if let Some(n) = rest.strip_prefix('>') {
                (">", n)
            } else if let Some(n) = rest.strip_prefix('<') {
                ("<", n)
            } else {
                return true; // unparsed -> keep
            };
            let Ok(want) = num.trim().parse::<f64>() else { return true };
            return match op {
                ">=" => os_version >= want,
                "<=" => os_version <= want,
                "==" => (os_version - want).abs() < f64::EPSILON,
                ">" => os_version > want,
                _ => os_version < want,
            };
        }
        true
    })
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
    crate::net::retry(url, |m| eprintln!("  {m}"), || {
        let resp = req.clone().call().with_context(|| format!("GET {url}"))?;
        let mut body = String::new();
        resp.into_reader()
            .read_to_string(&mut body)
            .context("reading response body")?;
        Ok(body)
    })
}

#[cfg(test)]
mod condition_tests {
    use super::condition_matches;

    #[test]
    fn processor_family_is_the_machines_64_bit() {
        // CCXP's real conditions (manifest 7.14.0.3): a win32-labeled add-on still installs
        // its 64-bit packages on 64-bit Windows.
        assert!(condition_matches("[OSProcessorFamily]==64-bit", "en_US"));
        assert!(condition_matches("[OSProcessorFamily]==64-bit&&[OSVersion]>=10.0", "en_US"));
        assert!(!condition_matches("[OSProcessorFamily]==64-bit&&[OSVersion]<=6.3", "en_US"));
        assert!(!condition_matches("[OSProcessorFamily]==32-bit", "en_US"));
        assert!(condition_matches("[installLanguage]==en_US", "en_US"));
        assert!(!condition_matches("[installLanguage]==de_DE", "en_US"));
    }
}
