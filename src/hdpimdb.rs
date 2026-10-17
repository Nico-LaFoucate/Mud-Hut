// SPDX-License-Identifier: Apache-2.0
//! Adobe's install record: `C:\Program Files (x86)\Common Files\Adobe\caps\hdpim.db`.
//!
//! HDPIM (and Creative Cloud on Windows) records every installed product here: one
//! `product_installation_info` row per (SAP code, codex version, processor family), its
//! key/value metadata (`BaseVersion`, `BuildVersion`, `BuildGuid`, `InstallDir`, `AppLaunch`,
//! `ConflictingProcessesXML`, `modules`, ...), and the version of every package it laid down.
//! Adobe's identity for "the same product" is (SAPCode, BaseVersion); an in-place update keeps
//! both and moves BuildVersion.
//!
//! Read-only, without wine: opened with `immutable=1`, so SQLite takes no lock and writes no
//! journal into the prefix. A torn read is possible only while an installer is writing, and the
//! callers skip the read when one runs in the prefix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// The database, relative to the prefix.
pub const DB_REL: &str = "drive_c/Program Files (x86)/Common Files/Adobe/caps/hdpim.db";

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub sap: String,
    /// The `ProductVersion` column: Adobe's codex version ("27.8", "26.3.1").
    pub codex: String,
    pub family: String,
    pub status: i64,
    pub meta: BTreeMap<String, String>,
    /// Installed package name -> package version.
    pub packages: BTreeMap<String, String>,
    /// Package name -> its key/value metadata (`TargetFolderList`, `UninstallPIMX`, ...).
    #[serde(skip)]
    pub package_meta: BTreeMap<String, BTreeMap<String, String>>,
}

impl Product {
    fn m(&self, k: &str) -> Option<&str> {
        self.meta.get(k).map(String::as_str).filter(|s| !s.is_empty())
    }
    pub fn base(&self) -> Option<&str> {
        self.m("BaseVersion")
    }
    pub fn build(&self) -> Option<&str> {
        self.m("BuildVersion")
    }
    pub fn build_guid(&self) -> Option<&str> {
        self.m("BuildGuid")
    }
    pub fn install_dir(&self) -> Option<&str> {
        self.m("InstallDir")
    }
    /// The modules installed with the product (`modules` meta key, comma-separated ids).
    pub fn modules(&self) -> Vec<String> {
        self.m("modules")
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default()
    }
    /// The app's executable as a Windows path: `AppLaunch` with `[INSTALLDIR]` replaced by
    /// `InstallDir` (Photoshop records InstallDir as the app folder and AppLaunch as
    /// `[INSTALLDIR]\Photoshop.exe`; Premiere records `C:\Program Files\Adobe` and
    /// `[INSTALLDIR]\Adobe Premiere Pro 2026\Adobe Premiere Pro.exe`).
    pub fn app_launch(&self) -> Option<String> {
        let launch = self.m("AppLaunch")?;
        let dir = self.install_dir().unwrap_or("");
        Some(replace_ci(launch, "[INSTALLDIR]", dir.trim_end_matches('\\')))
    }
}

