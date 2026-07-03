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
         \x20   <Platform>{plat}</Platform>\n\
         \x20   <EsdDirectory>./{sap}</EsdDirectory>\n\
         \x20   <Dependencies>\n\
         {deps}\
         \x20   </Dependencies>\n\
         \x20 </ProductInfo>\n\
         \x20 <RequestInfo>\n\
         \x20   <InstallDir>/</InstallDir>\n\
         \x20   <InstallLanguage>{lang}</InstallLanguage>\n\
         \x20 </RequestInfo>\n\
         </DriverInfo>\n",
        name = name,
        sap = plan.sap,
        ver = plan.product_version,
        plat = plan.platform,
        deps = deps,
        lang = plan.language,
    );

    let path = dest.join("driver.xml");
    fs::write(&path, xml).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}
