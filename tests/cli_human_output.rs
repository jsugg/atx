//! Human (non-JSON) presentation coverage.
//!
//! The rest of the e2e suites speak `--json`; this one drives the real
//! binary in its default human mode across management, doctor, service,
//! and completion commands so the renderers and their error paths stay
//! honest alongside the machine envelope.

#![allow(clippy::expect_used, clippy::panic)]

use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tempfile::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

/// Client pointed at the not-yet-existing state subdirectory, so ATX
/// creates it with its required 0700 mode.
fn tools(root: &std::path::Path) -> Command {
    let mut command = atx();
    command.arg("--state-dir").arg(root.join("state"));
    command
}

fn json_output(command: &mut Command) -> (Value, Output) {
    let output = command.output().expect("run atx");
    let value = serde_json::from_slice(&output.stdout).expect("JSON envelope");
    (value, output)
}

fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn lifecycle_commands_render_human_envelopes() {
    let root = tempdir().expect("root");
    let marker = root.path().join("marker");
    let script = format!("'/usr/bin/touch' '{}'", marker.display());
    let (submission, submitted) = json_output(tools(root.path()).args([
        "--json",
        "1s",
        "--",
        "/bin/sh",
        "-c",
        script.as_str(),
    ]));
    assert!(submitted.status.success(), "{submitted:?}");
    let job_id = submission["data"]["job_id"]
        .as_str()
        .expect("job id")
        .to_owned();

    // Poll through the JSON envelope so assertions below stay about the
    // human renderers rather than about timing.
    wait_for(|| marker.exists(), "the job to execute");
    wait_for(
        || {
            let (value, _) = json_output(tools(root.path()).args(["--json", "show", &job_id]));
            value["data"]["state"] == "succeeded"
        },
        "the job to reach succeeded",
    );

    let listed = tools(root.path()).args(["list"]).output().expect("list");
    assert!(listed.status.success(), "{listed:?}");
    let listing = String::from_utf8_lossy(&listed.stdout);
    assert!(listing.contains(&job_id), "{listing}");

    let colored = tools(root.path())
        .args(["--color", "always", "list"])
        .output()
        .expect("colored list");
    assert!(
        String::from_utf8_lossy(&colored.stdout).contains('\x1b'),
        "explicit color mode must emit escapes"
    );

    let shown = tools(root.path())
        .args(["show", &job_id])
        .output()
        .expect("show");
    assert!(shown.status.success(), "{shown:?}");
    let detail = String::from_utf8_lossy(&shown.stdout);
    assert!(detail.contains(&job_id), "{detail}");

    let (history, _) = json_output(tools(root.path()).args(["--json", "history"]));
    let run_id = history["data"][0]["run_id"]
        .as_str()
        .expect("run id")
        .to_owned();
    let runs = tools(root.path())
        .args(["history"])
        .output()
        .expect("history");
    assert!(runs.status.success(), "{runs:?}");
    let run_lines = String::from_utf8_lossy(&runs.stdout);
    assert!(run_lines.contains(&run_id), "{run_lines}");
    assert!(run_lines.contains(&job_id), "{run_lines}");

    let processes = tools(root.path()).args(["ps"]).output().expect("ps");
    let process_lines = String::from_utf8_lossy(&processes.stdout);
    assert_eq!(process_lines.trim(), "No live ATX processes.");

    let printed = tools(root.path())
        .args(["output", &run_id])
        .output()
        .expect("output");
    assert!(printed.status.success(), "{printed:?}");
    let printed_text = String::from_utf8_lossy(&printed.stdout);
    assert!(printed_text.contains(&run_id), "{printed_text}");

    let removed = tools(root.path())
        .args(["rm", "--keep-history", &job_id])
        .output()
        .expect("rm");
    assert!(removed.status.success(), "{removed:?}");
    let removal = String::from_utf8_lossy(&removed.stdout);
    assert!(removal.contains(&job_id), "{removal}");
}

