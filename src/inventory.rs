// SPDX-License-Identifier: Apache-2.0
//! `mudhut inventory`: which Adobe apps a prefix holds, at which build, and how they got there.
//! Read-only and wine-free (it runs on Collider's timer, beside open apps).
//!
//! Sources, per app folder under `Program Files\Adobe` (every year, not only the newest):
//! - Adobe's record, `caps/hdpim.db`: SAP code, codex, base version, BuildVersion, BuildGuid,
//!   install path, installed package versions and modules. The authority for what HDPIM manages.
//! - the exe's version resource: the binary that will actually run (corroboration).
//! - the Uninstall key HDPIM writes in this prefix's registry (`<SAP>_<maj>_<min>`): present when
//!   HDPIM installed the app HERE; a prefix copied from Windows carries Windows' hdpim.db but no
//!   registry.
//! - `AMT/application.xml` in the app folder: SAP code + codex version, which travels with a copy
//!   even when no record does.
//!
//! `managed_by` (contract field):
//! - `hdpim`: Adobe's record lists this folder and this prefix's registry has its Uninstall key.
//! - `windows-copy`: the record lists it (Windows' own hdpim.db came with the copy), no key here.
//!   Updatable exactly like an HDPIM install: the update keys off the record.
//! - `adopt-needed`: no record lists this folder (a copy without Windows' records). The update
//!   first lets HDPIM register the copied build, then updates it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::catalog::App;
use crate::hdpimdb::{Db, Product};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Managed {
    Hdpim,
    WindowsCopy,
    AdoptNeeded,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Sources {
    /// The exe's file version.
    pub exe_version: Option<String>,
    /// hdpim.db BuildVersion of the matching record.
    pub record_build: Option<String>,
    /// hdpim.db ProductVersion (codex) of the matching record.
    pub record_codex: Option<String>,
    /// DisplayVersion of the `<SAP>_<maj>_<min>` Uninstall key in this prefix.
    pub uninstall_key: Option<String>,
    /// AMT/application.xml ProductVersion (codex).
    pub amt_codex: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledApp {
    pub id: String,
    pub name: String,
    pub sap: String,
    /// The app folder ("Adobe Photoshop 2026").
    pub dir: String,
    pub install_dir: PathBuf,
    pub exe: Option<PathBuf>,
    pub year: Option<u16>,
    /// The best-known build: the record's BuildVersion, else the exe version, else the AMT codex.
    pub version: Option<String>,
    /// False when only a codex version ("26.5") is known, not a full build.
    pub exact: bool,
    pub codex: Option<String>,
    pub base: Option<String>,
    pub build_guid: Option<String>,
    pub managed_by: Managed,
    /// The record and the exe agree (when both exist).
    pub consistent: bool,
    pub sources: Sources,
    /// Installed package name -> version, from the record.
    pub packages: BTreeMap<String, String>,
    pub modules: Vec<String>,
    pub notes: Vec<String>,
}

impl InstalledApp {
    pub fn record<'a>(&self, db: Option<&'a Db>) -> Option<&'a Product> {
        let db = db?;
        db.products.iter().find(|p| p.sap == self.sap && Some(p.codex.as_str()) == self.codex.as_deref()
            && p.build() == self.sources.record_build.as_deref())
    }
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    /// The prefix exactly as the caller passed it (Collider keys on that string).
    pub prefix: String,
    pub ok: bool,
    pub error: Option<String>,
    pub apps: Vec<InstalledApp>,
    /// hdpim.db rows for catalog apps whose folder is not in this prefix (a copy from Windows
    /// carries the whole machine's record).
    pub records_without_folder: Vec<String>,
    #[serde(skip)]
    pub db: Option<Db>,
}

/// A 4-digit year in a folder name ("Adobe Photoshop 2026" -> 2026).
pub fn year_of(dir: &str) -> Option<u16> {
    let b = dir.as_bytes();
    (0..b.len().saturating_sub(3)).rev().find_map(|i| {
        let w = &dir[i..i + 4];
        let before_ok = i == 0 || !b[i - 1].is_ascii_digit();
        let after_ok = i + 4 >= b.len() || !b[i + 4].is_ascii_digit();
        if before_ok && after_ok && w.chars().all(|c| c.is_ascii_digit()) && (w.starts_with("19") || w.starts_with("20")) {
            w.parse().ok()
        } else {
            None
        }
    })
}