/// Replace `token` case-insensitively (Adobe writes `[INSTALLDIR]` and `[InstallDir]`).
pub fn replace_ci(s: &str, token: &str, with: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let t = token.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(j) = lower[i..].find(&t) {
        out.push_str(&s[i..i + j]);
        out.push_str(with);
        i += j + t.len();
    }
    out.push_str(&s[i..]);
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Db {
    pub products: Vec<Product>,
}

impl Db {
    /// Read `<prefix>/<DB_REL>`. `Ok(None)` when the prefix has no database.
    pub fn read(prefix: &Path) -> Result<Option<Db>> {
        let path = prefix.join(DB_REL);
        if !path.is_file() {
            return Ok(None);
        }
        read_file(&path).map(Some)
    }

    /// The row Adobe would treat as "this product is installed": same SAP code and base version.
    pub fn product(&self, sap: &str, base: &str) -> Option<&Product> {
        self.products
            .iter()
            .filter(|p| p.sap == sap && p.base().map(|b| crate::version::eq(b, base)).unwrap_or(false))
            .max_by(|a, b| crate::version::cmp(a.build().unwrap_or(&a.codex), b.build().unwrap_or(&b.codex)))
    }

    pub fn products_for(&self, sap: &str) -> Vec<&Product> {
        self.products.iter().filter(|p| p.sap == sap).collect()
    }
}

/// `file:` URI for SQLite: everything outside the unreserved set percent-encoded (the path has
/// spaces and parentheses).
fn sqlite_uri(path: &Path) -> String {
    let mut s = String::from("file:");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s.push_str("?immutable=1");
    s
}

pub fn read_file(path: &Path) -> Result<Db> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(
        sqlite_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;

    let mut products: Vec<Product> = Vec::new();
    {
        let mut st = conn
            .prepare("SELECT SAPCode, ProductVersion, ProcessorFamily, Status FROM product_installation_info")
            .context("reading product_installation_info")?;
        let rows = st.query_map([], |r| {
            Ok(Product {
                sap: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                codex: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                family: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                status: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                meta: BTreeMap::new(),
                packages: BTreeMap::new(),
                package_meta: BTreeMap::new(),
            })
        })?;
        for r in rows {
            products.push(r?);
        }
    }
    let idx = |products: &Vec<Product>, sap: &str, ver: &str, fam: &str| {
        products.iter().position(|p| p.sap == sap && p.codex == ver && p.family == fam)
    };
    {
        let mut st = conn.prepare(
            "SELECT SAPCode, ProductVersion, ProcessorFamily, Key, Value FROM product_installation_meta_info",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })?;
        for r in rows {
            let (sap, ver, fam, k, v) = r?;
            if let Some(i) = idx(&products, &sap, &ver, &fam) {
                products[i].meta.insert(k, v);
            }
        }
    }
    {
        let mut st = conn.prepare(
            "SELECT SAPCode, ProductVersion, ProcessorFamily, PackageName, PackageVersion FROM package_installation_info",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })?;
        for r in rows {
            let (sap, ver, fam, name, pv) = r?;
            if let Some(i) = idx(&products, &sap, &ver, &fam) {
                products[i].packages.insert(name, pv);
            }
        }
    }
    {
        // ProcessorFamily here is the package's ("64-bit"), not the product's ("64Bit"), so
        // match on SAP + codex only.
        let mut st = conn.prepare(
            "SELECT SAPCode, ProductVersion, PackageName, Key, Value FROM package_installation_meta_info",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })?;
        for r in rows {
            let (sap, ver, pkg, k, v) = r?;
            if let Some(p) = products.iter_mut().find(|p| p.sap == sap && p.codex == ver) {
                p.package_meta.entry(pkg).or_default().insert(k, v);
            }
        }
    }
    Ok(Db { products })
}

