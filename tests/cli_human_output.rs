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
mod support;

use support::tempdir;

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
    assert!(
        output.stdout.ends_with(b"\n"),
        "JSON must end in a newline: {output:?}"
    );
    let value = json_line(&output.stdout);
    (value, output)
}

fn json_line(bytes: &[u8]) -> Value {
    let line = bytes.strip_suffix(b"\n").expect("JSON newline");
    assert!(!line.contains(&b'\n'), "JSON must fit on one line");
    serde_json::from_slice(line).expect("JSON envelope")
}

fn expect_keys(value: &Value, expected: &[&str]) {
    let mut actual: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, expected);
}

fn expect_identifier(value: &Value) {
    let id = value.as_str().expect("identifier string");
    assert_eq!(id.len(), 26);
    assert!(
        id.bytes()
            .all(|byte| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&byte))
    );
    assert!(id.as_bytes()[0] <= b'7', "identifier exceeds 128 bits");
}

fn expect_timestamp(value: &Value) {
    let timestamp = value.as_str().expect("timestamp string");
    timestamp
        .parse::<jiff::Timestamp>()
        .expect("RFC 3339 timestamp");
    assert!(timestamp.ends_with('Z'), "timestamp must be UTC");
}

#[test]
fn json_v1_values_and_types_are_stable() {
    let root = tempdir().expect("root");
    let (dry, output) =
        json_output(tools(root.path()).args(["--json", "--dry-run", "30s", "--", "/usr/bin/true"]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(dry["schema_version"], 1);
    assert_eq!(dry["ok"], true);
    let submitted = &dry["data"];
    expect_identifier(&submitted["job_id"]);
    expect_timestamp(&submitted["next_due_utc"]);
    assert_eq!(submitted["state"], "scheduled");
    assert_eq!(submitted["runtime_tier"], "session");
    assert_eq!(submitted["supervised"], false);
    assert_eq!(submitted["dry_run"], true);
    assert_eq!(submitted["schedule"]["kind"], "one_shot_relative");
    assert_eq!(submitted["schedule"]["duration"], "30s");
    expect_keys(
        &submitted["schedule"],
        &["duration", "kind", "persisted_due_utc"],
    );
    expect_timestamp(&submitted["schedule"]["persisted_due_utc"]);

    let (job_id, run_id) = finished_job(root.path());
    let (job, _) = json_output(tools(root.path()).args(["--json", "show", &job_id]));
    let job = &job["data"];
    expect_identifier(&job["job_id"]);
    expect_timestamp(&job["next_due_utc"]);
    assert!(job["remaining_seconds"].is_i64());
    assert_eq!(job["state"], "succeeded");
    assert_eq!(job["runtime_tier"], "session");
    assert!(job["name"].is_null());
    assert!(job["description"].is_null());
    assert!(job["active_run_id"].is_null());
    assert!(job.get("outcome").is_none());
    assert_eq!(job["execution"]["mode"], "direct");
    assert!(
        job["execution"]["argv"]
            .as_array()
            .expect("argv")
            .iter()
            .all(Value::is_string)
    );
    assert!(
        std::path::Path::new(
            job["execution"]["working_directory"]
                .as_str()
                .expect("working directory")
        )
        .is_absolute()
    );
    assert!(
        job["execution"]["environment_keys"]
            .as_array()
            .expect("keys")
            .iter()
            .all(Value::is_string)
    );
    assert!(job["execution"]["shell_path"].is_null());

    let (history, _) = json_output(tools(root.path()).args(["--json", "history", &job_id]));
    let run = &history["data"][0];
    expect_identifier(&run["job_id"]);
    expect_identifier(&run["run_id"]);
    assert_eq!(run["sequence"], 1);
    for field in ["scheduled_for_utc", "started_at_utc", "finished_at_utc"] {
        expect_timestamp(&run[field]);
    }
    assert_eq!(run["state"], "succeeded");
    assert_eq!(run["outcome"], serde_json::json!({"kind":"exit","value":0}));
    assert!(run["stdout_path"].is_string());
    assert!(run["stderr_path"].is_string());
    let (captured, _) = json_output(tools(root.path()).args(["--json", "output", &run_id]));
    assert_eq!(captured["data"]["outcome"], run["outcome"]);
    for field in ["stdout", "stderr"] {
        assert!(captured["data"][field].is_string());
    }
    for field in ["stdout_truncated", "stderr_truncated"] {
        assert!(captured["data"][field].is_boolean());
    }
    let (doctor, output) = json_output(tools(root.path()).args(["--json", "doctor"]));
    assert!(output.status.success(), "{output:?}");
    assert!(
        doctor["data"]["schema_version"]
            .as_u64()
            .is_some_and(|version| version > 0)
    );
}

fn documented_example(heading: &str) -> Value {
    let docs = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/json-api.md"))
        .expect("JSON guide");
    let section = docs
        .split(heading)
        .nth(1)
        .expect("documented example section");
    let example = section
        .split("```json\n")
        .nth(1)
        .expect("JSON example")
        .split("```")
        .next()
        .expect("example end");
    serde_json::from_str(example).expect("valid documented JSON")
}

#[test]
fn documented_version_example_matches_the_binary() {
    let expected = documented_example("### Version example");
    let (actual, output) = json_output(atx().args(["--json", "version"]));
    assert!(output.status.success());
    assert_eq!(actual, expected);
}

#[test]
fn documented_error_example_matches_the_binary() {
    let root = tempdir().expect("root");
    let output = tools(root.path())
        .args(["--json", "show", "nosuch"])
        .output()
        .expect("missing job");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    let actual = json_line(&output.stderr);
    assert_eq!(actual, documented_example("### Error example"));

    let invalid = tools(root.path())
        .args(["--json", "0s", "--", "true"])
        .output()
        .expect("invalid duration");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    let invalid = json_line(&invalid.stderr);
    expect_keys(&invalid, &["error", "ok", "schema_version"]);
    expect_keys(&invalid["error"], &["code", "message"]);
    assert_eq!(invalid["schema_version"], 1);
    assert_eq!(invalid["ok"], false);
    assert_eq!(invalid["error"]["code"], "INVALID_ARGUMENT");
    assert!(invalid["error"]["message"].is_string());
}

#[test]
fn json_schedule_variants_keep_their_field_types() {
    let root = tempdir().expect("root");
    let (absolute, output) = json_output(tools(root.path()).args([
        "--json",
        "--dry-run",
        "--tz",
        "Europe/London",
        "--dst",
        "later",
        "2099-01-02T03:04:05",
        "--",
        "true",
    ]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(absolute["data"]["runtime_tier"], "session");
    let schedule = &absolute["data"]["schedule"];
    expect_keys(
        schedule,
        &[
            "dst_resolution",
            "kind",
            "original_input",
            "resolved_due_utc",
            "timezone",
            "timezone_database_version",
        ],
    );
    assert_eq!(schedule["kind"], "one_shot_absolute");
    assert_eq!(schedule["original_input"], "2099-01-02T03:04:05");
    assert_eq!(schedule["timezone"], "Europe/London");
    assert!(schedule["timezone_database_version"].is_string());
    assert_eq!(schedule["resolved_due_utc"], "2099-01-02T03:04:05Z");
    assert_eq!(schedule["dst_resolution"], "later");
    assert_eq!(
        absolute["data"]["next_due_utc"],
        schedule["resolved_due_utc"]
    );

    let (recurring, output) = json_output(tools(root.path()).args([
        "--json",
        "--dry-run",
        "--every",
        "120s",
        "--",
        "true",
    ]));
    assert!(output.status.success(), "{output:?}");
    let schedule = &recurring["data"]["schedule"];
    expect_keys(schedule, &["interval", "kind", "persisted_anchor_utc"]);
    assert_eq!(schedule["kind"], "recurring_interval");
    assert_eq!(schedule["interval"], "2m");
    expect_timestamp(&schedule["persisted_anchor_utc"]);
    assert_eq!(
        recurring["data"]["next_due_utc"],
        schedule["persisted_anchor_utc"]
    );
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
fn json_doctor_report_has_stable_keys_and_types() {
    let root = tempdir().expect("root");
    let (doctor_json, output) = json_output(tools(root.path()).args(["--json", "doctor"]));
    assert!(output.status.success(), "{output:?}");
    expect_keys(&doctor_json, &["data", "ok", "schema_version"]);
    let data = &doctor_json["data"];
    expect_keys(
        data,
        &[
            "checks",
            "config",
            "durable_available",
            "healthy",
            "schema_version",
            "tzdb_version",
        ],
    );
    assert_eq!(doctor_json["ok"], true);
    assert_eq!(doctor_json["schema_version"], 1);
    assert_eq!(data["healthy"], true);
    assert!(data["durable_available"].is_boolean());
    assert!(data["schema_version"].is_null());
    let checks = data["checks"].as_array().expect("checks");
    assert!(!checks.is_empty());
    for check in checks {
        let keys = if check.get("remediation").is_some() {
            &["message", "name", "remediation", "status"][..]
        } else {
            &["message", "name", "status"][..]
        };
        expect_keys(check, keys);
        assert!(check["name"].is_string());
        assert!(check["message"].is_string());
        assert!(matches!(
            check["status"].as_str(),
            Some("pass" | "warning" | "fail")
        ));
        if let Some(remediation) = check.get("remediation") {
            assert!(remediation.is_string());
        }
    }
    expect_keys(
        &data["config"],
        &[
            "cancel_grace",
            "color",
            "default_runtime",
            "default_shell",
            "default_timezone",
            "history_days",
            "max_log_bytes_per_stream",
            "terminal_job_days",
            "verbosity",
        ],
    );
    for field in [
        "cancel_grace",
        "color",
        "default_runtime",
        "default_shell",
        "default_timezone",
        "verbosity",
    ] {
        assert!(data["config"][field].is_string(), "{field}");
    }
    for field in [
        "history_days",
        "max_log_bytes_per_stream",
        "terminal_job_days",
    ] {
        assert!(data["config"][field].is_u64(), "{field}");
    }
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
    let data = &service_json["data"];
    expect_keys(
        data,
        &[
            "availability",
            "detail",
            "files",
            "guarantee",
            "installed",
            "manager",
            "running",
        ],
    );
    assert!(matches!(
        data["availability"].as_str(),
        Some("available" | "unavailable")
    ));
    for field in ["detail", "guarantee", "manager"] {
        assert!(data[field].is_string());
    }
    for field in ["installed", "running"] {
        assert!(data[field].is_boolean());
    }
    assert!(
        data["files"]
            .as_array()
            .expect("service files")
            .iter()
            .all(Value::is_string)
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

    let (cancelled, output) = json_output(tools(root.path()).args(["--json", "cancel", &job_id]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        cancelled["data"]
            .as_object()
            .expect("cancelled job")
            .keys()
            .collect::<Vec<_>>(),
        job.as_object()
            .expect("listed job")
            .keys()
            .collect::<Vec<_>>()
    );
    let (removed, output) = json_output(tools(root.path()).args(["--json", "rm", &job_id]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(removed["data"]["job_id"], cancelled["data"]["job_id"]);
    assert_eq!(removed["data"]["state"], cancelled["data"]["state"]);
    assert_eq!(
        removed["data"]
            .as_object()
            .expect("removed job")
            .keys()
            .collect::<Vec<_>>(),
        cancelled["data"]
            .as_object()
            .expect("cancelled job")
            .keys()
            .collect::<Vec<_>>()
    );
}

#[test]
fn json_run_payloads_expose_the_documented_key_sets() {
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

    assert!(
        history["data"]
            .as_array()
            .expect("runs")
            .iter()
            .all(|run| run["job_id"] == job_id)
    );

    let (rerun, output) = json_output(tools(root.path()).args(["--json", "run", &job_id, "--yes"]));
    assert!(output.status.success(), "{output:?}");
    expect_keys(
        &rerun["data"],
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
    assert_eq!(rerun["data"]["job_id"], job_id);
}

#[test]
fn json_processes_describe_a_live_command_and_monitor() {
    let root = tempdir().expect("root");
    let ready = root.path().join("ready");
    let release = root.path().join("release");
    let (submitted, output) = json_output(tools(root.path()).args([
        "--json",
        "--shell",
        "--name",
        "held",
        "--description",
        "contract test",
        "1s",
        "--",
        &format!(
            "touch '{}'; while [ ! -f '{}' ]; do sleep 1; done",
            ready.display(),
            release.display()
        ),
    ]));
    assert!(output.status.success(), "{output:?}");
    let job_id = submitted["data"]["job_id"].as_str().expect("job id");
    wait_for(|| ready.exists(), "the held command to start");
    wait_for(
        || {
            let (history, output) =
                json_output(tools(root.path()).args(["--json", "history", job_id]));
            output.status.success() && history["data"][0]["state"] == "running"
        },
        "the running identities to be saved",
    );
    let (processes, output) = json_output(tools(root.path()).args(["--json", "ps"]));
    assert!(output.status.success(), "{output:?}");
    let processes = processes["data"].as_array().expect("processes");
    assert_eq!(processes.len(), 2, "{processes:?}");
    let mut roles = Vec::new();
    for process in processes {
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
        expect_identifier(&process["job_id"]);
        expect_identifier(&process["run_id"]);
        assert_eq!(process["job_id"], job_id);
        assert!(
            process["pid"]
                .as_u64()
                .is_some_and(|pid| pid > 0 && u32::try_from(pid).is_ok())
        );
        assert!(
            process["process_group_id"]
                .as_i64()
                .is_some_and(|pgid| pgid > 0 && i32::try_from(pgid).is_ok())
        );
        assert_eq!(process["state"], "running");
        roles.push(process["role"].as_str().expect("role"));
    }
    roles.sort_unstable();
    assert_eq!(roles, ["command", "monitor"]);
    let (history, output) = json_output(tools(root.path()).args(["--json", "history", job_id]));
    assert!(output.status.success(), "{output:?}");
    let run = &history["data"][0];
    assert_eq!(run["state"], "running");
    assert!(run["outcome"].is_null());
    assert!(run["finished_at_utc"].is_null());
    expect_timestamp(&run["started_at_utc"]);
    let (job, output) = json_output(tools(root.path()).args(["--json", "show", job_id]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(job["data"]["name"], "held");
    assert_eq!(job["data"]["description"], "contract test");
    assert_eq!(job["data"]["execution"]["mode"], "shell");
    assert_eq!(job["data"]["execution"]["shell_path"], "/bin/sh");
    assert!(job["data"]["active_run_id"].is_null());
    let (_, output) =
        json_output(tools(root.path()).args(["--json", "cancel", job_id, "--grace", "1s"]));
    assert!(output.status.success(), "{output:?}");
}
