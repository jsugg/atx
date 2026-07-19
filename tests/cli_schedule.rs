#![allow(clippy::expect_used)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp, tz::TimeZone};
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
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/json-error-v1.json")).expect("JSON fixture");
    assert_eq!(error["schema_version"], fixture["schema_version"]);
    assert_eq!(error["ok"], fixture["ok"]);
    assert_eq!(error["error"]["code"], fixture["error"]["code"]);

    let exit_codes: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/exit-codes-v1.json")).expect("exit fixture");
    let expected_usage = exit_codes["usage"]
        .as_i64()
        .and_then(|code| i32::try_from(code).ok());
    assert_eq!(invalid.status.code(), expected_usage);
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

#[test]
fn doctor_json_reports_capabilities_without_creating_state() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .arg("doctor")
        .output()
        .expect("run doctor");

    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["healthy"], true);
    assert_eq!(value["data"]["durable_available"], false);
    assert!(!state.exists());
}

#[test]
fn durable_submission_never_falls_back_to_session_mode() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["--durable", "30s", "--", "true"])
        .output()
        .expect("run durable submission");

    assert_eq!(output.status.code(), Some(5));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("valid JSON error");
    assert_eq!(value["error"]["code"], "CAPABILITY_UNAVAILABLE");
    assert!(!state.exists());
}

#[test]
fn service_status_reports_exact_artifact_without_creating_state() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["service", "status"])
        .output()
        .expect("run service status");

    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON status");
    assert!(
        value["data"]["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    );
    assert!(!state.exists());
}

#[test]
fn relative_job_survives_submitter_terminal_closure() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("relative");
    let mut child = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--", "/usr/bin/touch"])
        .arg(&marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start submitter");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for submitter");

    assert!(output.status.success(), "{output:?}");
    wait_for_file(&marker);
}

#[test]
fn absolute_utc_job_runs_end_to_end() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("absolute");
    let due = Timestamp::now()
        .checked_add(SignedDuration::from_secs(2))
        .expect("future timestamp")
        .to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string();
    let output = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("--utc")
        .arg(due)
        .args(["--", "/usr/bin/touch"])
        .arg(&marker)
        .output()
        .expect("submit absolute job");

    assert!(output.status.success(), "{output:?}");
    wait_for_file(&marker);
}

fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if fs::metadata(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(path.exists(), "{} was not created", path.display());
}
