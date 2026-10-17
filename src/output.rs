// SPDX-License-Identifier: Apache-2.0
//! Output protocol — the contract Collider (and scripts) hook into.
//!
//! In `--json` mode every line on stdout is one JSON event:
//!   {"event":"progress","stage":"copy","pct":42,"msg":"Adobe Photoshop 2025"}
//!   {"event":"result", ...command-specific fields... }   <- exactly one, terminal
//!   {"event":"error","message":"...","code":"...","exit":N} <- terminal, on failure
//! A consumer reads lines as they arrive for progress and treats the final
//! `result`/`error` as the outcome.
//!
//! Without `--json`, progress goes to stderr (human-readable) and the terminal
//! result is left to the command to summarize on stdout.
//!
//! ⛔ NOTHING HERE MAY PANIC ON A CLOSED STREAM. `println!` panics when the write fails, and
//! the write fails the moment the reader goes away: closing Collider closed the pipe, Mud Hut
//! died on its next progress line, `PR_SET_PDEATHSIG` then took the installer host down with it,
//! and the install stopped half-way. Every write below ignores its error; after the first
//! failure that stream is dropped for good and the run carries on. An update also tees every
//! event into `<prefix>/.mudhut/update.log`, so a reader that comes back later can still see
//! what happened.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};

static STDOUT_GONE: AtomicBool = AtomicBool::new(false);
static STDERR_GONE: AtomicBool = AtomicBool::new(false);

/// Write one line to stdout; never panics. A failed write (EPIPE: the reader closed) disables
/// stdout for the rest of the run.
pub fn write_out(args: std::fmt::Arguments) {
    if STDOUT_GONE.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    if out.write_fmt(args).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush()).is_err() {
        STDOUT_GONE.store(true, Ordering::Relaxed);
    }
}

/// Write one line to stderr; never panics (a closed terminal answers EIO).
pub fn write_err(args: std::fmt::Arguments) {
    if STDERR_GONE.load(Ordering::Relaxed) {
        return;
    }
    let mut err = std::io::stderr().lock();
    if err.write_fmt(args).and_then(|_| err.write_all(b"\n")).and_then(|_| err.flush()).is_err() {
        STDERR_GONE.store(true, Ordering::Relaxed);
    }
}

/// `println!` that never panics.
macro_rules! outln {
    ($($t:tt)*) => { $crate::output::write_out(format_args!($($t)*)) };
}

/// `eprintln!` that never panics.
macro_rules! errln {
    ($($t:tt)*) => { $crate::output::write_err(format_args!($($t)*)) };
}

pub struct Emitter {
    json: bool,
    /// When set, every event is also appended here as one NDJSON line (whatever the mode).
    log: Mutex<Option<File>>,
    /// The app the current events are about (an update names it on every line, so Collider can
    /// put progress on the right card).
    app: Mutex<Option<String>>,
    /// While set, every progress event carries this stage (an update reports the install
    /// engine's own steps as its `apply` stage).
    stage: Mutex<Option<String>>,
}

impl Emitter {
    pub fn new(json: bool) -> Self {
        Emitter { json, log: Mutex::new(None), app: Mutex::new(None), stage: Mutex::new(None) }
    }

    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Tee every following event into `path` (appended, created if needed).
    pub fn attach_log(&self, path: &Path) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        *self.log.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        Ok(())
    }

    /// Name the app the following progress events belong to (None: not app-specific).
    pub fn set_app(&self, app: Option<&str>) {
        *self.app.lock().unwrap_or_else(|e| e.into_inner()) = app.map(str::to_string);
    }

    /// Report every following progress event under `stage` (None: as the caller names it).
    pub fn set_stage(&self, stage: Option<&str>) {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner()) = stage.map(str::to_string);
    }

    fn current_app(&self) -> Option<String> {
        self.app.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A progress tick. `pct` is 0..=100; `stage` is a short machine token.
    pub fn progress(&self, stage: &str, pct: u8, msg: &str) {
        let over = self.stage.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let stage = over.as_deref().unwrap_or(stage);
        let mut v = json!({"event":"progress","stage":stage,"pct":pct,"msg":msg});
        if let Some(app) = self.current_app() {
            v["app"] = json!(app);
        }
        self.tee(&v);
        if self.json {
            self.line(&v);
        } else {
            errln!("[{pct:>3}%] {stage}: {msg}");
        }
    }

    /// An informational note (no percentage).
    pub fn note(&self, msg: &str) {
        let mut v = json!({"event":"note","msg":msg});
        if let Some(app) = self.current_app() {
            v["app"] = json!(app);
        }
        self.tee(&v);
        if self.json {
            self.line(&v);
        } else {
            errln!("  {msg}");
        }
    }

    /// A non-terminal structured event (e.g. `app_result`). `v` must carry its own `event` tag.
    /// In human mode `human` is printed to stderr instead.
    pub fn event(&self, v: &Value, human: &str) {
        self.tee(v);
        if self.json {
            self.line(v);
        } else if !human.is_empty() {
            errln!("{human}");
        }
    }

    /// The single terminal success event. `value` must serialize to a JSON object;
    /// an `event: "result"` tag is injected. In non-JSON mode nothing is printed
    /// here — the command prints its own human summary.
    pub fn result<T: Serialize>(&self, value: &T) {
        let mut v = serde_json::to_value(value).unwrap_or_else(|_| json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("event".into(), json!("result"));
        }
        self.tee(&v);
        if self.json {
            self.line(&v);
        }
    }

    /// The terminal failure event (called from main on Err). `extra` carries the machine fields
    /// (`code`, `exit`, `prefix_changed`, ...) of a [`crate::failure::Failure`].
    pub fn error_with(&self, message: &str, extra: &serde_json::Map<String, Value>) {
        let mut v = json!({"event":"error","message":message});
        if let Some(obj) = v.as_object_mut() {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        }
        self.tee(&v);
        if self.json {
            self.line(&v);
        } else {
            errln!("error: {message}");
        }
    }

    fn tee(&self, v: &Value) {
        if let Some(f) = self.log.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "{v}");
            let _ = f.flush();
        }
    }

    fn line(&self, v: &Value) {
        outln!("{v}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_teed_to_the_log_with_the_app() {
        let dir = std::env::temp_dir().join(format!("mudhut-tee-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("update.log");
        let em = Emitter::new(false);
        em.attach_log(&log).unwrap();
        em.set_app(Some("audition"));
        em.progress("apply", 40, "x");
        em.set_app(None);
        em.note("n");
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["app"], "audition");
        assert_eq!(lines[0]["stage"], "apply");
        assert!(lines[1].get("app").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