/// Every installed folder of `cat` in the prefix (by catalog prefix, with its exe present).
fn app_dirs(prefix: &Path, cat: &App) -> Vec<(PathBuf, PathBuf)> {
    let adobe = prefix.join("drive_c/Program Files/Adobe");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&adobe) else { return out };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.is_dir() && d.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with(cat.dir_prefix)).unwrap_or(false))
        .collect();
    dirs.sort();
    for d in dirs {
        let exe = d.join(cat.exe);
        if exe.is_file() {
            out.push((d, exe));
        }
    }
    out
}

/// `AMT/application.xml` (searched up to 4 levels under the app folder): (SAPCode, ProductVersion).
pub fn amt_record(app_dir: &Path) -> Option<(String, String)> {
    let mut q = std::collections::VecDeque::from([(app_dir.to_path_buf(), 0u32)]);
    let mut seen = 0;
    while let Some((d, depth)) = q.pop_front() {
        let cand = d.join("AMT").join("application.xml");
        if cand.is_file() {
            let text = std::fs::read_to_string(&cand).ok()?;
            let get = |key: &str| -> Option<String> {
                let needle = format!("<Data key=\"{key}\">");
                let i = text.find(&needle)? + needle.len();
                let j = text[i..].find('<')? + i;
                Some(text[i..j].trim().to_string())
            };
            return Some((get("SAPCode")?, get("ProductVersion")?));
        }
        if depth >= 4 || seen > 2048 {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                seen += 1;
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) && e.file_name() != "AMT" {
                    q.push_back((e.path(), depth + 1));
                }
            }
        }
    }
    None
}

/// Uninstall keys named `<SAP>_<digits>(_<digits>)*` in `system.reg`, as (name, DisplayVersion).
/// Neutron's own `PHSP27` key (for Lightroom Classic's Edit in Photoshop) has no underscore and
/// is excluded by the pattern.
pub fn uninstall_keys(system_reg: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(system_reg) else { return vec![] };
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        if line.starts_with('[') {
            cur = None;
            let lower = line.to_ascii_lowercase();
            if let Some(i) = lower.find("\\\\currentversion\\\\uninstall\\\\") {
                let rest = &line[i + "\\\\currentversion\\\\uninstall\\\\".len()..];
                let name = rest.split(']').next().unwrap_or("");
                let mut parts = name.split('_');
                let sap = parts.next().unwrap_or("");
                let nums: Vec<&str> = parts.collect();
                if !sap.is_empty() && !nums.is_empty() && nums.iter().all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
                    cur = Some(name.to_string());
                }
            }
        } else if let Some(name) = &cur {
            if let Some(v) = line.strip_prefix("\"DisplayVersion\"=\"") {
                out.push((name.clone(), v.trim_end_matches('"').to_string()));
            }
        }
    }
    out
}

/// The record (if any) whose launch path names this app folder. Matched on the folder name, not
/// the drive: a Windows install on D: still names "Adobe Premiere Pro 2026".
fn record_for<'a>(db: Option<&'a Db>, sap: &str, dir_name: &str) -> Option<&'a Product> {
    let db = db?;
    let want = dir_name.to_ascii_lowercase();
    let names = |p: &Product| -> bool {
        [p.app_launch(), p.meta.get("AMTConfig.path").cloned(), p.install_dir().map(str::to_string)]
            .into_iter()
            .flatten()
            .any(|s| s.replace('/', "\\").split('\\').any(|c| c.to_ascii_lowercase() == want))
    };
    db.products.iter().filter(|p| p.sap == sap && names(p)).max_by(|a, b| {
        crate::version::cmp(a.build().unwrap_or(&a.codex), b.build().unwrap_or(&b.codex))
    })
}