#[test]
fn doctor_service_version_and_completions_render() {
    let root = tempdir().expect("root");

    let (doctor_json, doctor_json_output) =
        json_output(tools(root.path()).args(["--json", "doctor"]));
    assert!(
        doctor_json_output.status.success(),
        "{doctor_json_output:?}"
    );
    let tzdb_version = doctor_json["data"]["tzdb_version"]
        .as_str()
        .expect("tzdb version");
    let doctor = tools(root.path()).arg("doctor").output().expect("doctor");
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(report.contains(tzdb_version), "{report}");

    let (service_json, service_json_output) =
        json_output(tools(root.path()).args(["--json", "service", "status"]));
    assert!(
        service_json_output.status.success(),
        "{service_json_output:?}"
    );
    let service = tools(root.path())
        .args(["service", "status"])
        .output()
        .expect("service status");
    assert!(service.status.success(), "{service:?}");
    let status = String::from_utf8_lossy(&service.stdout);
    for field in ["manager", "guarantee"] {
        let value = service_json["data"][field]
            .as_str()
            .expect("service status field");
        assert!(status.contains(value), "{status}");
    }

    let version = tools(root.path()).arg("version").output().expect("version");
    assert!(version.status.success());
    assert!(!version.stdout.is_empty());

    let completions = tools(root.path())
        .args(["completions", "--shell", "bash"])
        .output()
        .expect("completions");
    assert!(completions.status.success());
    assert!(!completions.stdout.is_empty());
}

#[test]
fn human_errors_report_exit_codes_on_stderr() {
    let root = tempdir().expect("root");

    // No database yet: collection reads are empty successes in every mode.
    let missing = tools(root.path())
        .arg("list")
        .output()
        .expect("list without state");
    assert!(missing.status.success(), "{missing:?}");
    assert_eq!(String::from_utf8_lossy(&missing.stdout).trim(), "No jobs.");
    assert!(missing.stderr.is_empty(), "{missing:?}");
    for command in ["list", "history", "ps"] {
        let empty = tools(root.path())
            .args(["--json", command])
            .output()
            .expect("empty JSON read");
        assert!(empty.status.success(), "{command}: {empty:?}");
        let value: serde_json::Value =
            serde_json::from_slice(&empty.stdout).expect("JSON empty result");
        assert_eq!(value["data"], serde_json::json!([]), "{command}");
    }

    // With a database present, an unknown prefix is still not_found.
    let submitted = tools(root.path())
        .args(["1h", "--", "true"])
        .output()
        .expect("submit");
    assert!(submitted.status.success(), "{submitted:?}");
    let unknown = tools(root.path())
        .args(["show", "nosuch"])
        .output()
        .expect("unknown job");
    assert_eq!(unknown.status.code(), Some(3));

    // Unknown --state values are rejected by the typed enum before any
    // store access, so this exits with clap's usage code.
    let usage = tools(root.path())
        .args(["list", "--state", "bogus"])
        .output()
        .expect("usage");
    assert_eq!(usage.status.code(), Some(2));
    assert!(!usage.stderr.is_empty());

    // The typed enum accepts exactly the documented state names.
    for state in [
        "scheduled",
        "waiting",
        "starting",
        "running",
        "cancel-requested",
        "succeeded",
        "failed",
        "cancelled",
        "interrupted",
        "missed",
    ] {
        let listed = tools(root.path())
            .args(["--json", "list", "--state", state])
            .output()
            .expect("filtered list");
        assert!(
            listed.status.success(),
            "--state {state}: {}",
            String::from_utf8_lossy(&listed.stderr)
        );
    }
}

/// Submits a 1s touch-job plus a pending 30m job, waits until the short job
/// has a terminal outcome, and returns its `(job_id, run_id)`.
fn finished_job(root: &std::path::Path) -> (String, String) {
    let marker = root.join("marker");
    let script = format!("'/usr/bin/touch' '{}'", marker.display());
    let submitted = tools(root)
        .args(["--json", "1s", "--", "/bin/sh", "-c", script.as_str()])
        .output()
        .expect("submit");
    assert!(submitted.status.success(), "{submitted:?}");
    tools(root)
        .args(["--json", "30m", "--", "/bin/true"])
        .output()
        .expect("pending submit");

    wait_for(|| marker.exists(), "the job to execute");
    wait_for(
        || {
            let (value, _) = json_output(tools(root).args(["--json", "history"]));
            value["data"]
                .as_array()
                .is_some_and(|runs| runs.iter().any(|run| run["outcome"] != Value::Null))
        },
        "the run to finish",
    );
    let (history, _) = json_output(tools(root).args(["--json", "history"]));
    let data = history["data"].as_array().expect("runs");
    let run = data
        .iter()
        .find(|run| run["outcome"] != Value::Null)
        .expect("finished run");
    (
        run["job_id"].as_str().expect("job id").to_owned(),
        run["run_id"].as_str().expect("run id").to_owned(),
    )
}

