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

    // Wine for the HDPIM install engine ($MUDHUT_WINE / $NEUTRON_WINE / `neutron runtime
    // which` / newest installed Neutron runtime). `--method download` cannot run without it.
    match crate::hdpim::resolve_wine(None) {
        Some(w) => checks.push(Check { name: "wine", ok: true, required: true,
            detail: format!("install-engine wine: {}", w.display()) }),
        None => checks.push(Check { name: "wine", ok: false, required: true,
            detail: "neutron-wine isn't installed — run `neutron setup` first".into() }),
    }

    // The shipped tools the download engine drives. Verifies the installed layout
    // (tools/ next to the binary) is intact, not just that some dir resolved.
    {
        let tools = crate::download::repo_tools_dir()
            .unwrap_or_else(|_| PathBuf::from("tools"));
        let missing: Vec<&str> = ["hdpim_host.exe", "extract_accc_runtime.py", "register_accc_pim.py"]
            .into_iter()
            .filter(|f| !tools.join(f).is_file())
            .collect();
        let ok = missing.is_empty();
        checks.push(Check { name: "tools", ok, required: true,
            detail: if ok { format!("install tools at {}", tools.display()) }
                    else { format!("missing {} under {} — set $MUDHUT_TOOLS, or re-run ./install.sh if you built Mud Hut from source",
                                   missing.join(", "), tools.display()) } });
    }

    // Vulkan loader (DXVK / vkd3d-proton present via it). Every distro's 64-bit library folder, as
    // `neutron doctor` searches them: Debian and Ubuntu keep it in /usr/lib/x86_64-linux-gnu.
    let vk = has_lib64(Path::new("/"), "libvulkan.so.1") || which("vulkaninfo").is_some();
    checks.push(Check { name: "vulkan", ok: vk, required: true,
        detail: if vk { "Vulkan loader present".into() }
                else { "no Vulkan loader (libvulkan.so.1 / vulkaninfo)".into() } });

    // 32-bit C library: without /lib/ld-linux.so.2 Wine's 32-bit side cannot start and no prefix is
    // made (Neutron#29; `neutron doctor` checks the same file). Not a 32-bit library FOLDER: Fedora,
    // Bazzite and Nobara keep 32-bit libraries in /usr/lib.
    let lib32 = has_lib32_loader(Path::new("/"));
    checks.push(Check { name: "lib32", ok: lib32, required: true,
        detail: if lib32 { "32-bit C library present (/lib/ld-linux.so.2)".into() }
                else { "no 32-bit C library (/lib/ld-linux.so.2): install glibc's 32-bit package (lib32-glibc, glibc.i686 or libc6-i386)".into() } });

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

    // Adobe's Creative Cloud package (ACCCx): seeds the installer engine. Present, or
    // downloaded from Adobe on the first install.
    checks.push(Check { name: "accc", ok: true, required: false,
        detail: crate::download::accc_status() });

    let ready = checks.iter().all(|c| !c.required || c.ok);
    let report = Report { ready, checks };

    if em.is_json() {
        em.result(&report);
    } else {
        outln!("Host readiness: {}", if ready { "READY" } else { "NOT READY" });
        // Which build is this? Printed here so a stale install is visible without
        // having to compare timestamps by hand.
        outln!("  mudhut {} build {}", env!("CARGO_PKG_VERSION"), env!("MUDHUT_BUILD"));
        for c in &report.checks {
            let mark = if c.ok { "✓" } else if c.required { "✗" } else { "!" };
            outln!("  {mark} {:<9} {}", c.name, c.detail);
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
/// The 64-bit library folders `neutron doctor` searches (its LIB_DIRS), relative to `root`.
const LIB_DIRS: &[&str] = &["usr/lib64", "usr/lib/x86_64-linux-gnu", "usr/lib", "lib64", "lib/x86_64-linux-gnu", "lib",
                            "usr/local/lib64", "usr/local/lib"];

/// A 64-bit `name` in one of `LIB_DIRS` (a 32-bit copy in Fedora's /usr/lib does not count).
fn has_lib64(root: &Path, name: &str) -> bool {
    LIB_DIRS.iter().any(|d| {
        let mut head = [0u8; 5];
        std::fs::File::open(root.join(d).join(name))
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
            .is_ok()
            && &head[..4] == b"\x7fELF"
            && head[4] == 2
    })
}

/// The 32-bit dynamic loader every glibc distro installs with its 32-bit C library.
fn has_lib32_loader(root: &Path) -> bool {
    root.join("lib/ld-linux.so.2").exists()
}

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

#[cfg(test)]
mod lib_tests {
    use super::{has_lib32_loader, has_lib64};
    use std::path::Path;

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    #[test]
    fn libraries_are_found_where_each_distro_keeps_them() {
        let root = std::env::temp_dir().join(format!("mudhut-libs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Fedora: no /usr/lib32, the 32-bit loader in /lib, a 32-bit libvulkan in /usr/lib.
        put(&root, "lib/ld-linux.so.2", b"\x7fELF\x01");
        put(&root, "usr/lib/libvulkan.so.1", b"\x7fELF\x01");
        assert!(has_lib32_loader(&root));
        assert!(!has_lib64(&root, "libvulkan.so.1"), "a 32-bit copy is not the loader DXVK needs");
        // Ubuntu: the 64-bit loader in /usr/lib/x86_64-linux-gnu.
        put(&root, "usr/lib/x86_64-linux-gnu/libvulkan.so.1", b"\x7fELF\x02");
        assert!(has_lib64(&root, "libvulkan.so.1"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!has_lib32_loader(&root));
    }
}