/// Read one prefix. Never fails for a missing or unreadable record: that is reported in `notes`.
pub fn inventory(prefix_as_passed: &str) -> Result<Inventory> {
    let prefix = Path::new(prefix_as_passed);
    if !prefix.join("drive_c").is_dir() {
        return Ok(Inventory {
            prefix: prefix_as_passed.to_string(),
            ok: false,
            error: Some("This is not a prefix (it has no drive_c folder).".into()),
            apps: vec![],
            records_without_folder: vec![],
            db: None,
        });
    }
    let mut notes_db = None;
    let db = match Db::read(prefix) {
        Ok(d) => d,
        Err(e) => {
            notes_db = Some(format!("Adobe's install record could not be read: {e:#}"));
            None
        }
    };
    let keys = uninstall_keys(&prefix.join("system.reg"));
    let mut apps = Vec::new();
    let mut matched: Vec<(String, String)> = Vec::new();
    for cat in crate::catalog::apps() {
        for (dir, exe) in app_dirs(prefix, &cat) {
            let dir_name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let rec = record_for(db.as_ref(), cat.sap, &dir_name);
            let exe_version = crate::pe::file_version_string(&exe);
            let amt = amt_record(&dir).filter(|(sap, _)| sap == cat.sap).map(|(_, v)| v);
            let codex = rec.map(|r| r.codex.clone()).or_else(|| amt.clone());
            let key = codex.as_deref().and_then(|c| {
                let name = format!("{}_{}", cat.sap, c.replace('.', "_"));
                keys.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone())
            });
            let record_build = rec.and_then(|r| r.build()).map(str::to_string);
            let (version, exact) = match (&record_build, &exe_version, &amt) {
                (Some(b), _, _) => (Some(b.clone()), true),
                (None, Some(e), _) => (Some(e.clone()), true),
                (None, None, Some(a)) => (Some(a.clone()), false),
                _ => (None, false),
            };
            let consistent = match (&record_build, &exe_version) {
                (Some(b), Some(e)) => crate::version::eq(b, e) || crate::version::label(b) == crate::version::label(e),
                _ => true,
            };
            let managed_by = match (rec, &key) {
                (Some(_), Some(_)) => Managed::Hdpim,
                (Some(_), None) => Managed::WindowsCopy,
                (None, _) => Managed::AdoptNeeded,
            };
            let mut notes = Vec::new();
            if let Some(n) = &notes_db {
                notes.push(n.clone());
            }
            if !consistent {
                notes.push(format!(
                    "Adobe's record says {} but the program file reports {}.",
                    record_build.clone().unwrap_or_default(),
                    exe_version.clone().unwrap_or_default()
                ));
            }
            if let Some(r) = rec {
                matched.push((r.sap.clone(), r.codex.clone()));
            }
            apps.push(InstalledApp {
                id: cat.id.to_string(),
                name: cat.name.trim_end_matches(" (experimental)").to_string(),
                sap: cat.sap.to_string(),
                year: year_of(&dir_name),
                dir: dir_name,
                install_dir: dir.clone(),
                exe: Some(exe.clone()),
                version,
                exact,
                codex,
                base: rec.and_then(|r| r.base()).map(str::to_string),
                build_guid: rec.and_then(|r| r.build_guid()).map(str::to_string),
                managed_by,
                consistent,
                sources: Sources {
                    exe_version,
                    record_build,
                    record_codex: rec.map(|r| r.codex.clone()),
                    uninstall_key: key,
                    amt_codex: amt,
                },
                packages: rec.map(|r| r.packages.clone()).unwrap_or_default(),
                modules: rec.map(|r| r.modules()).unwrap_or_default(),
                notes,
            });
        }
    }
    let catalog_saps: Vec<&str> = crate::catalog::apps().iter().map(|a| a.sap).collect();
    let records_without_folder = db
        .as_ref()
        .map(|d| {
            d.products
                .iter()
                .filter(|p| catalog_saps.contains(&p.sap.as_str()) && !matched.contains(&(p.sap.clone(), p.codex.clone())))
                .map(|p| format!("{} {}", p.sap, p.build().unwrap_or(&p.codex)))
                .collect()
        })
        .unwrap_or_default();
    Ok(Inventory { prefix: prefix_as_passed.to_string(), ok: true, error: None, apps, records_without_folder, db })
}

