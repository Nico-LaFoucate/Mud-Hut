// SPDX-License-Identifier: Apache-2.0
//! `mudhut check-updates`: which Adobe updates exist for the apps in one or more prefixes.
//!
//! Safe on a timer and beside open apps: it never runs wine and never writes into a prefix
//! (only under `$XDG_CACHE_HOME/mudhut/`). Offline is a status, not an error: the cached feed
//! answers, `online:false`, exit 0.
//!
//! Per installed app (see `inventory`), against every build Adobe's feed lists for its SAP code:
//! - `update`: an in-place update, i.e. a newer build with the SAME base version (Adobe's own
//!   identity for "the same product": Photoshop 27.8 -> 27.11, Premiere 26.3 -> 26.5.2,
//!   Lightroom Classic 15.4 -> 15.6). Policy "hold": the RECOMMENDED target is the newest build
//!   on the verified list; when no newer build is verified, the newest build is offered with
//!   `verified:false` ("available, not yet verified by Neutron"), and the user may still choose
//!   it after a confirmation (`update --allow-unverified`).
//! - `newer_unverified`: the newest build, when it is newer than the recommended one and not
//!   verified.
//! - `new_major`: the newest build of a higher major with a different base (a new yearly
//!   version, installed beside the old one with `install --version`).
//! A copy from Windows is never blocked (operator decision, 2026-10-16): with Windows' records it
//! updates like an HDPIM install, without them it is adopted first (`update.adopt:true`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::feed::Candidate;
use crate::feedcache::{self, Mode};
use crate::inventory::{InstalledApp, Managed};
use crate::output::Emitter;

/// Apps tied together by Dynamic Link; updating one suggests the others (`related`).
pub const DYNAMIC_LINK: &[&str] = &["premiere", "aftereffects", "mediaencoder", "audition"];

pub struct CheckOpts {
    pub prefixes: Vec<String>,
    pub all_prefixes: bool,
    pub apps: Vec<String>,
    pub max_age: Duration,
    pub refresh: bool,
    pub offline: bool,
    pub cached: bool,
}

/// The prefixes neutron knows (`neutron --json prefix list`), one authority for the list.
pub fn neutron_prefixes() -> Result<Vec<String>> {
    let out = std::process::Command::new("neutron")
        .args(["--json", "prefix", "list"])
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| crate::failure::Failure::new("neutron-missing", crate::failure::EX_DEP, "The `neutron` command was not found. Run `neutron setup`."))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
        crate::failure::Failure::new(
            "neutron-too-old",
            crate::failure::EX_DEP,
            "This version of Neutron cannot list its prefixes. Run `neutron update`, or pass --prefix.",
        )
    })?;
    Ok(v["prefixes"].as_array().map(|a| a.iter().filter_map(|p| p["path"].as_str().map(str::to_string)).collect()).unwrap_or_default())
}

/// The installed build and base an app should be compared from. With Adobe's record that is
/// the record; a copy without one is matched to the feed build it is (by its exe version, else
/// its codex version), which also gives its base.
pub fn installed_identity(a: &InstalledApp, cands: &[Candidate]) -> (Option<String>, Option<String>, Option<Candidate>) {
    if a.managed_by != Managed::AdoptNeeded {
        return (a.version.clone(), a.base.clone(), cands.iter().find(|c| a.version.as_deref().map(|v| crate::version::eq(v, &c.product_version)).unwrap_or(false)).cloned());
    }
    let by_exe = a.sources.exe_version.as_deref().and_then(|v| cands.iter().find(|c| crate::version::eq(v, &c.product_version)));
    let by_codex = a.codex.as_deref().and_then(|v| cands.iter().find(|c| c.feed_version == v));
    if let Some(c) = by_exe.or(by_codex) {
        return (Some(c.product_version.clone()), Some(c.base()), Some(c.clone()));
    }
    // Not in the feed any more: keep what is known; the base is the one shared by the builds of
    // the same major (or the only base the product has, as for Lightroom Classic's 8.3).
    let v = a.version.clone();
    let mut bases: Vec<String> = cands.iter().map(|c| c.base()).collect();
    bases.dedup();
    let base = if bases.len() == 1 {
        Some(bases[0].clone())
    } else {
        v.as_deref().and_then(|v| cands.iter().find(|c| crate::version::major(&c.product_version) == crate::version::major(v)).map(|c| c.base()))
    };
    (v, base, None)
}

