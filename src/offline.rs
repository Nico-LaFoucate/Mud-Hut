// SPDX-License-Identifier: Apache-2.0
//! `--method offline`: install from a pre-downloaded Adobe offline package.
//!
//! A package is Adobe's ESD products layout — exactly what `mudhut download
//! <app> --dest <dir>` stages (and what a Set-up.exe offline bundle carries in
//! its `products/` dir): `<dir>/<SAP>/Application.json` + the payload zips, for
//! the product and each dependency component.
//!
//! Resolution is FULLY LOCAL: the manifests are read from the package, never
//! the network. The install itself is the same HDPIM decrypt engine `download`
//! uses ([`crate::download::hdpim_install_and_provision`]) — offline packages
//! are encrypted, so they cannot be copy-staged like `windows`.
//!
//! ISOs / archives are not mounted here — extract one first and point
//! `--source` at the dir that holds the `<SAP>/` payload dirs (or its parent
//! with a `products/` child).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::catalog;
use crate::feed::{Build, DownloadPlan, Manifest};
use crate::output::Emitter;

/// Resolve the products dir of an offline package: the dir itself, or a
/// `products/` child (the Set-up.exe bundle layout). A dir qualifies when at
/// least one catalog app's `<SAP>/Application.json` is staged in it.
pub fn products_dir(source: &Path) -> Option<PathBuf> {
    for cand in [source.to_path_buf(), source.join("products")] {
        if catalog::apps().iter().any(|a| cand.join(a.sap).join("Application.json").is_file()) {
            return Some(cand);
        }
    }
    None
}

/// `install --method offline`: verify the staged package for `app` is complete,
/// then run the HDPIM decrypt-install into `prefix`. No network access.
pub fn install(
    em: &Emitter,
    app: Option<&str>,
    source: Option<&Path>,
    prefix: &Path,
    dry_run: bool,
) -> Result<()> {
    let app_id = app.context(
        "`--method offline` installs one app at a time — pass an app id (e.g. photoshop)",
    )?;
    let cat = catalog::find(app_id)
        .with_context(|| format!("unknown app '{app_id}' (see `mudhut apps`)"))?;
    let source =
        source.context("--source is required for --method offline (the package dir)")?;
    if source.is_file() {
        bail!(
            "--source points at a file — ISO/archive packages aren't mounted yet; \
             extract it and point --source at the products dir"
        );
    }
    let products = products_dir(source).with_context(|| {
        format!(
            "{} is not an offline package (no <SAP>/Application.json found — \
             stage one with `mudhut download <app> --dest <dir>`)",
            source.display()
        )
    })?;

    // Local manifest resolve (the package is the source of truth — no feed).
    let (manifest, build) = read_manifest(&products, cat.sap, app_id).with_context(|| {
        format!("{} is not staged in this package ({})", cat.name, products.display())
    })?;
    em.progress(
        "resolve",
        30,
        &format!("local manifest · {} {} ({})", build.sap, build.product_version, build.platform),
    );
    let mut plan = crate::feed::plan(&build, &manifest, "en_US");
    if plan.packages.is_empty() {
        bail!("the staged {} manifest lists no en_US packages", cat.name);
    }

    // The DriverInfo dependency list must match what's staged: keep only the
    // dependency components actually present in the package.
    plan.dependencies
        .retain(|d| products.join(&d.sap).join("Application.json").is_file());

    // Verify every planned payload file exists (product + each staged dep) so an
    // incomplete package fails HERE with a clear message, not deep inside HDPIM.
    let mut missing = missing_packages(&plan, &products);
    for dep in &plan.dependencies {
        let (dm, db) = read_manifest(&products, &dep.sap, &dep.sap.to_lowercase())
            .with_context(|| format!("reading staged dependency {}", dep.sap))?;
        missing.extend(missing_packages(&crate::feed::plan(&db, &dm, "en_US"), &products));
    }
    if !missing.is_empty() {
        let shown: Vec<&str> = missing.iter().take(5).map(|s| s.as_str()).collect();
        bail!(
            "offline package incomplete — {} payload file(s) missing (e.g. {})",
            missing.len(),
            shown.join(", ")
        );
    }
    em.progress("resolve", 100, &format!("package verified · {} dep(s) staged", plan.dependencies.len()));

    em.note(&format!(
        "install {} {} ({}) + {} dep(s) from {}",
        cat.name,
        plan.product_version,
        plan.sap,
        plan.dependencies.len(),
        products.display()
    ));

    if dry_run {
        em.note(&format!(
            "(dry run) package complete — would HDPIM-install {} into {}",
            cat.name,
            prefix.display()
        ));
        if em.is_json() {
            em.result(&serde_json::json!({
                "ok": true,
                "dry_run": true,
                "method": "offline",
                "app": cat.id,
                "product_version": plan.product_version,
                "dependencies": plan.dependencies.iter().map(|d| d.sap.clone()).collect::<Vec<_>>(),
                "packages_verified": true,
                "prefix": prefix.display().to_string(),
            }));
        }
        return Ok(());
    }

    // Write the DriverInfo INTO the products dir, alongside the <SAP>/ payload
    // dirs — exactly like the download route. HDPIM resolves each <EsdDirectory>
    // relative to the driver.xml's OWN directory, NOT the process CWD: a driver.xml
    // written to a scratch dir (even with CWD=products) makes HDPIM fail at start
    // with error 103 "Error occurred in starting install" — it looks for
    // <scratch>/PHSP, which doesn't exist. So it must be co-located with the ESD
    // dirs. (If the package sits on read-only media, copy it to a writable dir
    // first — the write below surfaces the error.)
    let driver_xml = crate::driver::write_driver_xml(&plan, &products).with_context(|| {
        format!(
            "writing driver.xml into {} — the package dir must be writable \
             (copy it off read-only media first)",
            products.display()
        )
    })?;

    crate::download::hdpim_install_and_provision(
        em, &cat, &driver_xml, &products, prefix, "offline", dry_run,
    )
}

/// Read a component's staged `Application.json`, returning the typed manifest
/// plus a [`Build`] assembled from the manifest's own Platform/BuildGuid fields
/// (the fields `download` gets from the product feed — here fully local).
fn read_manifest(products: &Path, sap: &str, app_id: &str) -> Result<(Manifest, Build)> {
    let p = products.join(sap).join("Application.json");
    let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
    // Platform + BuildGuid live in the raw manifest, outside the typed subset.
    let v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
    let platform =
        v.get("Platform").and_then(|x| x.as_str()).unwrap_or("win64").to_string();
    let build_guid =
        v.get("BuildGuid").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let build = Build {
        app_id: app_id.to_string(),
        sap: sap.to_string(),
        feed_version: manifest.product_version.clone(),
        product_version: manifest.product_version.clone(),
        platform,
        build_guid,
    };
    Ok((manifest, build))
}

/// The planned payload files NOT present under `<products>/<SAP>/`.
fn missing_packages(plan: &DownloadPlan, products: &Path) -> Vec<String> {
    plan.packages
        .iter()
        .filter_map(|p| {
            let file = p.path.rsplit('/').next().unwrap_or(p.path.as_str());
            let path = products.join(&p.sap).join(file);
            if path.is_file() {
                None
            } else {
                Some(format!("{}/{}", p.sap, file))
            }
        })
        .collect()
}
