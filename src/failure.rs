// SPDX-License-Identifier: Apache-2.0
//! Structured failures: a machine `code`, a process exit code, and the fields Collider needs to
//! decide what to tell the user (`prefix_changed` above all: "nothing happened, try later" versus
//! "we put the old version back" versus "needs attention").
//!
//! Exit codes extend neutron's 0-4 (bin/neutron "Exit codes") so both CLIs read the same:
//!   0 ok · 1 failure · 2 bad prefix · 3 an app or installer is running · 4 missing dependency ·
//!   5 network or download failed, prefix unchanged · 6 not enough disk space ·
//!   7 failed and rolled back · 8 failed AND the rollback failed.

use serde_json::{json, Map, Value};

pub const EX_OK: u8 = 0;
pub const EX_FAIL: u8 = 1;
pub const EX_BADPREFIX: u8 = 2;
pub const EX_RUNNING: u8 = 3;
pub const EX_DEP: u8 = 4;
pub const EX_NET: u8 = 5;
pub const EX_DISK: u8 = 6;
pub const EX_ROLLED_BACK: u8 = 7;
pub const EX_ROLLBACK_FAILED: u8 = 8;

/// A failure that carries its own exit code and machine fields. Raise it with [`fail`] and
/// friends; `main` turns it into the terminal `error` event.
#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub exit: u8,
    pub message: String,
    pub fields: Map<String, Value>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl Failure {
    pub fn new(code: &'static str, exit: u8, message: impl Into<String>) -> Self {
        Failure { code, exit, message: message.into(), fields: Map::new() }
    }

    /// Attach a machine field (e.g. `prefix_changed`, `apps_running`).
    pub fn with(mut self, key: &str, v: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), v.into());
        self
    }

    /// The fields of the terminal `error` event, besides `message`.
    pub fn event_fields(&self) -> Map<String, Value> {
        let mut m = self.fields.clone();
        m.insert("code".into(), json!(self.code));
        m.insert("exit".into(), json!(self.exit));
        m
    }
}

/// `Err(anyhow)` carrying a [`Failure`].
pub fn fail(code: &'static str, exit: u8, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Failure::new(code, exit, message))
}

/// Find a [`Failure`] anywhere in an error chain.
pub fn find(e: &anyhow::Error) -> Option<&Failure> {
    e.chain().find_map(|c| c.downcast_ref::<Failure>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn a_failure_survives_added_context() {
        let r: anyhow::Result<()> = Err(Failure::new("app-running", EX_RUNNING, "close it").with("prefix_changed", false).into());
        let e = r.context("updating").unwrap_err();
        let f = find(&e).expect("failure in chain");
        assert_eq!(f.exit, 3);
        let ev = f.event_fields();
        assert_eq!(ev["code"], "app-running");
        assert_eq!(ev["prefix_changed"], false);
    }
}
