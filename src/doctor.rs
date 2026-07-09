// SPDX-License-Identifier: Apache-2.0
//! `mudhut doctor` — pre-flight validation of the host.
//!
//! Confirms the pieces an Adobe-on-Neutron install needs: the Neutron runtime,
//! a Vulkan loader, 32-bit graphics libraries (Wine runs Adobe's 32-bit helpers),
//! and enough free disk. Required checks failing => `ready: false`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::output::Emitter;

const SUITE_GIB_HINT: u64 = 25; // a full suite is large; warn under this

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    required: bool,
    detail: String,
}

#[derive(Serialize)]
struct Report {
    ready: bool,
    checks: Vec<Check>,
}

pub fn run(em: &Emitter, prefix: Option<&Path>) -> anyhow::Result<()> {
    let mut checks = Vec::new();

    // Neutron runtime CLI on PATH — the whole stack hangs off it.
    match which("neutron") {
        Some(p) => checks.push(Check { name: "neutron", ok: true, required: true,
            detail: format!("found at {}", p.display()) }),
        None => checks.push(Check { name: "neutron", ok: false, required: true,
            detail: "`neutron` not on PATH — install the Neutron runtime first".into() }),
    }

    // Wine for the HDPIM install engine ($MUDHUT_WINE / $NEUTRON_WINE / installed
    // Neutron runtime / dev tree). `--method download` cannot run without it.
    match crate::hdpim::resolve_wine() {
        Some(w) => checks.push(Check { name: "wine", ok: true, required: true,
            detail: format!("install-engine wine: {}", w.display()) }),
        None => checks.push(Check { name: "wine", ok: false, required: true,
            detail: "no wine — run `neutron runtime install` or set $MUDHUT_WINE".into() }),
    }

    // The shipped tools the download engine drives. Verifies the installed layout
    // (tools/ next to the binary) is intact, not just that some dir resolved.
    {
        let tools = crate::download::repo_tools_dir()
            .unwrap_or_else(|_| PathBuf::from("tools"));
        let missing: Vec<&str> = ["hdpim_host.exe", "extract_accc_runtime.py"]
            .into_iter()
            .filter(|f| !tools.join(f).is_file())
            .collect();
        let ok = missing.is_empty();
        checks.push(Check { name: "tools", ok, required: true,
            detail: if ok { format!("install tools at {}", tools.display()) }
                    else { format!("missing {} under {} — re-run install.sh or set $MUDHUT_TOOLS",
                                   missing.join(", "), tools.display()) } });
    }

    // Vulkan loader (DXVK / vkd3d-proton present via it).
    let vk = Path::new("/usr/lib/libvulkan.so.1").exists()
        || Path::new("/usr/lib64/libvulkan.so.1").exists()
        || which("vulkaninfo").is_some();
    checks.push(Check { name: "vulkan", ok: vk, required: true,
        detail: if vk { "Vulkan loader present".into() }
                else { "no Vulkan loader (libvulkan.so.1 / vulkaninfo)".into() } });

    // 32-bit graphics libs — Adobe ships 32-bit helper processes.
    let lib32 = Path::new("/usr/lib32").is_dir()
        || Path::new("/usr/lib/i386-linux-gnu").is_dir()
        || Path::new("/usr/lib32/libvulkan.so.1").exists();
    checks.push(Check { name: "lib32", ok: lib32, required: true,
        detail: if lib32 { "32-bit library path present".into() }
                else { "no 32-bit libs (install lib32-* / multilib graphics drivers)".into() } });

    // Free disk on the install target (fall back to $HOME).
    let target = prefix
        .map(existing_ancestor)
        .unwrap_or_else(|| home());
    match free_bytes(&target) {
        Some(b) => {
            let gib = b / (1 << 30);
            checks.push(Check { name: "disk", ok: gib >= 5, required: true,
                detail: format!("{gib} GiB free at {} ({})", target.display(),
                    if gib >= SUITE_GIB_HINT { "ample" }
                    else if gib >= 5 { "tight for a full suite" }
                    else { "too low" }) });
        }
        None => checks.push(Check { name: "disk", ok: true, required: false,
            detail: format!("could not measure free space at {}", target.display()) }),
    }

    // icoutils — optional; lets Collider show the real extracted app logos.
    let ico = which("wrestool").is_some() && which("icotool").is_some();
    checks.push(Check { name: "icoutils", ok: ico, required: false,
        detail: if ico { "present (Collider shows real app logos)".into() }
                else { "absent (Collider falls back to monogram badges)".into() } });

    let ready = checks.iter().all(|c| !c.required || c.ok);
    let report = Report { ready, checks };

    if em.is_json() {
        em.result(&report);
    } else {
        println!("Host readiness: {}", if ready { "READY" } else { "NOT READY" });
        for c in &report.checks {
            let mark = if c.ok { "✓" } else if c.required { "✗" } else { "!" };
            println!("  {mark} {:<9} {}", c.name, c.detail);
        }
    }
    Ok(())
}

/// Find `cmd` on PATH.
fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Nearest existing ancestor of `p` (a target prefix may not exist yet).
fn existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p;
    loop {
        if cur.exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return home(),
        }
    }
}

/// Free bytes on the filesystem holding `path`, via `df` (avoids an extra crate).
fn free_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("df")
        .arg("-B1")
        .arg("--output=avail")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .and_then(|l| l.trim().parse::<u64>().ok())
}
