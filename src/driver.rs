// SPDX-License-Identifier: Apache-2.0
//! Generate the HyperDrive `driver.xml` — the install descriptor Adobe's
//! standalone `Setup.exe` consumes. Mirrors the adobe-packager layout: the target
//! product + its dependencies, each pointing at its per-SAP `EsdDirectory`
//! (`<dest>/<SAP>/`), plus the requested install dir + language.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::feed::DownloadPlan;

/// Write `<dest>/driver.xml` describing `plan`'s install. Returns the path.
pub fn write_driver_xml(plan: &DownloadPlan, dest: &Path) -> Result<PathBuf> {
    let name = if plan.name.to_lowercase().starts_with("adobe") {
        plan.name.clone()
    } else {
        format!("Adobe {}", plan.name)
    };

    let mut deps = String::new();
    for d in &plan.dependencies {
        deps.push_str(&format!(
            "      <Dependency>\n\
             \x20       <SAPCode>{}</SAPCode>\n\
             \x20       <BaseVersion>{}</BaseVersion>\n\
             \x20       <EsdDirectory>./{}</EsdDirectory>\n\
             \x20     </Dependency>\n",
            d.sap, d.base_version, d.sap
        ));
    }

    let xml = format!(
        "<DriverInfo>\n\
         \x20 <ProductInfo>\n\
         \x20   <Name>{name}</Name>\n\
         \x20   <SAPCode>{sap}</SAPCode>\n\
         \x20   <CodexVersion>{ver}</CodexVersion>\n\
         \x20   <BaseVersion>{base}</BaseVersion>\n\
         \x20   <Platform>{plat}</Platform>\n\
         \x20   <EsdDirectory>./{sap}</EsdDirectory>\n\
         \x20   <IsNonCCProduct>false</IsNonCCProduct>\n\
         \x20   <IsNglEnabled>true</IsNglEnabled>\n\
         \x20   <SupportedLanguages>\n\
         \x20     <Language locale=\"{lang}\"/>\n\
         \x20   </SupportedLanguages>\n\
         \x20   <Dependencies>\n\
         {deps}\
         \x20   </Dependencies>\n\
         \x20 </ProductInfo>\n\
         \x20 <RequestInfo>\n\
         \x20   <InstallDir>C:\\Program Files\\Adobe</InstallDir>\n\
         \x20   <InstallLanguage>{lang}</InstallLanguage>\n\
         \x20 </RequestInfo>\n\
         </DriverInfo>\n",
        name = name,
        sap = plan.sap,
        ver = plan.product_version,
        base = plan.base_version,
        plat = plan.platform,
        deps = deps,
        lang = plan.language,
    );

    let path = dest.join("driver.xml");
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