/// Adobe installs yearly versions side by side, so a newer yearly version is offered once per app:
/// on its newest installed year, and not at all when that year is installed already.
fn newer_year_installed(apps: &[InstalledApp], a: &InstalledApp, v: &str) -> bool {
    apps.iter().any(|o| {
        o.sap == a.sap
            && (o.version.as_deref().map(|ov| crate::version::major(ov) > crate::version::major(v)).unwrap_or(false)
                || matches!((o.year, a.year), (Some(x), Some(y)) if x > y))
    })
}

/// Bytes an update to `target` downloads: every package of its plan whose version differs from
/// the installed one (full packages; deltas are not counted).
fn changed_bytes(target: &crate::feed::DownloadPlan, installed: &std::collections::BTreeMap<String, String>) -> u64 {
    target.packages.iter().filter(|p| installed.get(&p.name).map(|v| !crate::version::eq(v, &p.version)).unwrap_or(true)).map(|p| p.bytes).sum()
}

/// The plan options for updating `a`: a recorded install keeps exactly its recorded modules;
/// a copy without a record gets the default set (everything but the speech packs).
pub fn update_plan_opts(a: &InstalledApp) -> crate::feed::PlanOpts {
    match a.managed_by {
        Managed::AdoptNeeded => crate::feed::PlanOpts::default(),
        _ => crate::feed::PlanOpts { only_modules: Some(a.modules.clone()), ..Default::default() },
    }
}

fn plan_for(ledger: &crate::ledger::Ledger, c: &Candidate, app_id: &str, opts: &crate::feed::PlanOpts, offline: bool) -> Option<crate::feed::DownloadPlan> {
    let (m, _) = feedcache::manifest(ledger, &c.build_guid, offline)?;
    Some(crate::feed::plan_with(&c.to_build(app_id), &m, "en_US", opts))
}

/// Bytes of every dependency the build names (an update stages them all, unchanged ones too:
/// HDPIM uninstalls a dependency the request leaves out). None when a manifest is missing.
fn deps_bytes(ledger: &crate::ledger::Ledger, deps_doc: Option<&str>, t: &crate::feed::DownloadPlan, offline: bool) -> Option<u64> {
    if t.dependencies.is_empty() {
        return Some(0);
    }
    let doc = roxmltree::Document::parse(deps_doc?).ok()?;
    let mut total = 0;
    for d in &t.dependencies {
        let c = ["win64", "win32"].iter().find_map(|plat| {
            let all = crate::feed::candidates_from_doc(&doc, &d.sap, plat);
            all.iter().find(|c| !d.base_version.is_empty() && crate::version::eq(&c.base(), &d.base_version)).cloned().or_else(|| all.first().cloned())
        })?;
        total += plan_for(ledger, &c, &d.sap.to_lowercase(), &crate::feed::PlanOpts::default(), offline)?.total_bytes;
    }
    Some(total)
}

