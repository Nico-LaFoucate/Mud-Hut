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
    /// Whether the build is on Neutron's verified list (None when not looked up: a local package).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Adobe's newest build of the app when it is not this one (what `--allow-unverified` takes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
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
    /// `Modules.Module[]`: optional parts of the app (`Id`, `DisplayName`, `DeploymentType`
    /// OnDemand/Deferred, `ReferencePackages.ReferencePackage[]`). Kept raw, like Dependencies.
    #[serde(rename = "Modules", default)]
    pub modules: serde_json::Value,
}

/// One optional module of an app, from the manifest.
#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub id: String,
    pub display_name: String,
    pub deployment: String,
    pub packages: Vec<String>,
}

impl Module {
    /// Adobe's speech-to-text language packs (Premiere Pro and Media Encoder ship 16 of them,
    /// about 850 MB each). Identified by Adobe's own module label, "Speech to Text Language Pack
    /// (<language>)", and its `<SAP>-esl_lp_<lang>` id.
    pub fn is_speech_pack(&self) -> bool {
        self.display_name.starts_with("Speech to Text") || self.id.contains("-esl_lp_")
    }
}

impl Manifest {
    pub fn module_list(&self) -> Vec<Module> {
        let arr = self.modules.get("Module").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        arr.iter()
            .filter_map(|m| {
                let refs = m.get("ReferencePackages").and_then(|r| r.get("ReferencePackage"));
                let packages = match refs {
                    Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                    Some(serde_json::Value::String(s)) => vec![s.clone()],
                    _ => vec![],
                };
                Some(Module {
                    id: m.get("Id")?.as_str()?.to_string(),
                    display_name: m.get("DisplayName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    deployment: m.get("DeploymentType").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    packages,
                })
            })
            .collect()
    }
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
    /// The package's own version. Unchanged packages keep theirs across releases, so an
    /// update only needs the packages whose version differs from what hdpim.db records.
    #[serde(rename = "PackageVersion", default)]
    pub version: String,
    /// Binary deltas from the one or two previous package versions.
    #[serde(rename = "DeltaPackages", default)]
    pub deltas: Vec<DeltaPackage>,
    /// Bytes once unpacked (for the free-space check before an update).
    #[serde(rename = "ExtractSize", default)]
    pub extract_size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeltaPackage {
    #[serde(rename = "PackageName", default)]
    pub name: String,
    #[serde(rename = "BasePackageVersion", default)]
    pub base_version: String,
    #[serde(rename = "DownloadSize", default)]
    pub download_size: u64,
    #[serde(rename = "Path", default)]
    pub path: String,
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
    /// The modules to install, named in the DriverInfo's `<Modules>`. Empty when every module
    /// of the app is planned: the DriverInfo then carries no `<Modules>`, exactly as before.
    pub modules: Vec<String>,
    /// Modules left out (the speech-to-text packs unless asked for).
    pub skipped_modules: Vec<String>,
    /// Bytes the skipped modules would have added.
    pub skipped_bytes: u64,
}

/// Choices that shape a plan beyond the language.
#[derive(Debug, Clone, Default)]
pub struct PlanOpts {
    /// Install the highest build of this major (e.g. 28 for Photoshop 2027) instead of the
    /// newest build in the feed.
    pub major: Option<u64>,
    /// Include the speech-to-text language packs (default off, as in Creative Cloud).
    pub speech_packs: bool,
    /// Modules that are installed already and must stay (an update keeps what the user has).
    pub keep_modules: Vec<String>,
    /// With `major`: a build Neutron has not verified may be chosen (the user confirmed it).
    /// Under the hold policy the newest VERIFIED build of that major is picked when there is one.
    pub allow_unverified: bool,
    /// After installing a new major, remove the older majors of the same app (HDPIM's own
    /// uninstall, user preferences kept).
    pub remove_previous: bool,
    /// Exactly these modules (an update of a recorded install: the modules Adobe's record lists,
    /// as Creative Cloud updates what is installed and nothing more). Overrides the rest.
    pub only_modules: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub sap: String,
    pub base_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedPackage {
    /// Which component (SAP) this package belongs to — the per-SAP install dir.
    pub sap: String,
    pub name: String,
    /// Adobe's PackageVersion.
    pub version: String,
    /// The module this package belongs to, if any.
    pub module: Option<String>,
    /// Unpacked size (0 when the manifest does not say).
    pub extract_bytes: u64,
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

/// One build of a product in Adobe's feed (the feed lists every build of the current and the
/// previous major, each with its own base version and build GUID).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub sap: String,
    /// `<product version>`: the codex version, "27.11".
    pub feed_version: String,
    /// `<languageSet productVersion>`: the build, "27.11.0.26".
    pub product_version: String,
    /// `<languageSet baseVersion>`: Adobe's identity of the product line, "27.0".
    pub base_version: Option<String>,
    pub build_guid: String,
    pub platform: String,
}

impl Candidate {
    pub fn base(&self) -> String {
        self.base_version.clone().unwrap_or_else(|| format!("{}.0", crate::version::major(&self.product_version)))
    }
    pub fn to_build(&self, app_id: &str) -> Build {
        Build {
            app_id: app_id.to_string(),
            sap: self.sap.clone(),
            feed_version: self.feed_version.clone(),
            product_version: self.product_version.clone(),
            platform: self.platform.clone(),
            build_guid: self.build_guid.clone(),
            verified: None,
            newest: None,
        }
    }
}

/// Every `<product id=sap>` build that has a `<languageSet>` for `platform`, newest first.
pub fn candidates_from_doc(doc: &roxmltree::Document, sap: &str, platform: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for product in doc.descendants().filter(|n| n.has_tag_name("product")) {
        if product.attribute("id") != Some(sap) {
            continue;
        }
        let Some(langset) = product
            .descendants()
            .filter(|n| n.has_tag_name("platform"))
            .find(|p| p.attribute("id") == Some(platform))
            .and_then(|p| p.descendants().find(|n| n.has_tag_name("languageSet")))
        else {
            continue;
        };
        let Some(guid) = langset.attribute("buildGuid") else { continue };
        if out.iter().any(|c| c.build_guid == guid) {
            continue; // the same build listed under two channels
        }
        let feed_version = product.attribute("version").unwrap_or_default().to_string();
        out.push(Candidate {
            sap: sap.to_string(),
            product_version: langset.attribute("productVersion").unwrap_or(&feed_version).to_string(),
            feed_version,
            base_version: langset.attribute("baseVersion").map(str::to_string),
            build_guid: guid.to_string(),
            platform: platform.to_string(),
        });
    }
    out.sort_by(|a, b| crate::version::cmp(&b.product_version, &a.product_version));
    out
}

/// What `pick_candidate` chose.
#[derive(Debug, PartialEq)]
pub(crate) enum Pick<'a> {
    /// Install this build.
    Build(&'a Candidate),
    /// Only an unverified build is on offer; installing it needs `--allow-unverified`.
    Unverified(&'a Candidate),
    /// Nothing on offer (no build, or only blocked ones).
    None,
}

/// The build an install takes: the hold policy updates already follow (C70). The newest VERIFIED
/// build, never a blocked one, so a new yearly version that Neutron has not verified yet is not what
/// a new user gets by default. `--allow-unverified` is the user's informed choice of Adobe's newest
/// build (of the chosen yearly version), tested or not, still never a blocked one. An app with no
/// verified build at all (the experimental ones) keeps Adobe's newest non-blocked build when no
/// yearly version was chosen, as before. `all` is newest first.
pub(crate) fn pick_candidate<'a>(
    all: &'a [Candidate],
    list: &crate::verified::List,
    sap: &str,
    major: Option<u64>,
    allow_unverified: bool,
) -> Pick<'a> {
    let pool: Vec<&Candidate> = all
        .iter()
        .filter(|c| major.is_none_or(|m| crate::version::major(&c.product_version) == m))
        .filter(|c| list.blocked(&c.sap, &c.base(), &c.product_version).is_none())
        .collect();
    let Some(newest) = pool.first().copied() else { return Pick::None };
    if allow_unverified {
        return Pick::Build(newest); // the user chose Adobe's newest, tested or not
    }
    if let Some(v) = pool.iter().find(|c| list.verified(&c.sap, &c.base(), &c.product_version).is_some()) {
        return Pick::Build(v);
    }
    if major.is_none() && !list.covers(sap) {
        return Pick::Build(newest);
    }
    Pick::Unverified(newest)
}

/// Resolve the latest build for `app_id` from the live product feed.
pub fn resolve_build(em: &Emitter, ledger: &Ledger, app_id: &str) -> Result<Build> {
    resolve_build_version(em, ledger, app_id, None, false)
}

/// Resolve the newest build for `app_id`, or with `major`, the newest build of that major
/// (`install --version 28` = Photoshop 2027 beside 2026).
pub fn resolve_build_version(em: &Emitter, ledger: &Ledger, app_id: &str, major: Option<u64>, allow_unverified: bool) -> Result<Build> {
    let app = ledger
        .app(app_id)
        .with_context(|| format!("unknown app id '{app_id}' (see `mudhut ledger`)"))?;
    let platform = ledger.platform_for(app).to_string();
    let url = ledger.endpoints.products_feed.replace("{platform}", &platform);

    em.progress("resolve", 10, &format!("product feed · {} · {}", app.sap, platform));
    let xml = http_get_text(&url, &ledger.honest_headers(), None)
        .with_context(|| format!("fetching product feed {url}"))?;
    let doc = roxmltree::Document::parse(&xml).context("parsing product feed XML")?;

    let all = candidates_from_doc(&doc, &app.sap, &platform);
    let list = crate::verified::load(false);
    let pick = match pick_candidate(&all, &list.list, &app.sap, major, allow_unverified) {
        Pick::Build(c) => {
            let verified = list.list.verified(&c.sap, &c.base(), &c.product_version).is_some();
            let newest = all
                .iter()
                .find(|n| list.list.blocked(&n.sap, &n.base(), &n.product_version).is_none())
                .filter(|n| n.product_version != c.product_version)
                .map(|n| n.product_version.clone());
            if !verified {
                em.note(&format!(
                    "{} {} has not been tested by Neutron yet. It may not open, or parts of it may not work.",
                    app.name.trim_end_matches(" (experimental)"), c.product_version
                ));
            } else if let Some(n) = &newest {
                em.note(&format!(
                    "installing {}, the newest build Neutron has tested; Adobe's newest is {n} (not yet tested; --allow-unverified installs it)",
                    c.product_version
                ));
            }
            let mut b = c.to_build(app_id);
            b.verified = Some(verified);
            b.newest = newest;
            Some(b)
        }
        Pick::Unverified(newest) => {
            return Err(crate::failure::Failure::new(
                "unverified",
                crate::failure::EX_FAIL,
                format!(
                    "Neutron hasn't verified {} {} yet. It may not open, or parts of it may not work. To install it anyway, confirm (--allow-unverified).",
                    app.name.trim_end_matches(" (experimental)"),
                    crate::version::label(&newest.product_version)
                ),
            )
            .with("prefix_changed", false)
            .with("to", newest.product_version.clone())
            .into())
        }
        Pick::None => None,
    };
    match (pick, major) {
        (Some(b), _) => Ok(b),
        (None, None) => bail!("app {} ({}) not found in the product feed", app.name, app.sap),
        (None, Some(m)) => {
            let mut majors: Vec<u64> = all.iter().map(|c| crate::version::major(&c.product_version)).collect();
            majors.dedup();
            Err(crate::failure::Failure::new(
                "version-not-offered",
                crate::failure::EX_FAIL,
                format!(
                    "Adobe does not offer {} version {m}. Versions offered: {}.",
                    app.name.trim_end_matches(" (experimental)"),
                    majors.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", ")
                ),
            )
            .with("prefix_changed", false)
            .into())
        }
    }
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
            // The manifest names each dependency by (SAP code, base version), which is how
            // Adobe's own engine matches them ("No matching STI found in FFC for Product
            // (sapcode %s, baseversion %s)"). Prefer the newest build of THAT base; fall back to
            // the newest build at all, which is what this always did and still the only
            // answer when the base is not in the feed.
            let all = candidates_from_doc(&doc, &dep.sap, platform);
            let same_base = all.iter().find(|c| {
                !dep.base_version.is_empty() && crate::version::eq(&c.base(), &dep.base_version)
            });
            if let Some(c) = same_base.or(all.first()) {
                resolved = Some(c.to_build(&dep.sap.to_lowercase()));
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

/// Compute the download plan for `language` (BCP-ish, e.g. `en_US`) with the default choices.
pub fn plan(build: &Build, manifest: &Manifest, language: &str) -> DownloadPlan {
    plan_with(build, manifest, language, &PlanOpts::default())
}

/// Compute the download plan. Includes unconditional (core) packages + those whose Condition
/// matches the language and machine, and every module except the speech-to-text language packs
/// (unless `opts.speech_packs`, or the module is in `opts.keep_modules`).
///
/// ⭐ Speech packs default OFF (operator decision, 2026-10-16): Creative Cloud does not install
/// them by default (a Windows-installed Premiere 26.3 has none of the 16), and planning every
/// module made a default Premiere install about 14.5 GB larger, repeated by every update.
pub fn plan_with(build: &Build, manifest: &Manifest, language: &str, opts: &PlanOpts) -> DownloadPlan {
    let modules = manifest.module_list();
    let module_of = |pkg: &str| modules.iter().find(|m| m.packages.iter().any(|p| p == pkg));
    let wanted = |m: &Module| match &opts.only_modules {
        Some(only) => only.iter().any(|k| k == &m.id),
        None => !m.is_speech_pack() || opts.speech_packs || opts.keep_modules.iter().any(|k| k == &m.id),
    };

    let mut packages = Vec::new();
    let mut total_bytes = 0u64;
    let mut skipped_bytes = 0u64;
    for p in &manifest.packages.package {
        if !condition_matches(&p.condition, language) {
            continue;
        }
        let module = module_of(&p.name);
        if let Some(m) = module {
            if !wanted(m) {
                skipped_bytes += p.download_size;
                continue;
            }
        }
        total_bytes += p.download_size;
        packages.push(PlannedPackage {
            sap: build.sap.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
            module: module.map(|m| m.id.clone()),
            extract_bytes: p.extract_size,
            kind: p.kind.clone(),
            bytes: p.download_size,
            path: p.path.clone(),
            package_hash_key: p.hash.clone(),
            validation_url: p.validation_url.clone(),
        });
    }
    let skipped: Vec<String> = modules.iter().filter(|m| !wanted(m)).map(|m| m.id.clone()).collect();
    // Name the modules only when some are left out; otherwise keep the DriverInfo exactly as
    // every install so far has had it (no <Modules>).
    let selected: Vec<String> = if skipped.is_empty() {
        vec![]
    } else {
        modules.iter().filter(|m| wanted(m)).map(|m| m.id.clone()).collect()
    };

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
        modules: selected,
        skipped_modules: skipped,
        skipped_bytes,
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
    let (manifest, raw) = fetch_manifest(em, &ledger, &build)?;
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
            crate::download::fetch_packages(em, &ledger, &plan, dest)?;
            // HDPIM's ESD layout needs the manifest beside the payloads: without
            // `<SAP>/Application.json` a later `install --source <dest>` failed with HDPIM
            // error 107 ("Could not find 'Application.json' file inside ESD directory").
            crate::download::write_application_json(dest, &plan.sap, &raw)?;
            let driver = crate::driver::write_driver_xml(&plan, dest)?;
            em.note(&format!("wrote install descriptor {}", driver.display()));
            if em.is_json() {
                em.result(&serde_json::json!({
                    "ok": true, "app": plan.app, "product_version": plan.product_version,
                    "packages": plan.packages.len(), "total_bytes": plan.total_bytes,
                    "verified": true, "dest": dest.display().to_string(),
                }));
            } else {
                outln!(
                    "Downloaded + verified {} package(s), {:.2} GiB into {}",
                    plan.packages.len(),
                    plan.total_bytes as f64 / (1u64 << 30) as f64,
                    dest.display()
                );
            }
        }
    }
    Ok(())
}

fn print_plan_human(plan: &DownloadPlan) {
    outln!(
        "{} {} ({}) — build {}",
        plan.app, plan.product_version, plan.language, plan.build_guid
    );
    outln!(
        "  {} package(s), {:.2} GiB{}:",
        plan.packages.len(),
        plan.total_bytes as f64 / (1u64 << 30) as f64,
        if plan.dependencies.is_empty() {
            String::new()
        } else {
            format!(" (+{} shared components, not downloaded by this command)", plan.dependencies.len())
        }
    );
    for p in &plan.packages {
        outln!(
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
    crate::net::retry(url, |m| errln!("  {m}"), || {
        let resp = req.clone().call().with_context(|| format!("GET {url}"))?;
        let mut body = String::new();
        resp.into_reader()
            .read_to_string(&mut body)
            .context("reading response body")?;
        Ok(body)
    })
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    /// A trimmed Premiere-shaped manifest: core, one Deferred module, two speech packs
    /// (one OnDemand, the English one Deferred), and an OnDemand codec module.
    fn ppro() -> Manifest {
        serde_json::from_str(r#"{
          "Name":"Premiere Pro","SAPCode":"PPRO","ProductVersion":"26.5.2.5","BaseVersion":"26.0",
          "Packages":{"Package":[
            {"Type":"core","PackageName":"AdobePremierePro26AllTrial","DownloadSize":2000,"Path":"/a/core.zip","PackageVersion":"26.5.2.5",
             "DeltaPackages":[{"PackageName":"core_delta","BasePackageVersion":"26.5.1.1","DownloadSize":100,"Path":"/a/d.zip"}]},
            {"Type":"non-core","PackageName":"AdobePremierePro26AllTrial-esl_lp_de","DownloadSize":800,"Path":"/a/de.zip","PackageVersion":"26.5.0.99"},
            {"Type":"non-core","PackageName":"AdobePremierePro26AllTrial-esl_lp_en","DownloadSize":800,"Path":"/a/en.zip","PackageVersion":"26.5.0.99"},
            {"Type":"non-core","PackageName":"AdobePremierePro26AllTrial-ObjectMask","DownloadSize":600,"Path":"/a/om.zip","PackageVersion":"26.5.0.99"},
            {"Type":"non-core","PackageName":"AdobePremierePro26AllTrial-OptionalHEVCCodec","DownloadSize":17,"Path":"/a/hevc.zip","PackageVersion":"26.3.0.93"}
          ]},
          "Modules":{"Module":[
            {"Id":"PPRO-esl_lp_de","DisplayName":"Speech to Text Language Pack (German)","DeploymentType":"OnDemand","ReferencePackages":{"ReferencePackage":["AdobePremierePro26AllTrial-esl_lp_de"]}},
            {"Id":"PPRO-esl_lp_en","DisplayName":"Speech to Text Language Pack (English)","DeploymentType":"Deferred","ReferencePackages":{"ReferencePackage":["AdobePremierePro26AllTrial-esl_lp_en"]}},
            {"Id":"PPRO-ObjectMask","DisplayName":"Object Mask","DeploymentType":"Deferred","ReferencePackages":{"ReferencePackage":["AdobePremierePro26AllTrial-ObjectMask"]}},
            {"Id":"PPRO-OptionalHEVCCodec","DisplayName":"Optional HEVC codec","DeploymentType":"OnDemand","ReferencePackages":{"ReferencePackage":["AdobePremierePro26AllTrial-OptionalHEVCCodec"]}}
          ]},
          "Dependencies":{"Dependency":[{"SAPCode":"CCXP","BaseVersion":"2.2.1"}]}
        }"#).unwrap()
    }

    fn build() -> Build {
        Build { app_id: "premiere".into(), sap: "PPRO".into(), feed_version: "26.5.2".into(),
                product_version: "26.5.2.5".into(), platform: "win64".into(), build_guid: "g".into(),
                verified: None, newest: None }
    }

    #[test]
    fn speech_packs_are_off_by_default_and_named_out_of_the_request() {
        let p = plan(&build(), &ppro(), "en_US");
        let names: Vec<&str> = p.packages.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["AdobePremierePro26AllTrial", "AdobePremierePro26AllTrial-ObjectMask", "AdobePremierePro26AllTrial-OptionalHEVCCodec"]);
        assert_eq!(p.skipped_modules, vec!["PPRO-esl_lp_de", "PPRO-esl_lp_en"]);
        assert_eq!(p.skipped_bytes, 1600);
        assert_eq!(p.modules, vec!["PPRO-ObjectMask", "PPRO-OptionalHEVCCodec"], "the request names what IS installed");
        assert_eq!(p.packages[0].version, "26.5.2.5");
    }

    #[test]
    fn speech_packs_opt_in_or_kept_when_installed() {
        let all = plan_with(&build(), &ppro(), "en_US", &PlanOpts { speech_packs: true, ..Default::default() });
        assert_eq!(all.packages.len(), 5);
        assert!(all.modules.is_empty(), "nothing left out -> no <Modules>, as before");
        let kept = plan_with(&build(), &ppro(), "en_US", &PlanOpts { keep_modules: vec!["PPRO-esl_lp_de".into()], ..Default::default() });
        assert!(kept.packages.iter().any(|x| x.name.ends_with("esl_lp_de")));
        assert!(!kept.packages.iter().any(|x| x.name.ends_with("esl_lp_en")));
        // An update keeps exactly the recorded modules (Windows installed HEVC but no Object Mask).
        let upd = plan_with(&build(), &ppro(), "en_US", &PlanOpts { only_modules: Some(vec!["PPRO-OptionalHEVCCodec".into()]), ..Default::default() });
        let names: Vec<&str> = upd.packages.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["AdobePremierePro26AllTrial", "AdobePremierePro26AllTrial-OptionalHEVCCodec"]);
        assert_eq!(upd.modules, vec!["PPRO-OptionalHEVCCodec"]);
    }

    #[test]
    fn candidates_are_every_build_newest_first_with_base() {
        let xml = r#"<products>
          <product id="PHSP" version="27.10"><platforms><platform id="win64"><languageSet buildGuid="g10" productVersion="27.10.0.26" baseVersion="27.0"/></platform></platforms></product>
          <product id="PHSP" version="27.9.1"><platforms><platform id="win64"><languageSet buildGuid="g91" productVersion="27.9.1.1" baseVersion="27.0"/></platform></platforms></product>
          <product id="PHSP" version="26.11.8"><platforms><platform id="win64"><languageSet buildGuid="g26" productVersion="26.11.8.318" baseVersion="26.0"/></platform></platforms></product>
          <product id="PHSP" version="27.10"><platforms><platform id="win64"><languageSet buildGuid="g10" productVersion="27.10.0.26" baseVersion="27.0"/></platform></platforms></product>
          <product id="PHSP" version="28.0"><platforms><platform id="macarm64"><languageSet buildGuid="m" productVersion="28.0.0.1" baseVersion="28.0"/></platform></platforms></product>
        </products>"#;
        let doc = roxmltree::Document::parse(xml).unwrap();
        let c = candidates_from_doc(&doc, "PHSP", "win64");
        let v: Vec<&str> = c.iter().map(|x| x.product_version.as_str()).collect();
        assert_eq!(v, vec!["27.10.0.26", "27.9.1.1", "26.11.8.318"], "deduped by GUID, numeric order, platform-filtered");
        assert_eq!(c[2].base(), "26.0");
    }
}

#[cfg(test)]
mod pick_tests {
    use super::{pick_candidate, Candidate, Pick};

