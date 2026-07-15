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

    let invalid_json = atx()
        .args(["--json", "30s", "true"])
        .output()
        .expect("run invalid JSON command");
    assert_eq!(invalid_json.status.code(), Some(2));
    let error: serde_json::Value =
        serde_json::from_slice(&invalid_json.stderr).expect("JSON error");
    assert_eq!(error["schema_version"], 1);
    assert_eq!(error["error"]["code"], "INVALID_ARGUMENT");
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
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["state"], "scheduled");
    assert_eq!(value["data"]["supervised"], false);
    assert!(!state.exists());
}
