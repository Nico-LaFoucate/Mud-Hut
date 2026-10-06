// SPDX-License-Identifier: Apache-2.0
//! Mud Hut — the easy way to install Adobe apps into a Neutron prefix.
//!
//! A non-interactive, idempotent, transactional CLI that sets up the sandbox,
//! ingests the vendor binaries, and provisions the prefix (via `neutron prefix
//! provision`, which also writes the menu launchers). It never translates (that's
//! Neutron's job) and never ships or patches Adobe binaries — orchestration only.
//!
//! Layering mirrors the ecosystem: Mud Hut (install+provision) -> Neutron
//! (runtime) -> Collider (GUI). Collider drives this CLI exactly like it drives
//! `neutron`: `mudhut --json <cmd>` streams newline-delimited JSON events, the
//! last of which is the terminal `result`/`error`.

mod catalog;
mod doctor;
mod download;
mod driver;
mod feed;
mod hdpim;
mod install;
mod ledger;
mod iso;
mod offline;
mod output;
mod source;
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
        /// A Windows install root (drive_c, a mounted C:, or a copied tree), or
        /// an offline package dir (`<SAP>/Application.json` payload layout).
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Install one app (or the whole suite) into a Neutron prefix.
    Install(InstallArgs),
    /// Show the resolved download ledger (endpoints, SAP codes, versions).
    Ledger,
    /// Download and verify Adobe's Creative Cloud package (ACCCx) now, instead of on the first
    /// install. `neutron setup` runs this.
    Accc,
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

    /// Ingestion method: how to acquire the app bits. All methods feed the same
    /// stage->provision pipeline; they differ only in acquisition.
    #[arg(long, value_enum, default_value_t = Method::Windows)]
    method: Method,

    /// Source path. For `windows`: a Windows install root (drive_c / mounted C: /
    /// copied tree). For `offline`: the package dir (`<SAP>/` payload layout, as
    /// staged by `mudhut download --dest`). May be read-only — a mounted ISO
    /// works; the driver XML then goes to a scratch dir with absolute paths.
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
}

#[derive(Clone, ValueEnum)]
enum Method {
    /// Copy from an existing Windows install (implemented).
    Windows,
    /// Install genuine from Adobe via the HDPIM offline engine (decrypt; no
    /// Set-up.exe/WAM/CC-desktop). Needs `--source <staged products dir>` for now.
    Download,
    /// Install from a pre-downloaded Adobe offline package — the ESD products
    /// layout `mudhut download --dest` stages (`<SAP>/Application.json` + payload
    /// zips). Fully local. Needs `--source <dir>`; a mounted ISO is fine.
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
                download::install(&em, a.app.as_deref(), a.source.as_deref(), &a.prefix, a.dry_run,
                                  a.minimal, a.keep_download)
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
            em.error(&format!("{e:#}"));
            ExitCode::FAILURE
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
