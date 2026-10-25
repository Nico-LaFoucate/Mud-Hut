// SPDX-License-Identifier: Apache-2.0
//! What is running in a prefix, read from /proc without wine.
//!
//! A process belongs to a prefix when its environment carries `WINEPREFIX=<prefix>` as a WHOLE
//! entry (neutron's `_prefix_procs` rule: a substring test once made `~/mudhut-accc` own every
//! process of `~/mudhut-accc2`). The prefix is compared both as given and canonicalized, because
//! a symlinked home (`/home` -> `/var/home`) reaches the same prefix by two spellings.
//! A wine process's identity is its argv[0], the Windows path it was started as; /proc/<pid>/exe
//! only names the wine loader.
//!
//! Classes, in the order an update cares about them:
//! - `apps`: anything started from an app folder under `Program Files\Adobe\<App>` (every
//!   Adobe app, not just the one being updated: Dynamic Link ties them together and the update
//!   must never close a window the user has open).
//! - `installers`: Adobe's installers and Creative Cloud desktop, and another Mud Hut decrypt.
//! - `helpers`: Adobe's background processes (IPCBroker, CCXProcess, CoreSync, node, the crash
//!   reporter...). They outlive the apps; with no app open they are closed the way
//!   `neutron prefix provision` closes them.
//! - `other`: a Windows program that is none of the above (a plug-in installer, a tool).
//! - wine's own system processes, which end on their own.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Wine's own processes (bin/neutron WINE_SYSTEM_PROCS).
const WINE_SYSTEM: &[&str] = &[
    "wineserver", "services.exe", "winedevice.exe", "plugplay.exe", "explorer.exe", "svchost.exe",
    "rpcss.exe", "start.exe", "conhost.exe", "rundll32.exe", "tabtip.exe", "wineboot.exe",
];

/// Adobe's installer engine and Creative Cloud desktop (by exe file name, lower case).
const INSTALLERS: &[&str] = &[
    "hdpim_host.exe", "set-up.exe", "setup.exe", "hdhelper.exe", "uninstaller.exe",
    "adobe update helper.exe", "creative cloud.exe", "creative cloud helper.exe",
    "adobegcinvoker-1.0.exe", "remoteupdatemanager.exe",
];

/// Folders under `Program Files\Adobe` that hold shared machinery, not an app.
const SHARED_DIRS: &[&str] = &["common", "adobe creative cloud", "adobe creative cloud experience"];

#[derive(Debug, Clone, Serialize)]
pub struct Proc {
    pub pid: i32,
    pub comm: String,
    /// argv[0], `\` normalized to `/`.
    pub win_exe: String,
}