/// `mudhut inventory --prefix P [--prefix P ...]`.
pub fn cmd_inventory(em: &crate::output::Emitter, prefixes: &[String]) -> Result<()> {
    let mut out = Vec::new();
    for p in prefixes {
        let inv = inventory(p)?;
        if !em.is_json() {
            outln!("{}", inv.prefix);
            if let Some(e) = &inv.error {
                outln!("  {e}");
            }
            for a in &inv.apps {
                outln!(
                    "  {:<16} {:<28} {:<22} {}",
                    a.id,
                    a.dir,
                    a.version.clone().unwrap_or_else(|| "unknown".into()),
                    match a.managed_by {
                        Managed::Hdpim => "installed by Adobe's installer",
                        Managed::WindowsCopy => "copied from Windows (with Adobe's records)",
                        Managed::AdoptNeeded => "copied without Adobe's records",
                    }
                );
            }
        }
        out.push(inv);
    }
    if em.is_json() {
        em.result(&serde_json::json!({"ok": true, "schema": 1, "prefixes": out}));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn years_from_folder_names() {
        assert_eq!(year_of("Adobe Photoshop 2026"), Some(2026));
        assert_eq!(year_of("Adobe Lightroom Classic"), None);
        assert_eq!(year_of("Adobe Animate 2024"), Some(2024));
        assert_eq!(year_of("Thing 12026"), None);
    }

    #[test]
    fn uninstall_keys_exclude_neutrons_detector_key() {
        let dir = std::env::temp_dir().join(format!("mudhut-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("system.reg");
        std::fs::write(
            &reg,
            "WINE REGISTRY Version 2\n\n\
             [Software\\\\Wow6432Node\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Uninstall\\\\PHSP_27_8] 1\n\
             \"DisplayName\"=\"Adobe Photoshop 2026\"\n\"DisplayVersion\"=\"27.8.0.13\"\n\n\
             [Software\\\\Wow6432Node\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Uninstall\\\\PHSP27] 1\n\
             \"DisplayVersion\"=\"27.0\"\n\n\
             [Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Uninstall\\\\AUDT_26_5] 1\n\
             \"DisplayVersion\"=\"26.5\"\n",
        )
        .unwrap();
        let k = uninstall_keys(&reg);
        assert_eq!(k, vec![("PHSP_27_8".to_string(), "27.8.0.13".to_string()), ("AUDT_26_5".to_string(), "26.5".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mkapp(prefix: &Path, dir: &str, exe_rel: &str, amt: Option<(&str, &str)>) {
        let d = prefix.join("drive_c/Program Files/Adobe").join(dir);
        let exe = d.join(exe_rel);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"MZ").unwrap();
        if let Some((sap, v)) = amt {
            std::fs::create_dir_all(d.join("AMT")).unwrap();
            std::fs::write(
                d.join("AMT/application.xml"),
                format!("<Configuration><Payload><Data key=\"ProductVersion\">{v}</Data><Data key=\"SAPCode\">{sap}</Data></Payload></Configuration>"),
            )
            .unwrap();
        }
    }

    // The three kinds of prefix the contract names: an HDPIM install (record + key), a Windows
    // copy with Windows' record, and a copy with no record.
    #[test]
    fn classifies_hdpim_windows_copy_and_adopt_needed() {
        let p = std::env::temp_dir().join(format!("mudhut-inv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        mkapp(&p, "Adobe Audition 2026", "Adobe Audition.exe", Some(("AUDT", "26.5")));
        mkapp(&p, "Adobe Premiere Pro 2026", "Adobe Premiere Pro.exe", Some(("PPRO", "26.3")));
        mkapp(&p, "Adobe Animate 2024", "Animate.exe", Some(("FLPR", "24.0.10")));
        crate::hdpimdb::tests::make_db(
            &p.join(crate::hdpimdb::DB_REL),
            &[
                ("AUDT", "26.5", &[("BaseVersion", "26.0"), ("BuildVersion", "26.5.0.82"), ("InstallDir", "C:\\Program Files\\Adobe"),
                                    ("AppLaunch", "[INSTALLDIR]\\Adobe Audition 2026\\Adobe Audition.exe")], &[]),
                ("PPRO", "26.3", &[("BaseVersion", "26.0"), ("BuildVersion", "26.3.0.93"), ("InstallDir", "D:\\Adobe"),
                                    ("AppLaunch", "[INSTALLDIR]\\Adobe Premiere Pro 2026\\Adobe Premiere Pro.exe")], &[("AdobePremierePro26AllTrial", "26.3.0.93")]),
                ("AEFT", "26.3", &[("BaseVersion", "26.0"), ("BuildVersion", "26.3.0.87"), ("InstallDir", "C:\\Program Files\\Adobe"),
                                    ("AppLaunch", "[INSTALLDIR]\\Adobe After Effects 2026\\Support Files\\AfterFX.exe")], &[]),
            ],
        );
        std::fs::write(
            p.join("system.reg"),
            "[Software\\\\Wow6432Node\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Uninstall\\\\AUDT_26_5] 1\n\"DisplayVersion\"=\"26.5\"\n",
        )
        .unwrap();
        let inv = inventory(p.to_str().unwrap()).unwrap();
        let get = |id: &str| inv.apps.iter().find(|a| a.id == id).unwrap();
        assert_eq!(get("audition").managed_by, Managed::Hdpim);
        assert_eq!(get("audition").version.as_deref(), Some("26.5.0.82"));
        let pr = get("premiere");
        assert_eq!(pr.managed_by, Managed::WindowsCopy, "record names the folder (on another drive)");
        assert_eq!(pr.base.as_deref(), Some("26.0"));
        assert_eq!(pr.packages["AdobePremierePro26AllTrial"], "26.3.0.93");
        let an = get("animate");
        assert_eq!(an.managed_by, Managed::AdoptNeeded);
        assert_eq!(an.version.as_deref(), Some("24.0.10"));
        assert!(!an.exact);
        assert_eq!(an.year, Some(2024));
        assert_eq!(inv.records_without_folder, vec!["AEFT 26.3.0.87"]);
        let _ = std::fs::remove_dir_all(&p);
    }
}
