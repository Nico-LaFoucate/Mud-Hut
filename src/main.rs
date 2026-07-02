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

mod catalog;
mod doctor;
mod install;
mod output;
mod source;

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
}

#[derive(Args)]
struct InstallArgs {
    /// App id (e.g. photoshop). Omit and pass --suite to install everything found.
    app: Option<String>,

    /// Install every app discovered in the source.
    #[arg(long)]
    suite: bool,

    /// Ingestion method. Only `windows` (copy from a Windows install) is implemented in P1.
    #[arg(long, value_enum, default_value_t = Method::Windows)]
    method: Method,

    /// Source root for the `windows` method (drive_c / mounted C: / copied tree).
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
    /// Extract from an Adobe offline package / ISO (P2, not yet implemented).
    Iso,
    /// Download from Adobe (P3, not yet implemented).
    Download,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let em = Emitter::new(cli.json);

    let result = match cli.cmd {
        Cmd::Doctor { prefix } => doctor::run(&em, prefix.as_deref()),
        Cmd::Apps { source } => catalog::cmd_apps(&em, source.as_deref()),
        Cmd::Install(a) => match a.method {
            Method::Windows => install::from_windows(&em, &a.prefix, a.source.as_deref(),
                                                     a.app.as_deref(), a.suite, a.dry_run),
            Method::Iso => Err(anyhow::anyhow!("--method iso is not implemented yet (P2)")),
            Method::Download => Err(anyhow::anyhow!("--method download is not implemented yet (P3)")),
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
