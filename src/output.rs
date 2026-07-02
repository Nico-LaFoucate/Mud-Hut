// SPDX-License-Identifier: Apache-2.0
//! Output protocol — the contract Collider (and scripts) hook into.
//!
//! In `--json` mode every line on stdout is one JSON event:
//!   {"event":"progress","stage":"copy","pct":42,"msg":"Adobe Photoshop 2025"}
//!   {"event":"result", ...command-specific fields... }   <- exactly one, terminal
//!   {"event":"error","message":"..."}                    <- terminal, on failure
//! A consumer reads lines as they arrive for progress and treats the final
//! `result`/`error` as the outcome.
//!
//! Without `--json`, progress goes to stderr (human-readable) and the terminal
//! result is left to the command to summarize on stdout.

use std::io::Write;

use serde::Serialize;
use serde_json::{json, Value};

pub struct Emitter {
    json: bool,
}

impl Emitter {
    pub fn new(json: bool) -> Self {
        Emitter { json }
    }

    pub fn is_json(&self) -> bool {
        self.json
    }

    /// A progress tick. `pct` is 0..=100; `stage` is a short machine token.
    pub fn progress(&self, stage: &str, pct: u8, msg: &str) {
        if self.json {
            self.line(json!({"event":"progress","stage":stage,"pct":pct,"msg":msg}));
        } else {
            eprintln!("[{pct:>3}%] {stage}: {msg}");
            let _ = std::io::stderr().flush();
        }
    }

    /// An informational note (no percentage).
    pub fn note(&self, msg: &str) {
        if self.json {
            self.line(json!({"event":"note","msg":msg}));
        } else {
            eprintln!("  {msg}");
        }
    }

    /// The single terminal success event. `value` must serialize to a JSON object;
    /// an `event: "result"` tag is injected. In non-JSON mode nothing is printed
    /// here — the command prints its own human summary.
    pub fn result<T: Serialize>(&self, value: &T) {
        if !self.json {
            return;
        }
        let mut v = serde_json::to_value(value).unwrap_or_else(|_| json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("event".into(), json!("result"));
        }
        self.line(v);
    }

    /// A tagged intermediate event carrying structured data (e.g. the auth prompt
    /// with the sign-in URL + QR). JSON mode emits it verbatim; human mode is left
    /// to the command to render.
    pub fn event(&self, value: Value) {
        if self.json {
            self.line(value);
        }
    }

    /// The terminal failure event (called from main on Err).
    pub fn error(&self, message: &str) {
        if self.json {
            self.line(json!({"event":"error","message":message}));
        } else {
            eprintln!("error: {message}");
        }
    }

    fn line(&self, v: Value) {
        println!("{v}");
        let _ = std::io::stdout().flush();
    }
}
