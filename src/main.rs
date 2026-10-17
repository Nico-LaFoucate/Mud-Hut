// SPDX-License-Identifier: Apache-2.0
//! Mud Hut — the easy way to install Adobe apps into a Neutron prefix.
//!
//! A non-interactive, idempotent, transactional CLI that sets up the sandbox,
//! ingests the vendor binaries, and provisions the prefix (via `neutron prefix
//! provision`, which also writes the menu launchers). It never translates (that's
//! Neutron's job) and never ships or patches Adobe binaries — orchestration only.
//!
//! Layering: Collider (the GUI), Mud Hut and a terminal all sit above the `neutron`
//! CLI, which provisions and launches prefixes on the neutron-wine runtime. Collider
//! drives this CLI exactly like it drives `neutron`: `mudhut --json <cmd>` streams newline-delimited JSON events, the
//! last of which is the terminal `result`/`error`.

#[macro_use]
mod output;
mod busy;
mod catalog;
mod clock;
mod doctor;
mod download;
mod driver;
mod failure;
mod feed;
mod feedcache;
mod hdpim;
mod hdpimdb;
mod install;
mod inventory;
mod ledger;
mod iso;
mod net;
mod offline;
mod pe;
mod snapshot;
mod source;
mod state;
mod update;
mod updates;
mod verified;
mod version;
mod windows;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use output::Emitter;

