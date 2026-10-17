// SPDX-License-Identifier: Apache-2.0
//! Generate the HyperDrive `driver.xml` — the install descriptor (DriverInfo) that
//! HDPIM consumes. Mirrors the adobe-packager layout: the target
//! product + its dependencies, each pointing at its per-SAP `EsdDirectory`
//! (`<dest>/<SAP>/`), plus the requested install dir + language.
//!
//! The same document drives all three HDPIM verbs Mud Hut uses (HDPIM 6.5.0.348's parser reads
//! `/DriverInfo/ProductInfo` and `/DriverInfo/RequestInfo` for each of them):
//! - install and update: the full DriverInfo below. An update adds
//!   `<DeleteUserPreferences>false</DeleteUserPreferences>` to RequestInfo (HDPIM's default is
//!   false too; it is written out so the intent is in the request, lab-proven 2026-10-16).
//! - uninstall: SAP code, codex version, base version and platform only; HDPIM rejects an
//!   uninstall request that names dependencies or an EsdDirectory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::feed::DownloadPlan;

/// RequestInfo choices beyond the install dir and language.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// `<DeleteUserPreferences>`: Some(false) for updates and uninstalls (keep the user's
    /// settings, presets and plug-in storage); None leaves it out (HDPIM's default, false).
    pub delete_user_preferences: Option<bool>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Write `<dest>/driver.xml` describing `plan`'s install. Returns the path.
/// Write the DriverInfo with a RELATIVE `EsdDirectory` (`./<SAP>`), into `dest`
/// alongside the payload dirs. The long-proven layout used by `--method download`.
pub fn write_driver_xml(plan: &DownloadPlan, dest: &Path) -> Result<PathBuf> {
    write_driver_xml_in(plan, dest, None)
}

/// Write the DriverInfo to `xml_dir`.
///
/// `esd_root` selects how each `<EsdDirectory>` is expressed:
/// * `None` — relative `./<SAP>`. HDPIM resolves it against the directory the XML
///   itself lives in, so `xml_dir` must also BE the products dir.
/// * `Some(products)` — absolute `Z:\...\<SAP>`, which frees the XML to live
///   anywhere writable and lets the payloads sit on read-only media (a mounted
///   ISO, a read-only share).
pub fn write_driver_xml_in(plan: &DownloadPlan, xml_dir: &Path, esd_root: Option<&Path>) -> Result<PathBuf> {
    let esd = |sap: &str| -> String {
        match esd_root {
            None => format!("./{sap}"),
            Some(root) => esc(&crate::hdpim::to_z_path(&root.join(sap))),
        }
    };
    write(xml_dir, &render(plan, &esd, &Request::default()))
}

/// Write the DriverInfo to `xml_dir` with each component found under its own root
/// (`roots[SAP]` holds `<SAP>/`). A component under `xml_dir` gets the proven relative
/// `./<SAP>`; one staged elsewhere (a read-only `--source`, the cache) an absolute `Z:` path.
pub fn write_driver_xml_roots(
    plan: &DownloadPlan,
    xml_dir: &Path,
    roots: &BTreeMap<String, PathBuf>,
    req: &Request,
) -> Result<PathBuf> {
    let esd = |sap: &str| -> String {
        match roots.get(sap) {
            Some(r) if r != xml_dir => esc(&crate::hdpim::to_z_path(&r.join(sap))),
            _ => format!("./{sap}"),
        }
    };
    write(xml_dir, &render(plan, &esd, req))
}

