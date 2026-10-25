// SPDX-License-Identifier: Apache-2.0
//! `mudhut update`, `mudhut rollback`, `mudhut snapshots`.
//!
//! An update is Adobe's own: HDPIM's `hdpimInstallUpdate` with the newer build's packages (the
//! same DriverInfo as an install, plus `DeleteUserPreferences=false`). HDPIM removes the old
//! package set by its own uninstall lists and lays down the new one; preferences, presets and
//! third-party plug-ins stay (lab-proven on Audition by agent B, 2026-10-16).
//!
//! Order, so that nothing changes until everything that can fail without changing anything has
//! succeeded:
//!   lock -> plan -> preflight (apps closed, runtime, disk) -> download + verify ->
//!   end the prefix's wine session -> snapshot -> apply (HDPIM) -> verify -> provision.
//! Any failure after the snapshot puts the snapshot back (automatic rollback).
//!
//! Prefixes copied from Windows (operator decision, 2026-10-16: they MUST be updatable):
//! - with Windows' records (caps/hdpim.db rows): updated exactly like an HDPIM install;
//! - without records: ADOPTED first. HDPIM installs the very build the copy is (its records
//!   then exist, its uninstall lists name the copy's files) over the copied folder, and the
//!   update then runs as usual, so HDPIM removes the old build's files by Adobe's own lists and
//!   files the user added (plug-ins) stay. When Adobe no longer offers the copied build, the
//!   newer build is installed over the copy directly (files the old build had and the new one
//!   does not stay behind; said in the result).
//!
//! The run survives its reader: output ignores a closed pipe, SIGHUP is ignored, every event also
//! goes to `<prefix>/.mudhut/update.log`, and `<prefix>/.mudhut/state.json` says what is running,
//! so `check-updates` can show an update that outlived the Collider window that started it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::failure::{Failure, EX_DEP, EX_FAIL, EX_NET, EX_ROLLBACK_FAILED, EX_ROLLED_BACK, EX_RUNNING};
use crate::feed::{Candidate, DownloadPlan};
use crate::feedcache::{self, Mode};
use crate::hdpim::{Expect, Op};
use crate::inventory::{InstalledApp, Managed};
use crate::output::Emitter;
use crate::state;

pub struct UpdateOpts {
    pub prefix: String,
    pub apps: Vec<String>,
    pub all: bool,
    pub to: Option<String>,
    pub allow_unverified: bool,
    pub no_snapshot: bool,
    pub dry_run: bool,
    pub keep_download: bool,
    /// Products dirs holding packages to reuse (an offline package, a kept download).
    pub sources: Vec<PathBuf>,
}

/// One app's planned update.
struct Job {
    app: InstalledApp,
    from: Option<String>,
    target: Candidate,
    verified: bool,
    /// For a copy without records: the copied build to register first (None: install the
    /// target over the copy directly).
    adopt: Option<Candidate>,
    adopt_needed: bool,
    plan: Option<DownloadPlan>,
    raw_manifest: Option<String>,
    adopt_plan: Option<(DownloadPlan, String)>,
    /// Dependency components whose build changes: (plan, raw manifest).
    deps: Vec<(DownloadPlan, String)>,
    download_bytes: u64,
    extract_bytes: u64,
    roots: BTreeMap<String, PathBuf>,
    adopt_roots: BTreeMap<String, PathBuf>,
}

/// The outcome of one app, as the `app_result` event and the terminal result's `apps[]`.
#[derive(Debug, Clone)]
struct Outcome {
    app: String,
    ok: bool,
    from: Option<String>,
    to: Option<String>,
    /// "updated" | "up_to_date" | "unchanged" | "rolled_back" | "incomplete".
    state: String,
    message: String,
    exit: u8,
    extra: serde_json::Map<String, Value>,
}

impl Outcome {
    fn json(&self) -> Value {
        let mut v = json!({"event":"app_result","app":self.app,"ok":self.ok,"from":self.from,"to":self.to});
        if !self.ok || self.state != "updated" {
            v["state"] = json!(self.state);
            v["message"] = json!(self.message);
        }
        for (k, x) in &self.extra {
            v[k] = x.clone();
        }
        v
    }
}

fn name_of(app_id: &str) -> String {
    crate::catalog::find(app_id).map(|a| a.name.trim_end_matches(" (experimental)").to_string()).unwrap_or_else(|| app_id.into())
}

/// Plain "Close X first." Failure for a busy prefix.
fn busy_failure(b: &crate::busy::Busy, doing: &str) -> anyhow::Error {
    let names = b.blocking_names();
    Failure::new(
        if b.apps.is_empty() && b.other.is_empty() { "installer-running" } else { "app-running" },
        EX_RUNNING,
        format!("{} {doing} needs every Adobe app in this prefix closed. Nothing was changed.", crate::busy::close_first_sentence(&names)),
    )
    .with("prefix_changed", false)
    .with("apps_running", json!(names))
    .into()
}

/// `mudhut repair`: the seeding step of an install/update on its own (absent files + pim.db records),
/// with the prefix's wine session ended first, as an update does.
pub(crate) fn cmd_repair(em: &Emitter, prefix: &Path) -> Result<()> {
    if !prefix.join("drive_c").is_dir() {
        anyhow::bail!("{} is not a prefix (no drive_c)", prefix.display());
    }
    // Hold the prefix as install and update do: `neutron launch` refuses meanwhile, and a repair
    // cannot run in the middle of another Mud Hut job.
    let _lock = state::lock(prefix, "repair")?;
    quiesce(em, prefix)?;
    let cfg = crate::hdpim::discover(&crate::download::repo_tools_dir()?, crate::download::accc_packages_dir(em)?, Some(prefix))?;
    crate::hdpim::seed_runtime(em, &cfg, prefix)?;
    em.result(&json!({ "ok": true, "prefix": prefix, "repaired": ["desktop-components"] }));
    Ok(())
}