/// Download size for an update (None when a manifest is not available): the whole planned build
/// plus every dependency it names. Files already on disk under another build are reused when
/// the update runs, so the real download can be smaller.
pub fn download_bytes(ledger: &crate::ledger::Ledger, deps_doc: Option<&str>, a: &InstalledApp, target: &Candidate, adopt_from: Option<&Candidate>, offline: bool) -> Option<u64> {
    let t = plan_for(ledger, target, &a.id, &update_plan_opts(a), offline)?;
    let deps = deps_bytes(ledger, deps_doc, &t, offline)?;
    // HDPIM's update needs every package of the build in its download folder (unchanged ones
    // too), so the download is the whole planned build; packages already on disk under another
    // build are reused when the update runs.
    if a.managed_by == Managed::AdoptNeeded {
        // Adoption installs the copied build through Adobe's installer first (the whole build),
        // then updates it.
        let from = adopt_from?;
        if from.build_guid == target.build_guid {
            return Some(t.total_bytes + deps);
        }
        let f = plan_for(ledger, from, &a.id, &crate::feed::PlanOpts::default(), offline)?;
        let same: u64 = t.packages.iter().filter(|p| f.packages.iter().any(|q| q.path == p.path)).map(|p| p.bytes).sum();
        return Some(f.total_bytes + t.total_bytes - same + deps);
    }
    Some(t.total_bytes + deps)
}

/// Bytes of the packages whose version changed (what is new in the build).
pub fn changed_download_bytes(ledger: &crate::ledger::Ledger, a: &InstalledApp, target: &Candidate, offline: bool) -> Option<u64> {
    let t = plan_for(ledger, target, &a.id, &update_plan_opts(a), offline)?;
    Some(changed_bytes(&t, &a.packages))
}

/// The prefix's runtime ("11.10-130") from its stamp, or None when no installed runtime matches.
pub fn prefix_runtime(prefix: &Path) -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let wine = crate::hdpim::runtime_for_prefix(&home, prefix)?;
    let root = wine.parent()?.parent()?;
    std::fs::read_to_string(root.join("NEUTRON_WINE_VERSION")).ok().map(|s| s.trim().to_string())
}

/// "11.10-130" >= "11.10-128".
pub fn runtime_at_least(have: &str, want: &str) -> bool {
    let k = |s: &str| -> Vec<u64> { s.split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect() };
    k(have) >= k(want)
}

