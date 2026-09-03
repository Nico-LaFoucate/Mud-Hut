// Stamp the binary with the commit it was built from.
//
// Every build reported version "0.2.0", so there was no way — for a user or for
// me — to tell a freshly installed binary from one built an hour earlier. That
// turned "did the fix land?" into guesswork across several debugging rounds.
use std::process::Command;

fn main() {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    // A dirty tree matters most: it means the binary does not match any commit.
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if !s.is_empty() => "-dirty",
        _ => "",
    };
    let when = git(&["log", "-1", "--format=%cd", "--date=format:%Y-%m-%d %H:%M"])
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MUDHUT_BUILD={hash}{dirty} ({when})");
    // Re-run when HEAD moves so the stamp cannot go stale.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    // Also re-stamp when SOURCES change, or an edited-but-uncommitted tree keeps
    // the previous build's stamp and the "-dirty" marker silently goes stale --
    // which defeats the whole point of being able to tell builds apart.
    println!("cargo:rerun-if-changed=src");
}