fn write(xml_dir: &Path, xml: &str) -> Result<PathBuf> {
    let path = xml_dir.join("driver.xml");
    fs::write(&path, xml).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn product_name(name: &str) -> String {
    if name.to_lowercase().starts_with("adobe") {
        name.to_string()
    } else {
        format!("Adobe {name}")
    }
}

fn render(plan: &DownloadPlan, esd: &dyn Fn(&str) -> String, req: &Request) -> String {
    let name = product_name(&plan.name);
    let mut deps = String::new();
    for d in &plan.dependencies {
        deps.push_str(&format!(
            "      <Dependency>\n\
             \x20       <SAPCode>{}</SAPCode>\n\
             \x20       <BaseVersion>{}</BaseVersion>\n\
             \x20       <EsdDirectory>{}</EsdDirectory>\n\
             \x20     </Dependency>\n",
            d.sap, d.base_version, esd(&d.sap)
        ));
    }
    // `/ProductInfo/Modules/Module`, each with an `<Id>` child (HDPIM reads a Module's child
    // elements by name: "Id", "Baseline"). Only present when some module is left out.
    let modules = if plan.modules.is_empty() {
        String::new()
    } else {
        let mut m = String::from("    <Modules>\n");
        for id in &plan.modules {
            m.push_str(&format!("      <Module>\n        <Id>{}</Id>\n      </Module>\n", esc(id)));
        }
        m.push_str("    </Modules>\n");
        m
    };
    let mut request_extra = String::new();
    if let Some(d) = req.delete_user_preferences {
        request_extra.push_str(&format!("    <DeleteUserPreferences>{d}</DeleteUserPreferences>\n"));
    }
    format!(
        "<DriverInfo>\n\
         \x20 <ProductInfo>\n\
         \x20   <Name>{name}</Name>\n\
         \x20   <SAPCode>{sap}</SAPCode>\n\
         \x20   <CodexVersion>{ver}</CodexVersion>\n\
         \x20   <BaseVersion>{base}</BaseVersion>\n\
         \x20   <Platform>{plat}</Platform>\n\
         \x20   <EsdDirectory>{esd_product}</EsdDirectory>\n\
         \x20   <IsNonCCProduct>false</IsNonCCProduct>\n\
         \x20   <IsNglEnabled>true</IsNglEnabled>\n\
         \x20   <SupportedLanguages>\n\
         \x20     <Language locale=\"{lang}\"/>\n\
         \x20   </SupportedLanguages>\n\
         {modules}\
         \x20   <Dependencies>\n\
         {deps}\
         \x20   </Dependencies>\n\
         \x20 </ProductInfo>\n\
         \x20 <RequestInfo>\n\
         \x20   <InstallDir>C:\\Program Files\\Adobe</InstallDir>\n\
         \x20   <InstallLanguage>{lang}</InstallLanguage>\n\
         {request_extra}\
         \x20 </RequestInfo>\n\
         </DriverInfo>\n",
        name = esc(&name),
        sap = plan.sap,
        ver = plan.product_version,
        base = plan.base_version,
        plat = plan.platform,
        deps = deps,
        lang = plan.language,
        esd_product = esd(&plan.sap),
        modules = modules,
        request_extra = request_extra,
    )
}

/// The request for `hdpimUnInstallProduct`. `codex` must be spelled as hdpim.db records the
/// product (`ProductVersion` column, e.g. "26.3"): a request for another spelling finds nothing
/// and fails with error 130 (lab, 2026-10-16).
pub fn write_uninstall_xml(xml_dir: &Path, name: &str, sap: &str, codex: &str, base: &str, platform: &str) -> Result<PathBuf> {
    let xml = format!(
        "<DriverInfo>\n\
         \x20 <ProductInfo>\n\
         \x20   <Name>{name}</Name>\n\
         \x20   <SAPCode>{sap}</SAPCode>\n\
         \x20   <CodexVersion>{codex}</CodexVersion>\n\
         \x20   <BaseVersion>{base}</BaseVersion>\n\
         \x20   <Platform>{platform}</Platform>\n\
         \x20   <IsNonCCProduct>false</IsNonCCProduct>\n\
         \x20   <IsNglEnabled>true</IsNglEnabled>\n\
         \x20 </ProductInfo>\n\
         \x20 <RequestInfo>\n\
         \x20   <InstallDir>C:\\Program Files\\Adobe</InstallDir>\n\
         \x20   <InstallLanguage>en_US</InstallLanguage>\n\
         \x20   <DeleteUserPreferences>false</DeleteUserPreferences>\n\
         \x20 </RequestInfo>\n\
         </DriverInfo>\n",
        name = esc(&product_name(name)),
        sap = esc(sap),
        codex = esc(codex),
        base = esc(base),
        platform = esc(platform),
    );
    fs::create_dir_all(xml_dir).with_context(|| format!("creating {}", xml_dir.display()))?;
    let path = xml_dir.join("uninstall.xml");
    fs::write(&path, xml).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{Dependency, DownloadPlan};

    fn phsp_plan() -> DownloadPlan {
        DownloadPlan {
            app: "photoshop".into(),
            sap: "PHSP".into(),
            name: "Adobe Photoshop".into(),
            product_version: "27.8".into(),
            base_version: "27.0".into(),
            platform: "win64".into(),
            build_guid: "g".into(),
            language: "en_US".into(),
            packages: vec![],
            total_bytes: 0,
            dependencies: vec![
                Dependency { sap: "COCM".into(), base_version: "1.0".into() },
                Dependency { sap: "CORE".into(), base_version: "1.0".into() },
            ],
            modules: vec![],
            skipped_modules: vec![],
            skipped_bytes: 0,
        }
    }

    // The DriverInfo must match the proven Driver_core.xml shape that HDPIM accepts.
    #[test]
    fn driver_xml_has_the_proven_shape() {
        let dir = std::env::temp_dir().join(format!("mudhut-drv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = write_driver_xml(&phsp_plan(), &dir).unwrap();
        let xml = std::fs::read_to_string(&p).unwrap();
        for needle in [
            "<Name>Adobe Photoshop</Name>",
            "<SAPCode>PHSP</SAPCode>",
            "<CodexVersion>27.8</CodexVersion>",
            "<BaseVersion>27.0</BaseVersion>",
            "<Platform>win64</Platform>",
            "<EsdDirectory>./PHSP</EsdDirectory>",
            "<IsNonCCProduct>false</IsNonCCProduct>",
            "<IsNglEnabled>true</IsNglEnabled>",
            "<Language locale=\"en_US\"/>",
            "<SAPCode>COCM</SAPCode>",
            "<InstallDir>C:\\Program Files\\Adobe</InstallDir>",
            "<InstallLanguage>en_US</InstallLanguage>",
        ] {
            assert!(xml.contains(needle), "driver.xml missing {needle:?}\n---\n{xml}");
        }
        // Unchanged for installs: no <Modules>, no DeleteUserPreferences.
        assert!(!xml.contains("<Modules>") && !xml.contains("DeleteUserPreferences"), "{xml}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_esd_directory_points_outside_the_xml_dir() {
        // The read-only-media path: the XML lives in a scratch dir and names the
        // payload dirs by absolute Z: path, so nothing is written to the package.
        let plan = phsp_plan();
        let xml_dir = std::env::temp_dir().join(format!("mudhut-abs-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&xml_dir);
        let products = Path::new("/mnt/iso/products");
        let p = write_driver_xml_in(&plan, &xml_dir, Some(products)).unwrap();
        let x = std::fs::read_to_string(&p).unwrap();
        assert!(
            x.contains(r"<EsdDirectory>Z:\mnt\iso\products\PHSP</EsdDirectory>"),
            "product EsdDirectory not absolute:\n{x}"
        );
        assert!(!x.contains("<EsdDirectory>./"), "a relative EsdDirectory survived:\n{x}");
        // InstallDir is unrelated to the payload location and must stay put.
        assert!(x.contains(r"<InstallDir>C:\Program Files\Adobe</InstallDir>"));
        let _ = std::fs::remove_dir_all(&xml_dir);
    }

    #[test]
    fn mixed_roots_modules_and_update_request() {
        let mut plan = phsp_plan();
        plan.modules = vec!["PPRO-ObjectMask".into(), "PPRO-MediaIntelligence".into()];
        let xml_dir = std::env::temp_dir().join(format!("mudhut-roots-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&xml_dir);
        let mut roots = BTreeMap::new();
        roots.insert("PHSP".to_string(), xml_dir.clone());
        roots.insert("COCM".to_string(), PathBuf::from("/var/cache/x/products"));
        let p = write_driver_xml_roots(&plan, &xml_dir, &roots, &Request { delete_user_preferences: Some(false) }).unwrap();
        let x = std::fs::read_to_string(&p).unwrap();
        assert!(x.contains("<EsdDirectory>./PHSP</EsdDirectory>"), "{x}");
        assert!(x.contains(r"<EsdDirectory>Z:\var\cache\x\products\COCM</EsdDirectory>"), "{x}");
        assert!(x.contains("<EsdDirectory>./CORE</EsdDirectory>"), "unlisted roots stay relative: {x}");
        assert!(x.contains("<Module>\n        <Id>PPRO-ObjectMask</Id>\n      </Module>"), "{x}");
        assert!(x.contains("<DeleteUserPreferences>false</DeleteUserPreferences>"), "{x}");
        let u = write_uninstall_xml(&xml_dir, "Audition", "AUDT", "26.3", "26.0", "win64").unwrap();
        let ux = std::fs::read_to_string(&u).unwrap();
        assert!(ux.contains("<CodexVersion>26.3</CodexVersion>") && !ux.contains("EsdDirectory") && !ux.contains("Dependenc"), "{ux}");
        let _ = std::fs::remove_dir_all(&xml_dir);
    }
}