#[derive(Parser)]
#[command(
    name = "mudhut",
    // Includes the commit, so "is the fix in this binary?" is checkable
    // rather than assumed. A -dirty suffix means it matches no commit.
    version = concat!(env!("CARGO_PKG_VERSION"), " build ", env!("MUDHUT_BUILD")),
    about = "Install Adobe apps into a Neutron prefix"
)]
struct Cli {
    /// Emit newline-delimited JSON events on stdout (for Collider / scripting).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check host readiness (drivers, 32-bit libs, Neutron runtime, disk).
    Doctor {
        /// Where the install will land, for the free-space check (defaults to $HOME).
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
    /// List installable Adobe apps; with --source, report what's present there.
    Apps {
        /// A Windows install root (drive_c, a mounted C:, or a copied tree), or an
        /// offline package: a dir (`<SAP>/Application.json` payload layout) or a disc
        /// image (`.iso`/`.img`).
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Install one app (or the whole suite) into a Neutron prefix.
    Install(InstallArgs),
    /// Show the resolved download ledger (endpoints, apps and their SAP codes).
    Ledger,
    /// Download and verify Adobe's Creative Cloud package (ACCCx) now, instead of on the first
    /// install. `neutron setup` runs this.
    Accc,
    /// Which Adobe apps a prefix holds, at which build, and how they got there (read-only, no
    /// wine; safe while apps run).
    Inventory {
        /// A prefix to read (repeatable). Echoed exactly as given.
        #[arg(long, required = true)]
        prefix: Vec<String>,
    },
    /// Which Adobe updates exist for the apps in one or more prefixes. Read-only and wine-free
    /// (safe on a timer and beside open apps); offline answers from the cache with exit 0.
    #[command(name = "check-updates")]
    CheckUpdates {
        /// A prefix to check (repeatable). Echoed exactly as given.
        #[arg(long)]
        prefix: Vec<String>,
        /// Every prefix neutron knows (`neutron --json prefix list`).
        #[arg(long)]
        all_prefixes: bool,
        /// Only these apps (repeatable, e.g. --app photoshop).
        #[arg(long)]
        app: Vec<String>,
        /// Reuse Adobe's product list fetched within this long (e.g. 6h, 30m, 1d; 0 = fetch now).
        #[arg(long, default_value = "6h", value_parser = clock::parse_duration)]
        max_age: std::time::Duration,
        /// Fetch Adobe's product list now, whatever its age.
        #[arg(long)]
        refresh: bool,
        /// Never touch the network: answer from what is cached.
        #[arg(long)]
        offline: bool,
        /// Answer at once from what is cached (no network), for a first paint.
        #[arg(long)]
        cached: bool,
    },
    /// Update Adobe apps in a prefix in place (same yearly version), with a rollback copy kept
    /// first. Refuses (exit 3) while any Adobe app runs in the prefix.
    Update {
        /// Apps to update (e.g. photoshop premiere). Or --all.
        apps: Vec<String>,
        #[arg(long)]
        prefix: String,
        /// Every app in the prefix that has an update.
        #[arg(long)]
        all: bool,
        /// The exact build to move to (what check-updates showed), e.g. 26.5.2.5.
        #[arg(long)]
        to: Option<String>,
        /// Allow a build Neutron has not verified yet (after the user confirmed it).
        #[arg(long)]
        allow_unverified: bool,
        /// Do not keep a rollback copy (saves disk space; a failed update then cannot be undone).
        #[arg(long)]
        no_snapshot: bool,
        /// Plan and report what would be downloaded and done, changing nothing.
        #[arg(long)]
        dry_run: bool,
        /// Keep the downloaded packages afterwards.
        #[arg(long)]
        keep_download: bool,
    },
    /// Put back the version an update replaced (newest update first).
    Rollback {
        #[arg(long)]
        prefix: String,
        /// Only this app's latest update.
        #[arg(long)]
        app: Option<String>,
        /// A specific snapshot (see `mudhut snapshots`).
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// List the rollback copies kept in a prefix; --prune removes them.
    Snapshots {
        #[arg(long)]
        prefix: String,
        /// Remove kept copies (an update cut off half-way keeps its copy regardless).
        #[arg(long)]
        prune: bool,
        /// With --prune: keep this many newest copies per app.
        #[arg(long, default_value_t = 0)]
        keep: usize,
    },
    /// Resolve + download an app from Adobe (feed -> buildGuid -> manifest -> plan
    /// -> fetch+verify). Without --dest, reports the plan and downloads nothing.
    Download {
        /// App id (e.g. photoshop). See `mudhut ledger` for the list.
        app: String,
        /// Install language to plan for.
        #[arg(long, default_value = "en_US")]
        lang: String,
        /// Download+verify into this dir (preserving Adobe's layout). Omit = plan only.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Skip non-core packages (e.g. the large AI / Neural-Filter models).
        #[arg(long)]
        core_only: bool,
        /// Only packages whose name contains this substring (selective / testing).
        #[arg(long)]
        only: Option<String>,
    },
}

#[derive(Args)]
struct InstallArgs {
    /// App id (e.g. photoshop). Omit and pass --suite to install everything found.
    app: Option<String>,

    /// Install every app discovered in the source.
    #[arg(long)]
    suite: bool,

    /// Ingestion method: `windows` copies an existing install; `download` and
    /// `offline` run Adobe's installer engine. Every method ends with
    /// `neutron prefix provision`.
    #[arg(long, value_enum, default_value_t = Method::Windows)]
    method: Method,

    /// Source path. For `windows`: a Windows install root (drive_c / mounted C: /
    /// copied tree). For `offline`: a package dir (`<SAP>/Application.json` +
    /// payload layout) or a disc image (`.iso`/`.img`), which Mud Hut mounts
    /// read-only; a read-only source works, the driver XML then goes to a scratch
    /// dir with absolute paths. For `download`: optional, packages you already
    /// staged (nothing is downloaded).
    #[arg(long)]
    source: Option<PathBuf>,

    /// Target Neutron prefix (created if absent).
    #[arg(long)]
    prefix: PathBuf,

    /// Plan and report without copying or mutating the prefix.
    #[arg(long)]
    dry_run: bool,

    /// `--method download`: skip the add-on components (Camera Raw, Libraries, CoreSync).
    /// A smaller download for testing; the default installs everything the app lists.
    #[arg(long)]
    minimal: bool,

    /// `--method download`: keep the downloaded packages after a successful install
    /// (default: delete them; they can be tens of GB).
    #[arg(long)]
    keep_download: bool,

    /// `--method download`: install the newest build of this major version (e.g. `28` for
    /// Photoshop 2027) instead of the newest build Adobe offers. A new yearly version installs
    /// beside the old one.
    #[arg(long, value_name = "MAJOR")]
    version: Option<u64>,

    /// Also install the speech-to-text language packs (Premiere Pro and Media Encoder; about
    /// 13.8 GB). Off by default, as in Creative Cloud.
    #[arg(long)]
    speech_packs: bool,
}

#[derive(Clone, ValueEnum)]
enum Method {
    /// Copy from an existing Windows install (implemented).
    Windows,
    /// Install genuine from Adobe via HDPIM (decrypt; no Set-up.exe/WAM/CC-desktop).
    /// Downloads the app and its components; `--source` reuses packages you
    /// already staged.
    Download,
    /// Install from a pre-downloaded Adobe offline package — the ESD products
    /// layout (`<SAP>/Application.json` + payload zips). Fully local. Needs
    /// `--source`: the package dir, or a disc image (`.iso`/`.img`) that Mud Hut
    /// mounts read-only.
    Offline,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let em = Emitter::new(cli.json);

    // Reclaim any ISO left mounted by a previous run that was killed before its
    // teardown could run (see iso::sweep_stale — nothing runs on SIGKILL).
    iso::sweep_stale();

    let result = match cli.cmd {
        Cmd::Doctor { prefix } => doctor::run(&em, prefix.as_deref()),
        Cmd::Apps { source } => catalog::cmd_apps(&em, source.as_deref()),
        Cmd::Install(a) => ensure_prefix_dir(&a.prefix, a.dry_run).and_then(|()| match a.method {
            // download has its own decrypt-install engine (HDPIM), not copy-staging.
            Method::Download => {
                let opts = feed::PlanOpts { major: a.version, speech_packs: a.speech_packs, ..Default::default() };
                download::install(&em, a.app.as_deref(), a.source.as_deref(), &a.prefix, a.dry_run,
                                  a.minimal, a.keep_download, &opts)
            }
            // windows: acquire (copy discovery) -> stage + provision (shared).
            Method::Windows => windows::acquire(a.source.as_deref(), a.app.as_deref(), a.suite)
                .and_then(|acq| install::run(&em, &a.prefix, acq, a.dry_run)),
            // offline: packages are encrypted -> the same HDPIM engine as download,
            // but resolved fully locally from the staged package (no network).
            Method::Offline => {
                offline::install(&em, a.app.as_deref(), a.source.as_deref(), &a.prefix, a.dry_run)
            }
        }),
        Cmd::Ledger => ledger::cmd_ledger(&em),
        Cmd::Inventory { prefix } => inventory::cmd_inventory(&em, &prefix),
        Cmd::Update { apps, prefix, all, to, allow_unverified, no_snapshot, dry_run, keep_download } => update::cmd_update(
            &em,
            update::UpdateOpts { prefix, apps, all, to, allow_unverified, no_snapshot, dry_run, keep_download },
        ),
        Cmd::Rollback { prefix, app, snapshot } => update::cmd_rollback(&em, &prefix, app.as_deref(), snapshot.as_deref()),
        Cmd::Snapshots { prefix, prune, keep } => update::cmd_snapshots(&em, &prefix, prune, keep),
        Cmd::CheckUpdates { prefix, all_prefixes, app, max_age, refresh, offline, cached } => {
            updates::cmd_check_updates(&em, updates::CheckOpts { prefixes: prefix, all_prefixes, apps: app, max_age, refresh, offline, cached })
        }
        Cmd::Accc => download::prefetch_accc(&em),
        Cmd::Download { app, lang, dest, core_only, only } => {
            feed::cmd_download(&em, &app, &lang, dest.as_deref(), core_only, only.as_deref())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` renders the whole anyhow chain ("context: cause: cause"),
            // not just the outermost context. Without it the actual reason — the
            // OS error, wine's own complaint — is dropped, and Collider shows the
            // user a message with nothing actionable in it.
            match failure::find(&e) {
                Some(f) => {
                    em.error_with(&format!("{e:#}"), &f.event_fields());
                    ExitCode::from(f.exit)
                }
                None => {
                    em.error_with(&format!("{e:#}"), &serde_json::Map::new());
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Make sure the install target exists before ANY method runs.
///
/// Mud Hut's whole job is "produce a working prefix at this path", so the path
/// not existing yet is the NORMAL case, not an error. Wine will create a single
/// missing directory but not its parents, so a nested target (Collider's default
/// is `~/Neutron/Adobe`, and `~/Neutron` does not exist on a clean machine) made
/// `wineboot --init` die with a bare exit 1.
///
/// This lives here, at the dispatch, rather than inside a method: `--method
/// windows` created the prefix on its way to staging while the HDPIM path
/// (download AND offline) never did, and that asymmetry is exactly why the gap
/// survived an end-to-end verification — the runs that exercised it happened to
/// use paths that already existed.
fn ensure_prefix_dir(prefix: &std::path::Path, dry_run: bool) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    if prefix.exists() {
        if !prefix.is_dir() {
            bail!("--prefix is not a directory: {}", prefix.display());
        }
        return Ok(());
    }
    if dry_run {
        return Ok(()); // a dry run must not touch the filesystem
    }
    std::fs::create_dir_all(prefix)
        .with_context(|| format!("creating the prefix directory {}", prefix.display()))
}