/// End the prefix's wine session with no app open: Adobe's helpers are closed by
/// `neutron teardown` (the full, graceful one; it runs only when no app is open), then wait for
/// every process of the prefix to be gone, so the registry files on disk are complete.
fn quiesce(em: &Emitter, prefix: &Path) -> Result<()> {
    let b = crate::busy::scan(prefix);
    if b.blocking() {
        return Err(busy_failure(&b, "This"));
    }
    if !b.any {
        return Ok(());
    }
    if !b.helpers.is_empty() {
        em.note(&format!("closing Adobe's background helpers ({})", b.helpers.join(", ")));
        let _ = std::process::Command::new("neutron")
            .args(["--json", "teardown", "--prefix"])
            .arg(prefix)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let b = crate::busy::scan(prefix);
        if !b.any {
            return Ok(());
        }
        if b.blocking() {
            return Err(busy_failure(&b, "This"));
        }
        if std::time::Instant::now() >= deadline {
            let left: Vec<String> = crate::busy::prefix_procs(prefix).iter().map(|p| p.exe_name()).collect();
            return Err(Failure::new(
                "prefix-busy",
                EX_RUNNING,
                format!("Something is still running in this prefix ({}). Close it, then try again. Nothing was changed.", left.join(", ")),
            )
            .with("prefix_changed", false)
            .into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn set_progress(prefix: &Path, st: &mut state::State, stage: &str) {
    if let Some(p) = st.in_progress.as_mut() {
        p.stage = stage.into();
    }
    let _ = state::write_state(prefix, st);
}

/// Adobe's product list for `feed_platform` ("win64", or "win32,win64" for the shared
/// components, which are often 32-bit-labeled): the cache when fresh (1 h), else one fetch.
fn feed_xml(ledger: &crate::ledger::Ledger, feed_platform: &str) -> Result<String> {
    let f = feedcache::feed(ledger, feed_platform, Duration::from_secs(3600), Mode::Normal);
    f.xml.ok_or_else(|| {
        Failure::new("network", EX_NET, f.error.unwrap_or_else(|| "Couldn't reach Adobe's servers.".into()) + " Nothing was changed.")
            .with("prefix_changed", false)
            .into()
    })
}

/// Feed candidates for `sap` on `platform`.
fn candidates(ledger: &crate::ledger::Ledger, platform: &str, sap: &str) -> Result<Vec<Candidate>> {
    let xml = feed_xml(ledger, platform)?;
    let doc = roxmltree::Document::parse(&xml).context("parsing Adobe's product list")?;
    Ok(crate::feed::candidates_from_doc(&doc, sap, platform))
}

/// The manifest of `c`, from the forever cache or one fetch (network failure: exit 5).
fn manifest(ledger: &crate::ledger::Ledger, c: &Candidate) -> Result<(crate::feed::Manifest, String)> {
    feedcache::manifest(ledger, &c.build_guid, false).ok_or_else(|| {
        Failure::new("network", EX_NET, format!("Couldn't download Adobe's description of {} {}. Nothing was changed.", c.sap, c.product_version))
            .with("prefix_changed", false)
            .into()
    })
}

fn plan_shallow(p: &DownloadPlan) -> DownloadPlan {
    DownloadPlan {
        app: p.app.clone(),
        sap: p.sap.clone(),
        name: p.name.clone(),
        product_version: p.product_version.clone(),
        base_version: p.base_version.clone(),
        platform: p.platform.clone(),
        build_guid: p.build_guid.clone(),
        language: p.language.clone(),
        packages: p.packages.clone(),
        total_bytes: p.total_bytes,
        dependencies: p.dependencies.clone(),
        modules: p.modules.clone(),
        skipped_modules: p.skipped_modules.clone(),
        skipped_bytes: p.skipped_bytes,
    }
}

/// Every dependency the target build names, resolved to the newest build of its base, with its
/// plan + raw manifest. ALL of them go into the request and are staged in full, unchanged ones
/// too.
///
/// 🚨 HDPIM takes the request's dependency list as the product's new set of references and
/// UNINSTALLS a dependency no request lists any more when nothing else refers to it. Measured
/// (lab, 2026-10-16, Photoshop 27.8 -> 27.11 with the unchanged dependencies left out of the
/// request): "Starting 'UN-INSTALL' task for Package (Name: AdobeUXPWebViewSupport_x64-mul ...)",
/// the Adobe Color profiles, UAM and the 3D viewer plug-in were removed, and their records with
/// them. On the copy of a Windows machine they survived only because After Effects and Premiere
/// still referred to them. And like the app's own unchanged packages, an unchanged dependency
/// must be present in the ESD folder (HDPIM: "selected but only DB operation required").
fn plan_deps(ledger: &crate::ledger::Ledger, plan: &mut DownloadPlan) -> Result<Vec<(DownloadPlan, String)>> {
    let mut deps = Vec::new();
    let mut keep = Vec::new();
    if plan.dependencies.is_empty() {
        return Ok(deps);
    }
    let xml = feed_xml(ledger, "win32,win64")?;
    let doc = roxmltree::Document::parse(&xml).context("parsing Adobe's product list")?;
    for d in plan.dependencies.clone() {
        let mut picked = None;
        for platform in ["win64", "win32"] {
            let all = crate::feed::candidates_from_doc(&doc, &d.sap, platform);
            let same_base = all.iter().find(|c| !d.base_version.is_empty() && crate::version::eq(&c.base(), &d.base_version)).cloned();
            if let Some(c) = same_base.or_else(|| all.first().cloned()) {
                picked = Some(c);
                break;
            }
        }
        let Some(c) = picked else { continue };
        let (m, raw) = manifest(ledger, &c)?;
        let dp = crate::feed::plan(&c.to_build(&d.sap.to_lowercase()), &m, "en_US");
        keep.push(d.clone());
        deps.push((dp, raw));
    }
    plan.dependencies = keep;
    Ok(deps)
}

/// Yearly versions sit side by side, and `--to` names one build of one of them: the install of
/// that build's yearly version is the one to update, the others of the same app are left alone.
/// (With no install of that year, nothing is skipped and planning explains the build.)
fn other_year_of_target(apps: &[InstalledApp], a: &InstalledApp, to: Option<&str>) -> bool {
    let Some(want) = to else { return false };
    let year_of = |x: &InstalledApp| x.version.as_deref().map(crate::version::major);
    let target = crate::version::major(want);
    year_of(a) != Some(target) && apps.iter().any(|x| x.id == a.id && year_of(x) == Some(target))
}

fn plan_job(
    ledger: &crate::ledger::Ledger,
    verified: &crate::verified::List,
    app: &InstalledApp,
    to: Option<&str>,
    allow_unverified: bool,
) -> Result<Option<Job>> {
    let platform = ledger.app(&app.id).map(|la| ledger.platform_for(la).to_string()).unwrap_or_else(|| "win64".into());
    let cands = candidates(ledger, &platform, &app.sap)?;
    let (iv, base, adopt_from) = crate::updates::installed_identity(app, &cands);
    let Some(base) = base else {
        return Err(Failure::new(
            "unknown-version",
            EX_FAIL,
            format!("The installed {} could not be matched to a version Adobe offers, so it cannot be updated here.", app.name),
        )
        .with("prefix_changed", false)
        .into());
    };
    let is_verified = |c: &Candidate| verified.verified(&app.sap, &c.base(), &c.product_version).is_some();
    let blocked = |c: &Candidate| verified.blocked(&app.sap, &c.base(), &c.product_version);
    let newer: Vec<&Candidate> = cands
        .iter()
        .filter(|c| crate::version::eq(&c.base(), &base) && iv.as_deref().map(|v| crate::version::cmp(&c.product_version, v) == std::cmp::Ordering::Greater).unwrap_or(true))
        .collect();
    let target = match to {
        Some(want) => {
            let Some(c) = cands.iter().find(|c| crate::version::eq(&c.product_version, want) || c.feed_version == want) else {
                return Err(Failure::new("build-gone", EX_FAIL, format!("Adobe does not offer {} {want}.", app.name)).with("prefix_changed", false).into());
            };
            if !crate::version::eq(&c.base(), &base) {
                return Err(Failure::new(
                    "new-major",
                    EX_FAIL,
                    format!("{} {want} is a new yearly version, not an update. Install it beside the current one with `mudhut install {} --version {}`.", app.name, app.id, crate::version::major(&c.product_version)),
                )
                .with("prefix_changed", false)
                .into());
            }
            if !newer.iter().any(|n| n.build_guid == c.build_guid) {
                return Ok(None); // not newer than what is installed: nothing to do
            }
            c.clone()
        }
        None => match newer.iter().find(|c| is_verified(c) && blocked(c).is_none()).or(newer.iter().find(|c| blocked(c).is_none())) {
            Some(c) => (*c).clone(),
            None => return Ok(None),
        },
    };
    if let Some(b) = blocked(&target) {
        return Err(Failure::new(
            "blocked-build",
            EX_FAIL,
            format!("{} {} has a known problem on Neutron: {}", app.name, crate::version::label(&target.product_version), b.reason),
        )
        .with("prefix_changed", false)
        .into());
    }
    let verified_target = is_verified(&target);
    if !verified_target && !allow_unverified {
        return Err(Failure::new(
            "unverified",
            EX_FAIL,
            format!(
                "{} {} is not yet tested by Neutron. There may be issues we are currently unaware of. To update anyway, confirm (--allow-unverified).",
                app.name,
                crate::version::label(&target.product_version)
            ),
        )
        .with("prefix_changed", false)
        .with("to", target.product_version.clone())
        .into());
    }
    let adopt_needed = app.managed_by == Managed::AdoptNeeded;
    Ok(Some(Job {
        app: app.clone(),
        from: iv,
        verified: verified_target,
        adopt: if adopt_needed { adopt_from.filter(|a| a.build_guid != target.build_guid) } else { None },
        adopt_needed,
        target,
        plan: None,
        raw_manifest: None,
        adopt_plan: None,
        deps: vec![],
        download_bytes: 0,
        extract_bytes: 0,
        roots: BTreeMap::new(),
        adopt_roots: BTreeMap::new(),
    }))
}

/// Resolve the job's plans and sizes (network: manifests).
fn resolve_job(ledger: &crate::ledger::Ledger, job: &mut Job) -> Result<()> {
    let (m, raw) = manifest(ledger, &job.target)?;
    let opts = crate::updates::update_plan_opts(&job.app);
    let mut plan = crate::feed::plan_with(&job.target.to_build(&job.app.id), &m, "en_US", &opts);
    job.deps = plan_deps(ledger, &mut plan)?;
    // ⛔ EVERY planned package is staged, not only the changed ones. HDPIM's update mode checks
    // that each package it selects is in the ESD folder, including an unchanged one it only
    // needs "a DB operation" for: Photoshop 27.8 -> 27.11 with only the changed packages staged
    // failed with "Package Zip './PHSP\AdobePhotoshop27-SuperCafModels.zip' is not present in
    // ESD Mode" (error 182; lab, AE2026 copy, 2026-10-16). An unchanged package that is already
    // on disk under another build is reused instead of downloaded (fetch_packages).
    if job.adopt_needed {
        if let Some(a) = &job.adopt {
            let (am, araw) = manifest(ledger, a)?;
            let mut ap = crate::feed::plan(&a.to_build(&job.app.id), &am, "en_US");
            // The adoption install names the same dependencies as the update.
            ap.dependencies = plan.dependencies.clone();
            job.adopt_plan = Some((ap, araw));
        }
    }
    let staged = plan_shallow(&plan);
    job.download_bytes = staged.total_bytes
        + job.adopt_plan.as_ref().map(|(p, _)| p.total_bytes).unwrap_or(0)
        + job.deps.iter().map(|(d, _)| d.total_bytes).sum::<u64>();
    let ext = |p: &DownloadPlan| -> u64 { p.packages.iter().map(|x| if x.extract_bytes > 0 { x.extract_bytes } else { x.bytes * 2 }).sum() };
    job.extract_bytes = ext(&staged) + job.adopt_plan.as_ref().map(|(p, _)| ext(p)).unwrap_or(0) + job.deps.iter().map(|(d, _)| ext(d)).sum::<u64>();
    job.plan = Some(staged);
    job.raw_manifest = Some(raw);
    Ok(())
}

/// Download (verify) the job's packages into the cache; returns nothing, fills `roots`.
fn download_job(em: &Emitter, ledger: &crate::ledger::Ledger, job: &mut Job) -> Result<()> {
    let plan = job.plan.as_ref().expect("resolved");
    let dir = crate::download::cache_products_dir(plan)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let net = |e: anyhow::Error| -> anyhow::Error {
        if crate::failure::find(&e).is_some() {
            return e;
        }
        Failure::new("download-failed", EX_NET, format!("The download from Adobe failed ({e:#}). Nothing was changed; downloaded packages are kept for the next try."))
            .with("prefix_changed", false)
            .into()
    };
    crate::download::write_application_json(&dir, &plan.sap, job.raw_manifest.as_deref().unwrap_or("{}"))?;
    crate::download::fetch_packages(em, ledger, plan, &dir).map_err(net)?;
    job.roots.insert(plan.sap.clone(), dir.clone());
    for (dp, raw) in &job.deps {
        let ddir = crate::download::cache_products_dir(dp)?;
        std::fs::create_dir_all(&ddir)?;
        crate::download::write_application_json(&ddir, &dp.sap, raw)?;
        crate::download::fetch_packages(em, ledger, dp, &ddir).map_err(net)?;
        job.roots.insert(dp.sap.clone(), ddir);
    }
    if let Some((ap, araw)) = &job.adopt_plan {
        let adir = crate::download::cache_products_dir(ap)?;
        std::fs::create_dir_all(&adir)?;
        crate::download::write_application_json(&adir, &ap.sap, araw)?;
        crate::download::fetch_packages(em, ledger, ap, &adir).map_err(net)?;
        job.adopt_roots.insert(ap.sap.clone(), adir);
        for (k, v) in &job.roots {
            if k != &ap.sap {
                job.adopt_roots.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(())
}

/// Apply one job: (adopt) + update through HDPIM, then check the result.
fn apply_job(em: &Emitter, cfg: &crate::hdpim::Config, prefix: &Path, job: &Job) -> Result<PathBuf> {
    let cat = crate::catalog::find(&job.app.id).context("unknown app")?;
    let plan = job.plan.as_ref().expect("resolved");
    em.progress("apply", 5, "preparing the prefix for Adobe's installer");
    crate::hdpim::setup_prefix(em, cfg, prefix)?;
    crate::hdpim::seed_runtime(em, cfg, prefix)?;
    if let Some((ap, _)) = &job.adopt_plan {
        em.progress("apply", 20, &format!("registering the copied {} {} with Adobe's installer", job.app.name, ap.product_version));
        let xml_dir = job.adopt_roots.get(&ap.sap).cloned().context("adopt staged")?;
        let drv = crate::driver::write_driver_xml_roots(ap, &xml_dir, &job.adopt_roots, &crate::driver::Request::default())?;
        let expect = Expect { sap: ap.sap.clone(), base: ap.base_version.clone(), build: Some(ap.product_version.clone()) };
        crate::hdpim::run_hdpim(em, cfg, prefix, Op::Install, &drv, &xml_dir, &expect, &cat)?;
    }
    // A recorded install (or a just-adopted copy) gets Adobe's update entry point; a copy whose
    // build Adobe no longer offers gets the newer build installed over it.
    let op = if job.adopt_needed && job.adopt.is_none() { Op::Install } else { Op::Update };
    em.progress("apply", 40, &format!("{} {} -> {} (Adobe's installer)", job.app.name, job.from.as_deref().map(crate::version::label).unwrap_or_default(), crate::version::label(&plan.product_version)));
    let xml_dir = job.roots.get(&plan.sap).cloned().context("staged")?;
    let req = crate::driver::Request { delete_user_preferences: Some(false) };
    let drv = crate::driver::write_driver_xml_roots(plan, &xml_dir, &job.roots, &req)?;
    let expect = Expect { sap: plan.sap.clone(), base: plan.base_version.clone(), build: Some(plan.product_version.clone()) };
    crate::hdpim::run_hdpim(em, cfg, prefix, op, &drv, &xml_dir, &expect, &cat)?;

    em.set_stage(None);
    em.progress("verify", 90, "checking the updated app");
    let exe = crate::hdpim::installed_exe_for(prefix, &cat, &plan.sap, &plan.base_version)
        .context("Adobe's installer finished, but the app's program file is missing")?;
    if let Some(v) = crate::pe::file_version_string(&exe) {
        let (want, got) = (crate::version::key(&plan.product_version), crate::version::key(&v));
        if want.get(..2) != got.get(..2) {
            anyhow::bail!("Adobe's installer finished, but the program file still reports {v} instead of {}.", plan.product_version);
        }
    }
    crate::download::seed_ui_fonts(em, &exe, prefix);
    Ok(exe)
}

/// Roll back after a failure: end the wine session, put the snapshot back (all three hives),
/// re-provision. Returns notes; Err = the rollback itself failed.
/// The wine this prefix is stamped for (provision keeps the prefix on it).
fn stamped_wine(prefix: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    crate::hdpim::runtime_for_prefix(&home, prefix)
}

fn auto_rollback(em: &Emitter, prefix: &Path, dir: &Path, meta: &mut crate::snapshot::Meta) -> Result<Vec<String>> {
    em.progress("rollback", 10, "putting the previous version back");
    quiesce(em, prefix)?;
    let notes = crate::snapshot::restore(prefix, dir, meta, crate::snapshot::HIVES)?;
    em.progress("rollback", 80, "neutron prefix provision");
    let _ = crate::install::provision_quiet(prefix, stamped_wine(prefix).as_deref());
    em.progress("rollback", 100, "previous version restored");
    Ok(notes)
}

pub fn cmd_update(em: &Emitter, o: UpdateOpts) -> Result<()> {
    let prefix_s = o.prefix.clone();
    let prefix = Path::new(&prefix_s);
    if !prefix.join("drive_c").is_dir() {
        return Err(Failure::new("bad-prefix", crate::failure::EX_BADPREFIX, format!("{} is not a prefix (it has no drive_c folder).", prefix.display())).with("prefix_changed", false).into());
    }
    if o.apps.is_empty() && !o.all {
        anyhow::bail!("name the apps to update (e.g. `mudhut update --prefix P photoshop`) or pass --all");
    }
    // A closing terminal must not end an update half-way.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    if !o.sources.is_empty() {
        let mut v: Vec<PathBuf> = std::env::var_os("MUDHUT_PACKAGE_SOURCES").map(|x| std::env::split_paths(&x).collect()).unwrap_or_default();
        v.extend(o.sources.iter().cloned());
        if let Ok(j) = std::env::join_paths(v) {
            std::env::set_var("MUDHUT_PACKAGE_SOURCES", j);
        }
    }

    // One Mud Hut job per prefix; the prefix itself is taken only after the download (see
    // hdpim::wait_for_prefix), so the apps stay usable while it runs.
    let mut lock = if o.dry_run { None } else { Some(state::job_lock(prefix, "update")?) };
    if !o.dry_run {
        em.event(&json!({"event": "prefix_lock", "held": false}), "");
        let log = state::log_path(prefix);
        let _ = std::fs::rename(&log, log.with_extension("log.1"));
        let _ = em.attach_log(&log);
        std::env::set_var("MUDHUT_HDPIM_LOG_DIR", state::logs_dir(prefix));
    }
    let mut st = state::read_state(prefix);

    // Preflight that needs nothing from the network.
    if which("neutron").is_none() {
        return Err(Failure::new("neutron-missing", EX_DEP, "The `neutron` command was not found. Run `neutron setup`. Nothing was changed.").with("prefix_changed", false).into());
    }
    em.progress("preflight", 5, "checking the prefix");
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")?;
    if let Some(why) = crate::hdpim::stamp_problem(prefix) {
        return Err(Failure::new("runtime-stamp", EX_DEP, format!("{why} Nothing was changed."))
            .with("prefix_changed", false)
            .into());
    }
    if crate::hdpim::runtime_for_prefix(&home, prefix).is_none() {
        return Err(Failure::new(
            "runtime-missing",
            EX_DEP,
            "This prefix's Neutron runtime is not installed. Run `neutron update` first, then update the apps. Nothing was changed.",
        )
        .with("prefix_changed", false)
        .into());
    }

    // An update that was cut off (power loss, a killed process) left its snapshot in
    // "pre-update": put it back before anything else.
    if let Some(l) = lock.as_mut() {
        let cut: Vec<_> = crate::snapshot::list(prefix).into_iter().filter(|(_, m)| m.state == "pre-update").collect();
        if !cut.is_empty() && !l.take_prefix(prefix, "update")? {
            return Err(state::already_running(prefix));
        }
        for (dir, mut meta) in cut {
            em.note(&format!("The last update of {} stopped before it finished; putting back how it was first.", name_of(&meta.app)));
            quiesce(em, prefix)?;
            crate::snapshot::restore(prefix, &dir, &mut meta, crate::snapshot::HIVES)?;
            st.in_progress = None;
            let lu = state::LastUpdate {
                ok: false,
                app: meta.app.clone(),
                state: "rolled_back".into(),
                message: "An update that stopped before it finished was undone.".into(),
                at: crate::clock::now(),
                from: meta.from.clone(),
                to: meta.to.clone(),
                log: None,
            };
            st.history.push(lu.clone());
            st.last_update = Some(lu);
            let _ = state::write_state(prefix, &st);
        }
        l.release_prefix();
    }

    // Plan.
    em.progress("preflight", 15, "reading what is installed");
    let inv = crate::inventory::inventory(&prefix_s)?;
    let ledger = crate::ledger::Ledger::load_quick(em, false)?;
    let vl = crate::verified::load(false);
    let wanted: Vec<&InstalledApp> = inv
        .apps
        .iter()
        .filter(|a| o.all || o.apps.contains(&a.id))
        .filter(|a| !other_year_of_target(&inv.apps, a, o.to.as_deref()))
        .collect();
    for want in &o.apps {
        if !inv.apps.iter().any(|a| &a.id == want) {
            return Err(Failure::new("not-installed", EX_FAIL, format!("{} is not installed in this prefix.", name_of(want))).with("prefix_changed", false).into());
        }
    }
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for a in &wanted {
        em.set_app(Some(&a.id));
        match plan_job(&ledger, &vl.list, a, o.to.as_deref(), o.allow_unverified) {
            Ok(Some(mut j)) => match resolve_job(&ledger, &mut j) {
                Ok(()) => jobs.push(j),
                Err(e) => push(em, &mut outcomes, fail_outcome(&a.id, a.version.clone(), None, &e)),
            },
            Ok(None) => {
                if !o.all || o.to.is_some() {
                    push(em, &mut outcomes, Outcome {
                        app: a.id.clone(),
                        ok: true,
                        from: a.version.clone(),
                        to: a.version.clone(),
                        state: "up_to_date".into(),
                        message: format!("{} is up to date.", a.name),
                        exit: 0,
                        extra: Default::default(),
                    });
                }
            }
            Err(e) => {
                if o.apps.len() == 1 && !o.all {
                    em.set_app(None);
                    return Err(e);
                }
                push(em, &mut outcomes, fail_outcome(&a.id, a.version.clone(), None, &e));
            }
        }
    }
    em.set_app(None);

    if o.dry_run {
        let planned: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({"app": j.app.id, "from": j.from, "to": j.target.product_version, "verified": j.verified,
                       "managed_by": j.app.managed_by, "adopt": j.adopt.as_ref().map(|a| a.product_version.clone()),
                       "download_bytes": j.download_bytes, "extract_bytes": j.extract_bytes,
                       "packages": j.plan.as_ref().map(|p| p.packages.iter().map(|x| format!("{} {}", x.name, x.version)).collect::<Vec<_>>()),
                       "dependencies": j.deps.iter().map(|(d, _)| format!("{} {}", d.sap, d.product_version)).collect::<Vec<_>>(),
                       "modules": j.plan.as_ref().map(|p| p.modules.clone())})
            })
            .collect();
        em.result(&json!({"ok": true, "dry_run": true, "prefix": prefix_s, "plan": planned,
                          "apps": outcomes.iter().map(|x| x.json()).collect::<Vec<_>>()}));
        return Ok(());
    }

    // Disk: the download in the cache's filesystem and the unpacked size in the prefix's (they can
    // differ: checking only the prefix's missed a full home holding ~/.cache), before anything is
    // fetched.
    if !jobs.is_empty() {
        let download: u64 = jobs.iter().map(|j| j.download_bytes).sum();
        let unpacked: u64 = jobs.iter().map(|j| j.extract_bytes).sum();
        crate::download::require_space(prefix, crate::download::cache_root().as_deref(), download, unpacked, "the update")?;
    }

    st.in_progress = Some(state::InProgress {
        pid: std::process::id(),
        command: "update".into(),
        apps: jobs.iter().map(|j| j.app.id.clone()).collect(),
        started_at: crate::clock::now(),
        stage: "download".into(),
        log: state::log_path(prefix).display().to_string(),
        prefix_held: Some(false),
    });
    let _ = state::write_state(prefix, &st);

    // Download everything first: a network failure leaves the prefix untouched.
    let mut ready: Vec<Job> = Vec::new();
    for mut j in jobs {
        em.set_app(Some(&j.app.id));
        em.progress("download", 0, &format!("{} {} ({:.2} GB)", j.app.name, crate::version::label(&j.target.product_version), j.download_bytes as f64 / 1e9));
        match download_job(em, &ledger, &mut j) {
            Ok(()) => ready.push(j),
            Err(e) => push(em, &mut outcomes, fail_outcome(&j.app.id, j.from.clone(), Some(j.target.product_version.clone()), &e)),
        }
    }
    em.set_app(None);

    // Now the prefix: wait for its apps to close, then hold it until the end.
    if !ready.is_empty() {
        let names: Vec<String> = ready.iter().map(|j| format!("{} {}", j.app.name, crate::version::label(&j.target.product_version))).collect();
        let msg = format!("{} {} downloaded.", crate::busy::name_list(&names), if names.len() == 1 { "is" } else { "are" });
        set_progress(prefix, &mut st, "waiting");
        crate::hdpim::wait_for_prefix(em, lock.as_mut().expect("not a dry run"), prefix, "update", &msg)?;
        if let Some(p) = st.in_progress.as_mut() {
            p.prefix_held = Some(true);
        }
    }
    let cfg = if ready.is_empty() {
        None
    } else {
        Some(crate::hdpim::discover(&crate::download::repo_tools_dir()?, crate::download::accc_packages_dir(em)?, Some(prefix))?)
    };
    let mut changed_any = false;
    for j in &ready {
        let cfg = cfg.as_ref().expect("discovered");
        em.set_app(Some(&j.app.id));
        set_progress(prefix, &mut st, "snapshot");
        if let Err(e) = quiesce(em, prefix) {
            push(em, &mut outcomes, fail_outcome(&j.app.id, j.from.clone(), Some(j.target.product_version.clone()), &e));
            continue;
        }
        let rel = j.app.install_dir.strip_prefix(prefix).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let rec = j.app.record(inv.db.as_ref());
        let dep_recs: Vec<&crate::hdpimdb::Product> = j
            .deps
            .iter()
            .filter_map(|(d, _)| inv.db.as_ref().and_then(|db| db.product(&d.sap, &d.base_version)))
            .collect();
        let snap = if o.no_snapshot {
            None
        } else {
            em.progress("snapshot", 10, "keeping a copy of the current version for rollback");
            match crate::snapshot::create(prefix, &j.app.id, &j.app.sap, j.from.as_deref(), Some(&j.target.product_version), crate::snapshot::scope(prefix, &rel, rec, &dep_recs)) {
                Ok(s) => {
                    em.progress("snapshot", 100, &format!("snapshot {} ({})", s.1.id, s.1.mode));
                    Some(s)
                }
                Err(e) => {
                    push(em, &mut outcomes, fail_outcome(&j.app.id, j.from.clone(), Some(j.target.product_version.clone()), &e));
                    continue;
                }
            }
        };
        set_progress(prefix, &mut st, "apply");
        // Test hook for the rollback path: fail right after the snapshot, before Adobe's
        // installer changes anything else, or after it finished.
        let hook = std::env::var("MUDHUT_TEST_FAIL_AT").unwrap_or_default();
        em.set_stage(Some("apply"));
        let applied = if hook == "apply" { Err(anyhow::anyhow!("test hook: failing the update at apply")) } else { apply_job(em, cfg, prefix, j) };
        em.set_stage(None);
        let applied = applied.and_then(|x| if hook == "verify" { Err(anyhow::anyhow!("test hook: failing the update after Adobe's installer finished")) } else { Ok(x) });
        match applied {
            Ok(exe) => {
                changed_any = true;
                let mut extra = serde_json::Map::new();
                extra.insert("verified".into(), json!(j.verified));
                extra.insert("exe".into(), json!(exe.display().to_string()));
                if let Some((d, m)) = &snap {
                    let mut m = m.clone();
                    m.state = "committed".into();
                    let _ = crate::snapshot::write_meta(d, &m);
                    extra.insert("snapshot".into(), json!(m.id));
                    let _ = crate::snapshot::prune(prefix, Some(&j.app.id), 1, Some(&m.id));
                }
                if j.adopt_needed {
                    extra.insert("adopted".into(), json!(j.adopt.as_ref().map(|a| a.product_version.clone()).unwrap_or_else(|| "installed over the copy".into())));
                }
                let oc = Outcome { app: j.app.id.clone(), ok: true, from: j.from.clone(), to: Some(j.target.product_version.clone()), state: "updated".into(), message: String::new(), exit: 0, extra };
                em.event(&oc.json(), &format!("{} updated to {}", j.app.name, crate::version::label(&j.target.product_version)));
                outcomes.push(oc);
            }
            Err(e) => {
                let reason = format!("{e:#}");
                let mut oc = Outcome {
                    app: j.app.id.clone(),
                    ok: false,
                    from: j.from.clone(),
                    to: Some(j.target.product_version.clone()),
                    state: "incomplete".into(),
                    message: String::new(),
                    exit: EX_ROLLBACK_FAILED,
                    extra: Default::default(),
                };
                if let Some(code) = crate::failure::find(&e).map(|f| f.code) {
                    oc.extra.insert("code".into(), json!(code));
                }
                match snap {
                    Some((d, mut m)) => match auto_rollback(em, prefix, &d, &mut m) {
                        Ok(_) => {
                            oc.state = "rolled_back".into();
                            oc.exit = EX_ROLLED_BACK;
                            oc.message = format!("The update didn't finish, so the previous version ({}) was put back. {reason}", j.from.as_deref().map(crate::version::label).unwrap_or_default());
                        }
                        Err(re) => {
                            m.state = "rollback-failed".into();
                            let _ = crate::snapshot::write_meta(&d, &m);
                            oc.message = format!("The update didn't finish, and putting the previous version back failed too ({re:#}). The copy is kept in {}. {reason}", d.display());
                        }
                    },
                    None => {
                        oc.exit = EX_FAIL;
                        oc.message = format!("The update didn't finish, and no rollback copy was kept (turned off). {} may not open until the update is run again. {reason}", j.app.name);
                    }
                }
                oc.extra.insert("log".into(), json!(state::log_path(prefix).display().to_string()));
                em.event(&oc.json(), &oc.message);
                outcomes.push(oc);
            }
        }
        em.set_app(None);
    }

    let mut provisioned = Value::Null;
    if changed_any {
        set_progress(prefix, &mut st, "provision");
        em.progress("provision", 50, "neutron prefix provision");
        provisioned = json!(crate::install::provision_quiet(prefix, cfg.as_ref().map(|c| c.wine.as_path())).is_ok());
    }
    if !o.keep_download {
        for j in &ready {
            for r in j.roots.values().chain(j.adopt_roots.values()) {
                if let Some(c) = r.parent() {
                    let _ = std::fs::remove_dir_all(c);
                }
            }
        }
    }

    // State: nothing in progress; the last outcome; history.
    st.in_progress = None;
    for oc in outcomes.iter().filter(|x| x.state != "up_to_date") {
        let lu = state::LastUpdate {
            ok: oc.ok,
            app: oc.app.clone(),
            state: if oc.ok { "updated".into() } else { oc.state.clone() },
            message: oc.message.clone(),
            at: crate::clock::now(),
            from: oc.from.clone(),
            to: oc.to.clone(),
            log: Some(state::log_path(prefix).display().to_string()),
        };
        st.history.push(lu.clone());
        st.last_update = Some(lu);
    }
    let _ = state::write_state(prefix, &st);

    let ok = outcomes.iter().all(|x| x.ok) && provisioned != json!(false);
    let attempted: Vec<&Outcome> = outcomes.iter().filter(|x| x.state != "up_to_date").collect();
    let exit = if ok {
        0
    } else if attempted.len() == 1 && provisioned != json!(false) {
        attempted[0].exit.max(1)
    } else {
        EX_FAIL
    };
    if !em.is_json() {
        for x in &outcomes {
            outln!(
                "{:<14} {}",
                x.app,
                if x.ok && x.state == "updated" {
                    format!("updated {} -> {}", x.from.clone().unwrap_or_default(), x.to.clone().unwrap_or_default())
                } else {
                    x.message.clone()
                }
            );
        }
    }
    em.result(&json!({
        "ok": ok,
        "prefix": prefix_s,
        "apps": outcomes.iter().map(|x| { let mut v = x.json(); v.as_object_mut().map(|m| m.remove("event")); v }).collect::<Vec<_>>(),
        "provisioned": provisioned,
        "exit": exit,
    }));
    if exit != 0 {
        // The terminal event is the result above; exit with its code without a second event.
        std::process::exit(exit as i32);
    }
    Ok(())
}

/// Record an app's outcome and announce it at once as its `app_result` event.
fn push(em: &Emitter, outcomes: &mut Vec<Outcome>, oc: Outcome) {
    em.event(&oc.json(), &oc.message);
    outcomes.push(oc);
}

fn fail_outcome(app: &str, from: Option<String>, to: Option<String>, e: &anyhow::Error) -> Outcome {
    let f = crate::failure::find(e);
    let mut extra = serde_json::Map::new();
    if let Some(f) = f {
        extra.insert("code".into(), json!(f.code));
        for (k, v) in &f.fields {
            if k != "prefix_changed" {
                extra.insert(k.clone(), v.clone());
            }
        }
    }
    Outcome {
        app: app.into(),
        ok: false,
        from,
        to,
        state: "unchanged".into(),
        message: f.map(|f| f.message.clone()).unwrap_or_else(|| format!("{e:#}")),
        exit: f.map(|f| f.exit).unwrap_or(EX_FAIL),
        extra,
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(cmd)).find(|p| p.is_file())
}

// ------------------------------------------------------------------- rollback / snapshots

pub fn cmd_rollback(em: &Emitter, prefix_s: &str, app: Option<&str>, snapshot: Option<&str>) -> Result<()> {
    let prefix = Path::new(prefix_s);
    if !prefix.join("drive_c").is_dir() {
        return Err(Failure::new("bad-prefix", crate::failure::EX_BADPREFIX, format!("{} is not a prefix (it has no drive_c folder).", prefix.display())).with("prefix_changed", false).into());
    }
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    let _lock = state::lock(prefix, "rollback")?;
    let busy = crate::busy::scan(prefix);
    if busy.blocking() {
        return Err(busy_failure(&busy, "Rolling back"));
    }
    let all = crate::snapshot::list(prefix);
    let pick = all
        .iter()
        .rev()
        .find(|(_, m)| {
            (m.state == "committed" || m.state == "pre-update")
                && snapshot.map(|s| s == m.id).unwrap_or(true)
                && app.map(|a| a == m.app).unwrap_or(true)
        })
        .cloned();
    let Some((dir, mut meta)) = pick else {
        return Err(Failure::new(
            "no-snapshot",
            EX_FAIL,
            match app {
                Some(a) => format!("There is no earlier version of {} to go back to in this prefix.", name_of(a)),
                None => "There is no update to roll back in this prefix.".into(),
            },
        )
        .with("prefix_changed", false)
        .into());
    };
    crate::snapshot::check_order(prefix, &meta)?;
    let log = state::log_path(prefix);
    let _ = em.attach_log(&log);
    em.set_app(Some(&meta.app));
    let mut st = state::read_state(prefix);
    st.in_progress = Some(state::InProgress {
        pid: std::process::id(),
        command: "rollback".into(),
        apps: vec![meta.app.clone()],
        started_at: crate::clock::now(),
        stage: "rollback".into(),
        log: log.display().to_string(),
        prefix_held: Some(true),
    });
    let _ = state::write_state(prefix, &st);
    em.progress("rollback", 5, &format!("putting back {} {}", name_of(&meta.app), meta.from.clone().unwrap_or_default()));
    quiesce(em, prefix)?;
    // A manual rollback keeps user.reg: the user's own settings since the update stay.
    let hives: Vec<&str> = if meta.state == "pre-update" { crate::snapshot::HIVES.to_vec() } else { vec!["system.reg", "userdef.reg"] };
    let r = crate::snapshot::restore(prefix, &dir, &mut meta, &hives);
    let notes = match r {
        Ok(n) => n,
        Err(e) => {
            meta.state = "rollback-failed".into();
            let _ = crate::snapshot::write_meta(&dir, &meta);
            st.in_progress = None;
            let _ = state::write_state(prefix, &st);
            return Err(Failure::new("rollback-failed", EX_ROLLBACK_FAILED, format!("Putting the previous version back failed ({e:#}). The copy is kept in {}.", dir.display())).into());
        }
    };
    em.progress("provision", 60, "neutron prefix provision");
    let provisioned = crate::install::provision_quiet(prefix, stamped_wine(prefix).as_deref()).is_ok();
    // Check the program file is the old one again.
    let mut ok = true;
    let mut msg = String::new();
    if let (Some(cat), Some(from)) = (crate::catalog::find(&meta.app), meta.from.as_deref()) {
        // Yearly versions sit side by side; an update never changes the major, so `from`'s major
        // names the install this snapshot belongs to.
        let year_of = |a: &InstalledApp| a.version.as_deref().map(crate::version::major) == Some(crate::version::major(from));
        let exe = crate::inventory::inventory(prefix_s).ok().and_then(|i| {
            let mut same: Vec<InstalledApp> = i.apps.into_iter().filter(|a| a.id == meta.app).collect();
            let at = same.iter().position(year_of).unwrap_or(0);
            (!same.is_empty()).then(|| same.swap_remove(at)).and_then(|a| a.exe)
        });
        if let Some(v) = exe.as_deref().and_then(crate::pe::file_version_string) {
            if crate::version::key(&v).get(..2) != crate::version::key(from).get(..2) {
                ok = false;
                msg = format!("The rollback finished, but {} reports {v} instead of {from}.", cat.name);
            }
        }
    }
    st.in_progress = None;
    let lu = state::LastUpdate {
        ok,
        app: meta.app.clone(),
        state: "rolled_back".into(),
        message: if ok { format!("{} was put back to {}.", name_of(&meta.app), meta.from.clone().unwrap_or_default()) } else { msg.clone() },
        at: crate::clock::now(),
        from: meta.to.clone(),
        to: meta.from.clone(),
        log: Some(log.display().to_string()),
    };
    st.history.push(lu.clone());
    st.last_update = Some(lu);
    let _ = state::write_state(prefix, &st);
    em.set_app(None);
    if !em.is_json() {
        outln!("{} put back to {}", meta.app, meta.from.clone().unwrap_or_default());
    }
    em.result(&json!({
        "ok": ok, "prefix": prefix_s, "app": meta.app, "snapshot": meta.id,
        "from": meta.to, "to": meta.from, "provisioned": provisioned, "notes": notes, "message": msg,
    }));
    if !ok {
        std::process::exit(EX_FAIL as i32);
    }
    Ok(())
}

pub fn cmd_snapshots(em: &Emitter, prefix_s: &str, prune: bool, keep: usize) -> Result<()> {
    let prefix = Path::new(prefix_s);
    if !prefix.join("drive_c").is_dir() {
        return Err(Failure::new("bad-prefix", crate::failure::EX_BADPREFIX, format!("{} is not a prefix (it has no drive_c folder).", prefix.display())).into());
    }
    let mut removed = Vec::new();
    if prune {
        let _lock = state::lock(prefix, "snapshots")?;
        removed = crate::snapshot::prune(prefix, None, keep, None);
    }
    let rows: Vec<Value> = crate::snapshot::list(prefix)
        .into_iter()
        .map(|(d, m)| {
            json!({"id": m.id, "app": m.app, "from": m.from, "to": m.to, "created": m.created, "state": m.state,
                   "mode": m.mode, "files": m.entries.iter().map(|e| e.files).sum::<u64>(),
                   "bytes": m.entries.iter().map(|e| e.bytes).sum::<u64>(), "path": d.display().to_string()})
        })
        .collect();
    if !em.is_json() {
        for r in &rows {
            outln!("{}  {} {} -> {}  {}  {:.1} GB ({})", r["id"].as_str().unwrap_or(""), r["app"].as_str().unwrap_or(""),
                r["from"].as_str().unwrap_or("?"), r["to"].as_str().unwrap_or("?"), r["state"].as_str().unwrap_or(""),
                r["bytes"].as_u64().unwrap_or(0) as f64 / 1e9, r["mode"].as_str().unwrap_or(""));
        }
    }
    em.result(&json!({"ok": true, "prefix": prefix_s, "snapshots": rows, "removed": removed}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, year: u16, version: &str) -> InstalledApp {
        InstalledApp {
            id: id.into(), name: id.into(), sap: "PPRO".into(), dir: format!("Adobe {id} {year}"),
            install_dir: "/x".into(), exe: None, year: Some(year), version: Some(version.into()), exact: true,
            codex: None, base: None, build_guid: None, managed_by: Managed::Hdpim, consistent: true,
            sources: Default::default(), packages: Default::default(), modules: vec![], notes: vec![],
        }
    }

    #[test]
    fn a_pinned_build_updates_only_the_install_of_its_year() {
        let (y25, y26) = (installed("premiere", 2025, "25.6.4.1"), installed("premiere", 2026, "26.3.0.12"));
        let apps = vec![y25.clone(), y26.clone(), installed("aftereffects", 2025, "25.6.0.1")];
        assert!(other_year_of_target(&apps, &y25, Some("26.5.2.3")), "the 2025 install is left alone");
        assert!(!other_year_of_target(&apps, &y26, Some("26.5.2.3")));
        assert!(!other_year_of_target(&apps, &y25, None), "without --to every install updates in its own year");
        // No install of the build's year: nothing is skipped, planning explains the build.
        assert!(!other_year_of_target(&apps, &y25, Some("27.0.0.40")));
        assert!(!other_year_of_target(&apps, &apps[2], Some("26.5.2.3")), "another app's year does not count");
    }
}
