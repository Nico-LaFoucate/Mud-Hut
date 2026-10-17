// SPDX-License-Identifier: Apache-2.0
//! Mud Hut's per-prefix state, inside the prefix: `<prefix>/.mudhut/`.
//!
//! - `lock`: an `flock` held for the whole of an install, update or rollback. It is the one
//!   authority for "Mud Hut is working on this prefix": `neutron launch` refuses and `neutron
//!   update` skips re-provisioning while it is held, and a second Mud Hut run waits its turn.
//!   The kernel drops it when the process dies, so a crash never leaves a stale lock.
//! - `state.json`: what is running (`in_progress`) and how the last update ended
//!   (`last_update`), written at every stage, so `check-updates` can report an update that
//!   outlived the Collider window that started it, or one that was cut off.
//! - `update.log`: every event of the last update, one JSON object per line.
//! - `logs/`: HDPIM's own logs from updates. `snapshots/`: rollback snapshots.
//!
//! Inside the prefix on purpose: per prefix by construction (the lesson of 2026-09-18, every
//! per-app identifier must be per prefix), it moves and is deleted with the prefix, and it sits
//! on the same filesystem as the app folders, so snapshots can reflink and rollback can rename.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub fn dir(prefix: &Path) -> PathBuf {
    prefix.join(".mudhut")
}

pub fn lock_path(prefix: &Path) -> PathBuf {
    dir(prefix).join("lock")
}

pub fn state_path(prefix: &Path) -> PathBuf {
    dir(prefix).join("state.json")
}

pub fn log_path(prefix: &Path) -> PathBuf {
    dir(prefix).join("update.log")
}

pub fn logs_dir(prefix: &Path) -> PathBuf {
    dir(prefix).join("logs")
}

/// The held prefix lock; released when dropped (or when the process ends).
pub struct Lock {
    _file: File,
}

/// Take the prefix lock without waiting. Held by another run: a Failure with exit 3 that names
/// what holds it.
pub fn lock(prefix: &Path, command: &str) -> Result<Lock> {
    std::fs::create_dir_all(dir(prefix)).with_context(|| format!("creating {}", dir(prefix).display()))?;
    let path = lock_path(prefix);
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        let st = read_state(prefix);
        let what = st
            .in_progress
            .as_ref()
            .map(|p| format!("{} of {} (pid {})", p.command, p.apps.join(", "), p.pid))
            .unwrap_or_else(|| "another Mud Hut run".into());
        return Err(crate::failure::Failure::new(
            "update-running",
            crate::failure::EX_RUNNING,
            format!("Mud Hut is already working on this prefix ({what}). Wait for it to finish, then try again."),
        )
        .with("prefix_changed", false)
        .into());
    }
    let _ = f.set_len(0);
    let _ = writeln!(f, "{} {} {}", std::process::id(), command, crate::clock::now());
    Ok(Lock { _file: f })
}

/// Whether some process holds the prefix lock right now (never creates the file).
pub fn is_locked(prefix: &Path) -> bool {
    let Ok(f) = File::open(lock_path(prefix)) else { return false };
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if r == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InProgress {
    pub pid: u32,
    pub command: String,
    pub apps: Vec<String>,
    pub started_at: String,
    pub stage: String,
    pub log: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastUpdate {
    pub ok: bool,
    pub app: String,
    /// "updated" | "rolled_back" | "incomplete" | "unchanged".
    pub state: String,
    pub message: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub in_progress: Option<InProgress>,
    #[serde(default)]
    pub last_update: Option<LastUpdate>,
    #[serde(default)]
    pub history: Vec<LastUpdate>,
}

pub fn read_state(prefix: &Path) -> State {
    std::fs::read_to_string(state_path(prefix)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Write state.json atomically (temp + rename).
pub fn write_state(prefix: &Path, st: &State) -> Result<()> {
    std::fs::create_dir_all(dir(prefix))?;
    let mut st = st.clone();
    st.schema = 1;
    if st.history.len() > 20 {
        let n = st.history.len() - 20;
        st.history.drain(..n);
    }
    let tmp = dir(prefix).join("state.json.part");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&st)?)?;
    std::fs::rename(&tmp, state_path(prefix))?;
    Ok(())
}

/// What `check-updates` reports for a prefix: the update running now (only while its lock is
/// held), and the last outcome, turning an `in_progress` whose process is gone into an
/// "incomplete" last update.
pub fn report(prefix: &Path) -> (Option<InProgress>, Option<LastUpdate>) {
    let st = read_state(prefix);
    let locked = is_locked(prefix);
    match (&st.in_progress, locked) {
        (Some(p), true) => (Some(p.clone()), st.last_update.clone()),
        (Some(p), false) => (
            None,
            Some(LastUpdate {
                ok: false,
                app: p.apps.join(", "),
                state: "incomplete".into(),
                message: "The last update stopped before it finished. Run the update again to finish it; \
                          the apps are put back to how they were first."
                    .into(),
                at: p.started_at.clone(),
                from: None,
                to: None,
                log: Some(p.log.clone()),
            }),
        ),
        (None, _) => (None, st.last_update.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_visible_and_dies_with_the_holder() {
        let p = std::env::temp_dir().join(format!("mudhut-lock-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        assert!(!is_locked(&p));
        let l = lock(&p, "update").unwrap();
        assert!(is_locked(&p));
        let second = lock(&p, "update").err().expect("a second lock must be refused");
        assert_eq!(crate::failure::find(&second).unwrap().exit, 3);
        drop(l);
        assert!(!is_locked(&p));
        // A crashed holder: a child takes the lock and is killed; the kernel releases it.
        // `-o`: the lock fd is not inherited by `sleep`, so killing flock frees it.
        let mut child = std::process::Command::new("flock")
            .arg("-o")
            .arg(lock_path(&p))
            .args(["sleep", "30"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(is_locked(&p));
        let _ = child.kill();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!is_locked(&p));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn a_dead_in_progress_reads_as_incomplete() {
        let p = std::env::temp_dir().join(format!("mudhut-state-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        let st = State {
            in_progress: Some(InProgress { pid: 1, command: "update".into(), apps: vec!["audition".into()], ..Default::default() }),
            ..Default::default()
        };
        write_state(&p, &st).unwrap();
        let (running, last) = report(&p);
        assert!(running.is_none());
        let last = last.unwrap();
        assert_eq!(last.state, "incomplete");
        assert!(!last.ok);
        let _ = std::fs::remove_dir_all(&p);
    }
}
