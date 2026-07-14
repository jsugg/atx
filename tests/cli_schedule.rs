#![allow(clippy::expect_used)]

use std::process::Command;

use tempfile::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

#[test]
fn version_and_usage_have_stable_exit_codes() {
    let version = atx().arg("version").output().expect("run version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("atx "));

    let invalid = atx()
        .args(["30s", "true"])
        .output()
        .expect("run invalid command");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
}

#[test]
fn json_dry_run_is_machine_readable_and_leaves_no_state() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["--dry-run", "30s", "--", "printf", "%s", "hello"])
        .output()
        .expect("run dry-run");

    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(value["state"], "scheduled");
    assert_eq!(value["supervised"], false);
    assert!(!state.exists());
}