    fn cand(v: &str, base: &str) -> Candidate {
        Candidate { sap: "PHSP".into(), feed_version: v.into(), product_version: v.into(),
                    base_version: Some(base.into()), build_guid: format!("g{v}"), platform: "win64".into() }
    }

    /// The feed the day Adobe ships 2027: newest first, the 2026 point releases below it.
    fn feed() -> Vec<Candidate> {
        vec![cand("28.0.0.10", "28.0"), cand("27.12.0.5", "27.0"), cand("27.11.0.26", "27.0"), cand("27.8.0.13", "27.0")]
    }

    fn list(json: &str) -> crate::verified::List {
        serde_json::from_str(json).unwrap()
    }

    const VERIFIED_2026: &str = r#"{"schema":1,"serial":3,"apps":{"PHSP":{"27.0":{
        "verified":[{"build":"27.11.0.26"},{"build":"27.8.0.13"}],"blocked":[]}}}}"#;

    #[test]
    fn a_fresh_install_takes_the_newest_verified_build_not_adobes_newest() {
        let f = feed();
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", None, false), Pick::Build(&f[2]));
    }

    #[test]
    fn the_user_can_choose_adobes_newest_untested_build() {
        let f = feed();
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", None, true), Pick::Build(&f[0]), "2027, untested");
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", Some(27), true), Pick::Build(&f[1]),
                   "the newest 2026 build, untested");
    }

    #[test]
    fn a_blocked_build_is_never_installed() {
        let f = feed();
        let l = list(r#"{"schema":1,"serial":3,"apps":{"PHSP":{"27.0":{
            "verified":[{"build":"27.11.0.26"},{"build":"27.8.0.13"}],"blocked":[{"build":"27.11.0.26","reason":"crash"}]}}}}"#);
        assert_eq!(pick_candidate(&f, &l, "PHSP", None, false), Pick::Build(&f[3]));
        // --allow-unverified still skips a blocked build.
        let only_blocked = list(r#"{"schema":1,"serial":3,"apps":{"PHSP":{"28.0":{"verified":[],"blocked":[{"build":"28.0.0.10"}]}}}}"#);
        assert_eq!(pick_candidate(&f[..1], &only_blocked, "PHSP", None, true), Pick::None);
    }

    #[test]
    fn an_app_neutron_never_verified_keeps_adobes_newest() {
        let f: Vec<Candidate> = feed().into_iter().map(|c| Candidate { sap: "IDSN".into(), ..c }).collect();
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "IDSN", None, false), Pick::Build(&f[0]),
                   "the experimental apps have no verified build: unchanged behavior");
        // ...but a chosen yearly version of it still asks first, as before.
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "IDSN", Some(28), false), Pick::Unverified(&f[0]));
    }

    #[test]
    fn a_chosen_yearly_version_needs_confirmation_when_unverified() {
        let f = feed();
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", Some(28), false), Pick::Unverified(&f[0]));
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", Some(28), true), Pick::Build(&f[0]));
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", Some(27), false), Pick::Build(&f[2]));
        assert_eq!(pick_candidate(&f, &list(VERIFIED_2026), "PHSP", Some(25), false), Pick::None);
    }

    #[test]
    fn a_covered_app_with_nothing_verified_on_offer_asks_first() {
        let f = feed();
        let stale = list(r#"{"schema":1,"serial":3,"apps":{"PHSP":{"27.0":{"verified":[{"build":"27.1.0.1"}],"blocked":[]}}}}"#);
        assert_eq!(pick_candidate(&f, &stale, "PHSP", None, false), Pick::Unverified(&f[0]));
        assert_eq!(pick_candidate(&f, &stale, "PHSP", None, true), Pick::Build(&f[0]));
    }
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
