// SPDX-License-Identifier: Apache-2.0
//! A reader that goes away must not take Mud Hut with it. Run the real binary with its stdout
//! (and stderr) connected to pipes whose read ends are already closed, the state Collider leaves
//! behind when its window closes: every write answers EPIPE. Before output.rs stopped using
//! `println!`, the first line panicked and the process exited 101.

use std::process::{Command, Stdio};

fn run_with_closed_pipes(args: &[&str]) -> std::process::ExitStatus {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mudhut"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mudhut");
    drop(child.stdout.take());
    drop(child.stderr.take());
    child.wait().expect("wait")
}

#[test]
fn json_mode_survives_a_closed_stdout() {
    let st = run_with_closed_pipes(&["--json", "apps"]);
    assert!(st.success(), "mudhut --json apps died on a closed stdout: {st}");
}

#[test]
fn human_mode_survives_closed_stdout_and_stderr() {
    let st = run_with_closed_pipes(&["apps"]);
    assert!(st.success(), "mudhut apps died on a closed stdout/stderr: {st}");
}