/// Golden key-set contract: every `data` payload exposes exactly the fields
/// docs/json-api.md promises. A new field must land in both places.
#[test]
fn json_job_payloads_expose_the_documented_key_sets() {
    fn expect_keys(value: &Value, expected: &[&str]) {
        let mut actual: Vec<&str> = value
            .as_object()
            .expect("payload")
            .keys()
            .map(String::as_str)
            .collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    let root = tempdir().expect("root");
    let (submission, _) =
        json_output(tools(root.path()).args(["--json", "30m", "--", "/bin/true"]));
    expect_keys(
        &submission["data"],
        &[
            "dry_run",
            "job_id",
            "next_due_utc",
            "runtime_tier",
            "schedule",
            "state",
            "supervised",
        ],
    );

    let _ = finished_job(root.path());
    let (listing, _) = json_output(tools(root.path()).args(["--json", "list"]));
    expect_keys(&listing, &["data", "ok", "schema_version"]);
    let job = listing["data"][0].clone();
    let job_id = job["job_id"].as_str().expect("job id").to_owned();
    expect_keys(
        &job,
        &[
            "active_run_id",
            "description",
            "execution",
            "job_id",
            "name",
            "next_due_utc",
            "remaining_seconds",
            "runtime_tier",
            "schedule",
            "state",
        ],
    );
    expect_keys(
        &job["execution"],
        &[
            "argv",
            "environment_keys",
            "mode",
            "shell_path",
            "working_directory",
        ],
    );

    // show renders the same job object shape as list.
    let (shown, _) = json_output(tools(root.path()).args(["--json", "show", &job_id]));
    let shown_keys: Vec<String> = shown["data"]
        .as_object()
        .expect("job payload")
        .keys()
        .cloned()
        .collect();
    let listed_keys: Vec<String> = job
        .as_object()
        .expect("job payload")
        .keys()
        .cloned()
        .collect();
    assert_eq!(shown_keys, listed_keys);
}

#[test]
fn json_run_payloads_expose_the_documented_key_sets() {
    fn expect_keys(value: &Value, expected: &[&str]) {
        let mut actual: Vec<&str> = value
            .as_object()
            .expect("payload")
            .keys()
            .map(String::as_str)
            .collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    let root = tempdir().expect("root");
    let (job_id, run_id) = finished_job(root.path());

    let (history, _) = json_output(tools(root.path()).args(["--json", "history"]));
    let run = history["data"][0].clone();
    expect_keys(
        &run,
        &[
            "finished_at_utc",
            "job_id",
            "outcome",
            "run_id",
            "scheduled_for_utc",
            "sequence",
            "started_at_utc",
            "state",
            "stderr_path",
            "stdout_path",
        ],
    );

    let (output, _) = json_output(tools(root.path()).args(["--json", "output", &run_id]));
    expect_keys(
        &output["data"],
        &[
            "job_id",
            "outcome",
            "run_id",
            "state",
            "stderr",
            "stderr_truncated",
            "stdout",
            "stdout_truncated",
        ],
    );

    let (processes, _) = json_output(tools(root.path()).args(["--json", "ps"]));
    if let Some(process) = processes["data"].as_array().and_then(|p| p.first()) {
        expect_keys(
            process,
            &[
                "job_id",
                "pid",
                "process_group_id",
                "role",
                "run_id",
                "state",
            ],
        );
    }
    assert!(
        history["data"]
            .as_array()
            .expect("runs")
            .iter()
            .all(|run| run["job_id"] == job_id)
    );
}
