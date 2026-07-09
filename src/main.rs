// SPDX-License-Identifier: Apache-2.0
//! Mud Hut — the easy way to install Adobe apps into a Neutron prefix.
//!
//! A non-interactive, idempotent, transactional CLI that sets up the sandbox,
//! ingests the vendor binaries, provisions the prefix (via `neutron prefix
//! provision`), and installs launchers. It never translates (that's Neutron's
//! job) and never ships or patches Adobe binaries — orchestration only.
//!
//! Layering mirrors the ecosystem: Mud Hut (install+provision) -> Neutron
//! (runtime) -> Collider (GUI). Collider drives this CLI exactly like it drives
//! `neutron`: `mudhut --json <cmd>` streams newline-delimited JSON events, the
//! last of which is the terminal `result`/`error`.

mod auth;
mod catalog;
mod doctor;
mod download;
mod driver;
mod feed;
mod hdpim;
mod install;
mod ledger;
mod offline;
mod output;
mod source;
mod windows;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use output::Emitter;

#[derive(Parser)]
#[command(name = "mudhut", version, about = "Install Adobe apps into a Neutron prefix")]
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
        /// A Windows install root (drive_c, a mounted C:, or a copied tree).
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Install one app (or the whole suite) into a Neutron prefix.
    Install(InstallArgs),
    /// Show the resolved download ledger (endpoints, SAP codes, versions).
    Ledger,
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
    /// Adobe sign-in (device/QR flow). Collider drives these: `begin` once, then
    /// `poll` every few seconds until the user has signed in.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Mint the login link + QR. Returns { url, qr, request_id, device_id }.
    Begin,
    /// Poll once; runs the token exchange when sign-in completes.
    Poll {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        device_id: String,
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
    /// copied tree). For `offline`: the Adobe offline package / ISO.
    #[arg(long)]
    source: Option<PathBuf>,

    /// Target Neutron prefix (created if absent).
    #[arg(long)]
    prefix: PathBuf,

    /// Plan and report without copying or mutating the prefix.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, ValueEnum)]
enum Method {
    /// Copy from an existing Windows install (implemented).
    Windows,
    /// Install genuine from Adobe via the HDPIM offline engine (decrypt; no
    /// Set-up.exe/WAM/CC-desktop). Needs `--source <staged products dir>` for now.
    Download,
    /// Extract from an Adobe offline package / ISO (roadmap 1.4, not yet implemented).
    Offline,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let em = Emitter::new(cli.json);

    let result = match cli.cmd {
        Cmd::Doctor { prefix } => doctor::run(&em, prefix.as_deref()),
        Cmd::Apps { source } => catalog::cmd_apps(&em, source.as_deref()),
        Cmd::Install(a) => match a.method {
            // download has its own decrypt-install engine (HDPIM), not copy-staging.
            Method::Download => {
                download::install(&em, a.app.as_deref(), a.source.as_deref(), &a.prefix, a.dry_run)
            }
            // windows/offline: acquire (method-specific) -> stage + provision (shared).
            Method::Windows => windows::acquire(a.source.as_deref(), a.app.as_deref(), a.suite)
                .and_then(|acq| install::run(&em, &a.prefix, acq, a.dry_run)),
            Method::Offline => offline::acquire(&em, a.source.as_deref(), a.app.as_deref(), a.suite)
                .and_then(|acq| install::run(&em, &a.prefix, acq, a.dry_run)),
        },
        Cmd::Ledger => ledger::cmd_ledger(&em),
        Cmd::Download { app, lang, dest, core_only, only } => {
            feed::cmd_download(&em, &app, &lang, dest.as_deref(), core_only, only.as_deref())
        }
        Cmd::Auth { action } => match action {
            AuthAction::Begin => auth::cmd_begin(&em),
            AuthAction::Poll { request_id, device_id } => auth::cmd_poll(&em, &request_id, &device_id),
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            em.error(&e.to_string());
            ExitCode::FAILURE
        }
    }
}