/// One prefix of the check result.
fn check_prefix(
    prefix: &str,
    ledger: &crate::ledger::Ledger,
    docs: &std::collections::BTreeMap<String, String>,
    deps_doc: Option<&str>,
    verified: &crate::verified::Loaded,
    only: &[String],
    offline: bool,
) -> Value {
    let inv = match crate::inventory::inventory(prefix) {
        Ok(i) => i,
        Err(e) => return json!({"prefix": prefix, "ok": false, "error": format!("{e:#}"), "apps": []}),
    };
    if !inv.ok {
        return json!({"prefix": prefix, "ok": false, "error": inv.error, "apps_running": [], "update_in_progress": null, "last_update": null, "apps": []});
    }
    let p = Path::new(prefix);
    let busy = crate::busy::scan(p);
    let (running, last) = crate::state::report(p);
    let runtime = prefix_runtime(p);

    // Candidates per SAP, parsed once.
    let mut rows: Vec<(InstalledApp, Value)> = Vec::new();
    for a in inv.apps.iter().filter(|a| only.is_empty() || only.contains(&a.id)) {
        let platform = ledger.app(&a.id).map(|la| ledger.platform_for(la).to_string()).unwrap_or_else(|| "win64".into());
        let cands: Vec<Candidate> = docs
            .get(&platform)
            .and_then(|x| roxmltree::Document::parse(x).ok().map(|d| crate::feed::candidates_from_doc(&d, &a.sap, &platform)))
            .unwrap_or_default();
        let (iv, base, adopt_from) = installed_identity(a, &cands);
        let list = &verified.list;
        let is_verified = |c: &Candidate| list.verified(&a.sap, &c.base(), &c.product_version).is_some();
        let is_blocked = |c: &Candidate| list.blocked(&a.sap, &c.base(), &c.product_version).is_some();
        let newer: Vec<&Candidate> = match (&iv, &base) {
            (Some(v), Some(b)) => cands
                .iter()
                .filter(|c| crate::version::eq(&c.base(), b) && crate::version::cmp(&c.product_version, v) == std::cmp::Ordering::Greater && !is_blocked(c))
                .collect(),
            _ => vec![],
        };
        let rec = newer.iter().find(|c| is_verified(c)).copied().or(newer.first().copied());
        let newest = newer.first().copied();

        let mut update = Value::Null;
        if let Some(t) = rec {
            let entry = list.verified(&a.sap, &t.base(), &t.product_version);
            // Open apps do not block: the update downloads while they run and then waits for them
            // to close (0.3.6; `apps_running` still names them).
            let blocked_reason: Option<String> = if running.is_some() {
                Some("An update is already running in this prefix.".into())
            } else if !busy.installers.is_empty() {
                Some("Adobe's installer is running in this prefix. Wait for it to finish.".into())
            } else if let Some(why) = crate::hdpim::stamp_problem(p) {
                Some(why)
            } else if runtime.is_none() {
                Some("This prefix's Neutron runtime is not installed. Run `neutron update` first.".into())
            } else if let (Some(min), Some(have)) = (entry.and_then(|e| e.min_runtime.as_deref()), runtime.as_deref()) {
                (!runtime_at_least(have, min)).then(|| format!("This version needs a newer Neutron. Run `neutron update` first (it needs {min}, this prefix has {have})."))
            } else {
                None
            };
            update = json!({
                "kind": "in_place",
                "to": t.product_version,
                "label": crate::version::label(&t.product_version),
                "verified": entry.is_some(),
                "download_bytes": download_bytes(ledger, deps_doc, a, t, adopt_from.as_ref(), offline),
                "changed_bytes": changed_download_bytes(ledger, a, t, offline),
                "can_update": blocked_reason.is_none(),
                "blocked_reason": blocked_reason,
                "adopt": a.managed_by == Managed::AdoptNeeded,
            });
        }
        let newer_unverified = match (newest, rec) {
            (Some(n), Some(r)) if n.build_guid != r.build_guid && !is_verified(n) => {
                json!({"to": n.product_version, "label": crate::version::label(&n.product_version)})
            }
            _ => Value::Null,
        };
        let new_major = match (&iv, &base) {
            (Some(v), Some(_)) if newer_year_installed(&inv.apps, a, v) => Value::Null,
            (Some(v), Some(b)) => cands
                .iter()
                .filter(|c| !crate::version::eq(&c.base(), b) && crate::version::major(&c.product_version) > crate::version::major(v) && !is_blocked(c))
                .max_by(|x, y| crate::version::cmp(&x.product_version, &y.product_version))
                .map(|c| {
                    let year = a.year.map(|y| y as u64 + crate::version::major(&c.product_version) - crate::version::major(v));
                    json!({
                        "year": year,
                        "to": c.product_version,
                        "label": year.map(|y| y.to_string()).unwrap_or_else(|| crate::version::label(&c.product_version)),
                        "major": crate::version::major(&c.product_version),
                        "verified": is_verified(c),
                    })
                })
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let mut notes = a.notes.clone();
        if iv.is_none() || base.is_none() {
            notes.push("The installed version could not be matched to one Adobe offers, so updates cannot be checked.".into());
        }
        let row = json!({
            "id": a.id, "name": a.name, "sap": a.sap,
            "installed": {
                "version": iv, "base": base, "year": a.year, "managed_by": a.managed_by,
                "label": iv.as_deref().map(crate::version::label), "dir": a.dir,
            },
            "update": update,
            "newer_unverified": newer_unverified,
            "new_major": new_major,
            "related": [],
            "notes": notes,
        });
        rows.push((a.clone(), row));
    }
    // Dynamic Link partners in this prefix that also have an update.
    let with_update: Vec<String> = rows.iter().filter(|(_, r)| !r["update"].is_null()).map(|(a, _)| a.id.clone()).collect();
    let apps: Vec<Value> = rows
        .into_iter()
        .map(|(a, mut r)| {
            if DYNAMIC_LINK.contains(&a.id.as_str()) {
                let rel: Vec<&String> = with_update.iter().filter(|id| *id != &a.id && DYNAMIC_LINK.contains(&id.as_str())).collect();
                r["related"] = json!(rel);
            }
            r
        })
        .collect();
    json!({
        "prefix": prefix,
        "ok": true,
        "apps_running": busy.apps,
        "update_in_progress": running,
        "last_update": last,
        "runtime": runtime,
        "apps": apps,
    })
}

pub fn cmd_check_updates(em: &Emitter, o: CheckOpts) -> Result<()> {
    let mut prefixes = o.prefixes.clone();
    if o.all_prefixes {
        prefixes.extend(neutron_prefixes()?);
    }
    if prefixes.is_empty() {
        if !o.all_prefixes {
            anyhow::bail!("pass --prefix (repeatable) or --all-prefixes");
        }
        // No Neutron prefixes yet: nothing is installed, so there is nothing to check and no reason to ask Adobe.
        if !em.is_json() {
            outln!("No Neutron prefixes found.");
        }
        em.result(&json!({ "ok": true, "schema": 1, "checked_at": crate::clock::now(), "prefixes": [] }));
        return Ok(());
    }
    let offline = o.offline || o.cached;
    let mode = if offline { Mode::Offline } else if o.refresh { Mode::Refresh } else { Mode::Normal };
    let ledger = crate::ledger::Ledger::load_quick(em, offline).context("loading the download ledger")?;
    let verified = crate::verified::load(offline);

    // One feed per platform the apps use.
    let mut platforms: Vec<String> = ledger.apps.values().map(|a| ledger.platform_for(a).to_string()).collect();
    platforms.sort();
    platforms.dedup();
    let mut docs = std::collections::BTreeMap::new();
    let (mut online, mut from_cache, mut feed_error, mut fetched_at) = (true, false, None::<String>, None::<u64>);
    let mut feed_error_detail = None::<String>;
    for plat in &platforms {
        let f = feedcache::feed(&ledger, plat, o.max_age, mode);
        online &= f.online;
        from_cache |= f.from_cache;
        if feed_error.is_none() {
            feed_error = f.error.clone();
            feed_error_detail = f.error_detail.clone();
        }
        fetched_at = match (fetched_at, f.fetched_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(x) = f.xml {
            docs.insert(plat.clone(), x);
        }
    }
    // The shared components (Camera Raw, CCX, ...) are listed in the 32/64-bit feed.
    let deps_feed = feedcache::feed(&ledger, "win32,win64", o.max_age, mode);
    let now = crate::clock::now_secs();
    let next = match (online, fetched_at) {
        (true, Some(t)) => t + o.max_age.as_secs().max(60),
        _ => now + o.max_age.as_secs().clamp(60, 3600),
    };
    let rows: Vec<Value> = prefixes.iter().map(|p| check_prefix(p, &ledger, &docs, deps_feed.xml.as_deref(), &verified, &o.apps, offline)).collect();

    if !em.is_json() {
        if let Some(e) = &feed_error {
            outln!("{e}");
        }
        for r in &rows {
            outln!("{}", r["prefix"].as_str().unwrap_or(""));
            if let Some(e) = r["error"].as_str() {
                outln!("  {e}");
            }
            for a in r["apps"].as_array().into_iter().flatten() {
                let inst = a["installed"]["label"].as_str().unwrap_or("?");
                let up = &a["update"];
                let line = if up.is_null() {
                    "up to date".to_string()
                } else {
                    format!(
                        "{} available{}{}",
                        up["label"].as_str().unwrap_or(""),
                        if up["verified"].as_bool() == Some(true) { " (verified)" } else { " (not yet verified by Neutron)" },
                        up["blocked_reason"].as_str().map(|b| format!(" — {b}")).unwrap_or_default()
                    )
                };
                let major = a["new_major"]["label"].as_str().map(|l| format!("; {} {l} is out", a["name"].as_str().unwrap_or(""))).unwrap_or_default();
                outln!("  {:<16} {:<10} {line}{major}", a["id"].as_str().unwrap_or(""), inst);
            }
        }
    }
    em.result(&json!({
        "ok": true,
        "schema": 1,
        "checked_at": crate::clock::now(),
        // When Adobe's product list was last fetched from Adobe (this run or an earlier one);
        // Collider shows it as "Last checked".
        "feed": {
            "fetched_at": fetched_at.map(crate::clock::rfc3339),
            "source": if from_cache { "cache" } else { "network" },
        },
        "online": online,
        "feed_error": feed_error,
        "feed_error_detail": feed_error_detail,
        "from_cache": from_cache,
        "next_check_after": crate::clock::rfc3339(next),
        "verified_list": verified,
        "policy": "hold",
        "prefixes": rows,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(v: &str, base: &str) -> Candidate {
        Candidate { sap: "FLPR".into(), feed_version: crate::version::label(v), product_version: v.into(), base_version: Some(base.into()), build_guid: format!("g{v}"), platform: "win64".into() }
    }

    fn adopt_app(exe: Option<&str>, codex: &str) -> InstalledApp {
        InstalledApp {
            id: "animate".into(), name: "Animate".into(), sap: "FLPR".into(), dir: "Adobe Animate 2024".into(),
            install_dir: "/x".into(), exe: None, year: Some(2024), version: Some(codex.into()), exact: false,
            codex: Some(codex.into()), base: None, build_guid: None, managed_by: Managed::AdoptNeeded, consistent: true,
            sources: crate::inventory::Sources { exe_version: exe.map(str::to_string), amt_codex: Some(codex.into()), ..Default::default() },
            packages: Default::default(), modules: vec![], notes: vec![],
        }
    }

    #[test]
    fn a_copy_without_records_is_matched_to_its_feed_build() {
        let cands = vec![cand("24.0.15.5", "24.0"), cand("24.0.10.14", "24.0"), cand("23.0.17.4", "23.0")];
        // by codex (AMT "24.0.10" = feed version "24.0.10")
        let (v, b, from) = installed_identity(&adopt_app(None, "24.0.10"), &cands);
        assert_eq!((v.as_deref(), b.as_deref()), (Some("24.0.10.14"), Some("24.0")));
        assert_eq!(from.unwrap().build_guid, "g24.0.10.14");
        // by exe version (zero-padding differences are equal)
        let (v, _, _) = installed_identity(&adopt_app(Some("24.0.10.14"), "x"), &cands);
        assert_eq!(v.as_deref(), Some("24.0.10.14"));
    }

    #[test]
    fn a_newer_year_already_installed_is_not_offered_again() {
        let mut y2025 = adopt_app(None, "25.6.6");
        (y2025.year, y2025.version) = (Some(2025), Some("25.6.6.005".into()));
        let mut y2026 = adopt_app(None, "26.5");
        (y2026.year, y2026.version) = (Some(2026), Some("26.5.0.82".into()));
        let apps = vec![y2025.clone(), y2026.clone()];
        assert!(newer_year_installed(&apps, &y2025, "25.6.6.005"));
        assert!(!newer_year_installed(&apps, &y2026, "26.5.0.82"));
        // Another app's newer year does not count.
        let mut other = y2026.clone();
        other.sap = "AUDT".into();
        assert!(!newer_year_installed(&[y2025.clone(), other], &y2025, "25.6.6.005"));
    }

    #[test]
    fn runtime_order() {
        assert!(runtime_at_least("11.10-130", "11.10-128"));
        assert!(!runtime_at_least("11.10-61", "11.10-128"));
        assert!(runtime_at_least("11.11-1", "11.10-130"));
    }
}