impl Proc {
    pub fn exe_name(&self) -> String {
        self.win_exe.rsplit('/').next().unwrap_or(&self.win_exe).to_string()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Busy {
    /// Display names of the apps with a live process ("Premiere Pro", "Adobe Bridge 2026").
    pub apps: Vec<String>,
    /// Exe names of running installers / Creative Cloud desktop.
    pub installers: Vec<String>,
    /// Exe names of Adobe background helpers.
    pub helpers: Vec<String>,
    /// Exe names of other Windows programs.
    pub other: Vec<String>,
    /// Any process at all (wine's own included) is alive in the prefix.
    pub any: bool,
}

impl Busy {
    /// Something the user must close first: an app, an installer, or an unknown program.
    pub fn blocking(&self) -> bool {
        !self.apps.is_empty() || !self.installers.is_empty() || !self.other.is_empty()
    }
    /// Names for a "close these first" sentence (apps, then installers, then other programs).
    pub fn blocking_names(&self) -> Vec<String> {
        let mut v = self.apps.clone();
        v.extend(self.installers.iter().cloned());
        v.extend(self.other.iter().cloned());
        v
    }
}

fn same_prefix(value: &[u8], literal: &str, canon: Option<&Path>) -> bool {
    let Ok(v) = std::str::from_utf8(value) else { return false };
    let v = v.trim_end_matches('/');
    if v == literal {
        return true;
    }
    match canon {
        Some(c) => std::fs::canonicalize(v).map(|p| p == c).unwrap_or(false),
        None => false,
    }
}

/// Every live process whose environment names this prefix, excluding our own process and
/// non-Windows processes (a shell or Python that merely inherited `WINEPREFIX`).
pub fn prefix_procs(prefix: &Path) -> Vec<Proc> {
    let literal = prefix.to_string_lossy().trim_end_matches('/').to_string();
    let canon: Option<PathBuf> = std::fs::canonicalize(prefix).ok();
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        let hit = env.split(|b| *b == 0).any(|entry| {
            entry.strip_prefix(b"WINEPREFIX=").map(|v| same_prefix(v, &literal, canon.as_deref())).unwrap_or(false)
        });
        if !hit {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim().to_string();
        let argv0 = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|c| c.split(|b| *b == 0).next().map(|a| String::from_utf8_lossy(a).replace('\\', "/")))
            .unwrap_or_default();
        let windowsy = argv0.contains(":/") || argv0.to_ascii_lowercase().ends_with(".exe") || comm == "wineserver"
            || comm.to_ascii_lowercase().ends_with(".exe");
        if !windowsy {
            continue;
        }
        out.push(Proc { pid, comm, win_exe: argv0 });
    }
    out
}

/// The app folder a Windows exe path runs from (`Adobe Photoshop 2026`), if it is one.
pub fn app_dir_of(win_exe: &str) -> Option<String> {
    let lower = win_exe.to_ascii_lowercase();
    let key = "/program files/adobe/";
    let i = lower.find(key)? + key.len();
    let rest = &win_exe[i..];
    let dir = rest.split('/').next()?.to_string();
    if dir.is_empty() || !rest.contains('/') || SHARED_DIRS.contains(&dir.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(dir)
}

/// Classify the processes of `prefix`.
pub fn scan(prefix: &Path) -> Busy {
    classify(&prefix_procs(prefix))
}

pub fn classify(procs: &[Proc]) -> Busy {
    let mut b = Busy { any: !procs.is_empty(), ..Default::default() };
    let push = |v: &mut Vec<String>, s: String| {
        if !v.contains(&s) {
            v.push(s);
        }
    };
    for p in procs {
        let name = p.exe_name();
        let lname = name.to_ascii_lowercase();
        let lcomm = p.comm.to_ascii_lowercase();
        if WINE_SYSTEM.contains(&lcomm.as_str()) || WINE_SYSTEM.contains(&lname.as_str())
            || p.win_exe.to_ascii_lowercase().starts_with("c:/windows/")
        {
            continue;
        }
        if INSTALLERS.contains(&lname.as_str()) || INSTALLERS.contains(&lcomm.as_str()) {
            push(&mut b.installers, name);
            continue;
        }
        if let Some(dir) = app_dir_of(&p.win_exe) {
            let display = crate::catalog::apps()
                .into_iter()
                .find(|a| dir.starts_with(a.dir_prefix))
                .map(|a| a.name.trim_end_matches(" (experimental)").to_string())
                .unwrap_or(dir);
            push(&mut b.apps, display);
            continue;
        }
        if p.win_exe.to_ascii_lowercase().contains("/adobe/") {
            push(&mut b.helpers, name);
            continue;
        }
        push(&mut b.other, name);
    }
    b
}

/// Plain sentence naming what must be closed, e.g. "Close Premiere Pro and After Effects first."
pub fn close_first_sentence(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!("Close {} first.", name_list(names))
}

/// "A", "A and B", "A, B, and C".
pub fn name_list(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        2 => format!("{} and {}", names[0], names[1]),
        _ => format!("{}, and {}", names[..names.len() - 1].join(", "), names[names.len() - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(exe: &str, comm: &str) -> Proc {
        Proc { pid: 1, comm: comm.into(), win_exe: exe.replace('\\', "/") }
    }

    #[test]
    fn classifies_apps_installers_helpers_and_wine() {
        let b = classify(&[
            p(r"C:\Program Files\Adobe\Adobe Premiere Pro 2026\Adobe Premiere Pro.exe", "Adobe Premiere "),
            p(r"C:\Program Files\Adobe\Adobe After Effects 2026\Support Files\AfterFX.exe", "AfterFX.exe"),
            p(r"C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common\IPCBox\AdobeIPCBroker.exe", "AdobeIPCBroker."),
            p(r"C:\Program Files (x86)\Adobe\Adobe Creative Cloud Experience\CCXProcess.exe", "CCXProcess.exe"),
            p(r"C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common\HDBox\Set-up.exe", "Set-up.exe"),
            p(r"Z:\var\tmp\tools\hdpim_host.exe", "hdpim_host.exe"),
            p(r"C:\windows\system32\services.exe", "services.exe"),
            p(r"C:\windows\system32\winedevice.exe", "winedevice.exe"),
            p(r"C:\users\me\Downloads\plugin-installer.exe", "plugin-installer"),
        ]);
        assert_eq!(b.apps, vec!["Premiere Pro", "After Effects"]);
        assert_eq!(b.installers, vec!["Set-up.exe", "hdpim_host.exe"]);
        assert_eq!(b.helpers, vec!["AdobeIPCBroker.exe", "CCXProcess.exe"]);
        assert_eq!(b.other, vec!["plugin-installer.exe"]);
        assert!(b.blocking());
        let quiet = classify(&[p(r"C:\windows\system32\services.exe", "services.exe")]);
        assert!(!quiet.blocking() && quiet.any);
    }

    #[test]
    fn the_shared_common_folder_is_not_an_app() {
        assert_eq!(app_dir_of("C:/Program Files/Adobe/Common/Plug-ins/x.exe"), None);
        assert_eq!(app_dir_of("C:/Program Files/Adobe/Adobe Photoshop 2026/Photoshop.exe").as_deref(), Some("Adobe Photoshop 2026"));
    }

    #[test]
    fn sentences() {
        assert_eq!(close_first_sentence(&["Photoshop".into()]), "Close Photoshop first.");
        assert_eq!(close_first_sentence(&["A".into(), "B".into(), "C".into()]), "Close A, B, and C first.");
    }

    // The whole-entry rule against a real process: a child started with WINEPREFIX=<p>2 must not
    // be seen in <p>, and is seen in <p>2 (by both the literal and a symlinked spelling).
    #[test]
    fn whole_entry_and_symlinked_spelling() {
        let base = std::env::temp_dir().join(format!("mudhut-busy-{}", std::process::id()));
        let (a, a2) = (base.join("pfx"), base.join("pfx2"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&a2).unwrap();
        let link = base.join("link-to-pfx2");
        let _ = std::os::unix::fs::symlink(&a2, &link);
        // argv[0] like a wine process: a Windows path.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exec -a 'C:\\Program Files\\Adobe\\Adobe Photoshop 2026\\Photoshop.exe' sleep 30"])
            .env("WINEPREFIX", &a2)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(scan(&a).apps.is_empty(), "a prefix must not own its sibling's processes");
        assert_eq!(scan(&a2).apps, vec!["Photoshop"]);
        assert_eq!(scan(&link).apps, vec!["Photoshop"], "symlinked spelling");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&base);
    }
}
