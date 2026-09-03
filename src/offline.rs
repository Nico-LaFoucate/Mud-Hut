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
//! `--source` must be an already-extracted, **WRITABLE** directory holding the
//! `<SAP>/` payload dirs (or its parent with a `products/` child).
//!
//! The writability is the real constraint, not the container format: HDPIM
//! resolves each `<EsdDirectory>` relative to the driver XML's OWN directory,
//! so `Driver_core.xml` has to be written INTO the package next to the payload
//! dirs (see the comment at the write site below). Mounting an ISO read-only
//! therefore does NOT help — it fails the same way a scratch dir does. Any
//! read-only source has to be copied to writable storage first.

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
            "--source points at a file ({}), but --method offline needs a directory.\n\
             \n\
             Extract it to writable storage and point --source at the dir holding the\n\
             <SAP>/ payload dirs (e.g. PHSP/Application.json), or its parent.\n\
             \n\
             Note: mounting an ISO instead of extracting it will NOT work. The install\n\
             writes Driver_core.xml into the package dir (HDPIM resolves <EsdDirectory>\n\
             relative to that XML's own directory), so the package must be WRITABLE.",
            source.display()
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

    // WHERE THE DRIVER XML GOES, and why it is a choice rather than a constraint.
    //
    // HDPIM resolves a RELATIVE <EsdDirectory> (`./PHSP`) against the directory the
    // driver XML itself lives in -- NOT the process CWD. So with relative paths the
    // XML must be co-located with the <SAP>/ payload dirs; a scratch dir (even with
    // CWD=products) makes HDPIM fail at start with error 103, looking for
    // <scratch>/PHSP. That is the proven, long-standing download layout.
    //
    // It also means relative paths force a WRITE into the package -- which is the
    // only reason read-only media (a mounted ISO, a read-only share) was ever a
    // problem. Nothing is written to the media itself by the install; it was our own
    // path choice. An ABSOLUTE <EsdDirectory> lifts that: the XML can live in a
    // scratch dir and point at the payloads wherever they are. `InstallDir` in the
    // same XML has always been absolute, so the schema is not relative-only.
    //
    // Default stays relative (the proven path). Absolute is used when the package is
    // read-only, or when forced with MUDHUT_ESD_ABSOLUTE=1 to A/B the two on the same
    // package.
    let force_abs =
        std::env::var_os("MUDHUT_ESD_ABSOLUTE").is_some_and(|v| v != "0" && !v.is_empty());
    let writable = is_writable(&products);
    let mut scratch: Option<PathBuf> = None;

    let driver_xml = if force_abs || !writable {
        let dir = std::env::temp_dir().join(format!("mudhut-driver-{}", std::process::id()));
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating scratch dir {}", dir.display()))?;
        em.note(&format!(
            "driver XML -> {} with ABSOLUTE EsdDirectory ({})",
            dir.display(),
            if writable { "forced by MUDHUT_ESD_ABSOLUTE" } else { "package dir is read-only" }
        ));
        let x = crate::driver::write_driver_xml_in(&plan, &dir, Some(products.as_path()))
            .with_context(|| format!("writing driver.xml into {}", dir.display()))?;
        scratch = Some(dir);
        x
    } else {
        crate::driver::write_driver_xml(&plan, &products).with_context(|| {
            format!("writing driver.xml into {}", products.display())
        })?
    };

    let r = crate::download::hdpim_install_and_provision(
        em, &cat, &driver_xml, &products, prefix, "offline", dry_run,
    );
    if let Some(dir) = scratch {
        let _ = fs::remove_dir_all(dir);
    }
    r
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

/// Whether `dir` can be written to. Not fatal: a read-only package is installed
/// via an absolute `EsdDirectory` instead (see the write site above).
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".mudhut-write-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}