/// A Windows path recorded by Adobe (`C:\Program Files\Adobe\...`) inside `prefix`. Only `C:`
/// paths map; each component is matched case-insensitively when the exact name is absent
/// (Windows names are case-insensitive, a Linux copy of them is not).
pub fn win_to_prefix(prefix: &Path, win: &str) -> Option<PathBuf> {
    let w = win.trim().replace('/', "\\");
    let rest = w.strip_prefix("C:\\").or_else(|| w.strip_prefix("c:\\"))?;
    let mut cur = prefix.join("drive_c");
    for comp in rest.split('\\').filter(|c| !c.is_empty()) {
        let exact = cur.join(comp);
        if exact.exists() {
            cur = exact;
            continue;
        }
        let want = comp.to_lowercase();
        let found = std::fs::read_dir(&cur).ok().and_then(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .find(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase() == want).unwrap_or(false))
        });
        cur = found.unwrap_or(exact);
    }
    Some(cur)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A synthetic hdpim.db with the real schema (from a Windows install's caps/hdpim.db).
    pub fn make_db(path: &Path, rows: &[(&str, &str, &[(&str, &str)], &[(&str, &str)])]) {
        let _ = std::fs::remove_file(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE product_installation_info ( SAPCode varchar(36), ProductVersion varchar(36), ProcessorFamily varchar(36), Status INT, constraint pk PRIMARY KEY (SAPCode, ProductVersion, ProcessorFamily) );
             CREATE TABLE package_installation_info ( SAPCode varchar(36), ProductVersion varchar(36), ProcessorFamily varchar(36), PackageName varchar(36), PackageVersion varchar(36), constraint pk PRIMARY KEY (SAPCode, ProductVersion, ProcessorFamily, PackageName, PackageVersion) );
             CREATE TABLE product_installation_meta_info ( SAPCode varchar(36), ProductVersion varchar(36), ProcessorFamily varchar(36), Key TEXT, Value TEXT, constraint pk PRIMARY KEY (SAPCode, ProductVersion, ProcessorFamily, Key) );
             CREATE TABLE package_installation_meta_info ( SAPCode varchar(36), ProductVersion varchar(36), ProcessorFamily varchar(36), PackageName  varchar(36), PackageVersion varchar(36), Key TEXT, Value TEXT, constraint pk PRIMARY KEY (SAPCode, ProductVersion, ProcessorFamily, PackageName, PackageVersion, Key) );",
        )
        .unwrap();
        for (sap, codex, meta, pkgs) in rows {
            c.execute("INSERT INTO product_installation_info VALUES (?1, ?2, '64Bit', 1)", [sap, codex]).unwrap();
            for (k, v) in *meta {
                c.execute(
                    "INSERT INTO product_installation_meta_info VALUES (?1, ?2, '64Bit', ?3, ?4)",
                    [sap, codex, k, v],
                )
                .unwrap();
            }
            for (n, v) in *pkgs {
                c.execute("INSERT INTO package_installation_info VALUES (?1, ?2, '64Bit', ?3, ?4)", [sap, codex, n, v])
                    .unwrap();
            }
        }
        // The base-version row Windows writes beside each product (not a product row itself).
        c.execute(
            "INSERT INTO product_installation_meta_info VALUES ('PPRO', '26.0', '64Bit', 'LatestInstalledVersion', '26.3')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn reads_products_meta_and_packages_without_writing() {
        let dir = std::env::temp_dir().join(format!("mudhut-hdpimdb-{}", std::process::id()));
        let db = dir.join("drive_c/Program Files (x86)/Common Files/Adobe/caps/hdpim.db");
        make_db(
            &db,
            &[(
                "PPRO",
                "26.3",
                &[
                    ("BaseVersion", "26.0"),
                    ("BuildVersion", "26.3.0.93"),
                    ("InstallDir", "C:\\Program Files\\Adobe"),
                    ("AppLaunch", "[INSTALLDIR]\\Adobe Premiere Pro 2026\\Adobe Premiere Pro.exe"),
                    ("modules", "PPRO-ObjectMask,PPRO-MediaIntelligence"),
                ],
                &[("AdobePremierePro26AllTrial", "26.3.0.93")],
            )],
        );
        let before: Vec<_> = std::fs::read_dir(db.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        let d = Db::read(&dir).unwrap().unwrap();
        let after: Vec<_> = std::fs::read_dir(db.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(before, after, "reading must not create journal files");
        assert_eq!(d.products.len(), 1);
        let p = d.product("PPRO", "26.0").unwrap();
        assert_eq!(p.build(), Some("26.3.0.93"));
        assert_eq!(p.app_launch().unwrap(), "C:\\Program Files\\Adobe\\Adobe Premiere Pro 2026\\Adobe Premiere Pro.exe");
        assert_eq!(p.modules(), vec!["PPRO-ObjectMask", "PPRO-MediaIntelligence"]);
        assert_eq!(p.packages["AdobePremierePro26AllTrial"], "26.3.0.93");
        assert!(d.product("PPRO", "25.0").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_paths_map_into_the_prefix_case_insensitively() {
        let dir = std::env::temp_dir().join(format!("mudhut-winpath-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("drive_c/Program Files/Adobe/Adobe Photoshop 2026")).unwrap();
        let p = win_to_prefix(&dir, "C:\\PROGRAM FILES\\adobe\\Adobe Photoshop 2026\\Photoshop.exe").unwrap();
        assert_eq!(p, dir.join("drive_c/Program Files/Adobe/Adobe Photoshop 2026/Photoshop.exe"));
        assert!(win_to_prefix(&dir, "D:\\x").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
