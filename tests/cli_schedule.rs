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
fn shell_submission_warns_unless_quiet() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");

    // Interactive submission warns about shell interpretation.
    let warned = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--dry-run", "--shell", "30s", "--", "echo hi; rm -rf /"])
        .output()
        .expect("run shell dry-run");
    assert!(warned.status.success(), "{warned:?}");
    let stderr = String::from_utf8_lossy(&warned.stderr);
    assert!(
        stderr.contains("Warning: --shell"),
        "expected shell warning, got: {stderr}"
    );

    // --quiet suppresses the warning.
    let quiet = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--quiet", "--dry-run", "--shell", "30s", "--", "echo hi"])
        .output()
        .expect("run quiet shell dry-run");
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(
        String::from_utf8_lossy(&quiet.stderr).is_empty(),
        "quiet mode must not warn"
    );
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

#[test]
fn fixed_rate_job_runs_more_than_once_and_can_be_cancelled() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("recurring");
    let script = format!("printf 'x\\n' >>'{}'", marker.display());
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["--every", "1s", "--", "/bin/sh", "-c"])
        .arg(script)
        .output()
        .expect("submit recurring job");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("submission JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let occurrences =
            fs::read_to_string(&marker).map_or(0, |contents| contents.lines().count());
        if occurrences >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recurring job ran {occurrences} times"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let cancelled = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", job])
        .output()
        .expect("cancel recurring job");
    assert!(cancelled.status.success(), "{cancelled:?}");
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

#[test]
fn output_prints_captured_streams_after_a_run() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--", "/bin/sh", "-c"])
        .arg("printf 'on stdout'; printf 'on stderr' >&2")
        .output()
        .expect("submit job");
    assert!(submitted.status.success(), "{submitted:?}");
    let value: serde_json::Value = serde_json::from_slice(&submitted.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");
    // Second job so a single-character prefix is guaranteed ambiguous:
    // UUIDv7 first chars encode ~36h of timestamp, identical within a test.
    let second = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--", "/bin/true"])
        .output()
        .expect("submit second job");
    assert!(second.status.success(), "{second:?}");
    wait_for_history(&state, job);
    wait_for_history(
        &state,
        serde_json::from_slice::<serde_json::Value>(&second.stdout)
            .expect("second JSON")["data"]["job_id"]
            .as_str()
            .expect("second job ID"),
    );

    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read output by job ID");
    assert!(shown.status.success(), "{shown:?}");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(text.contains("on stdout"), "stdout missing: {text:?}");
    assert!(text.contains("on stderr"), "stderr missing: {text:?}");

    let encoded = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--json", "output", job])
        .output()
        .expect("read JSON output");
    assert!(encoded.status.success(), "{encoded:?}");
    let value: serde_json::Value = serde_json::from_slice(&encoded.stdout).expect("JSON envelope");
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["stdout_truncated"], false);

    let unknown = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--json", "output", "zzzzzzzzzzzzzzzzzzzzzzzzzz"])
        .output()
        .expect("read unknown run");
    assert_eq!(unknown.status.code(), Some(3));
    let error: serde_json::Value = serde_json::from_slice(&unknown.stderr).expect("JSON error");
    assert_eq!(error["error"]["code"], "JOB_NOT_FOUND");

    let ambiguous = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["output", "0"])
        .output()
        .expect("read ambiguous run");
    assert_eq!(ambiguous.status.code(), Some(3));
}

#[test]
fn tty_flag_rejects_pipe_stdout_before_creating_state() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");

    // Test harness pipes stdout: not a terminal, usage error, no state.
    let piped = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--tty", "1s", "--", "/bin/true"])
        .output()
        .expect("submit with --tty under pipe");
    assert_eq!(piped.status.code(), Some(2), "{piped:?}");
    assert!(!state.exists());
}

/// Wait until `atx history <job>` shows at least one terminal run.
fn wait_for_history(state: &std::path::Path, job: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let done = match atx()
            .arg("--state-dir")
            .arg(state)
            .args(["history", job, "--limit", "1"])
            .output()
        {
            Ok(runs) if runs.status.success() => {
                // Only a row for this job in a terminal state counts; an
                // empty or transiently failing listing must keep waiting.
                let text = String::from_utf8_lossy(&runs.stdout);
                text.lines().any(|line| {
                    line.contains(job)
                        && matches!(
                            line.split('\t').nth(2),
                            Some("Succeeded" | "Failed" | "Cancelled" | "Interrupted")
                        )
                })
            }
            _ => false,
        };
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "run never completed for {job}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn env_sources_reach_the_run_environment() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");

    // Inline --env KEY=VALUE.
    let inline = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args([
            "1s",
            "--env",
            "GAP_INLINE=inline-value",
            "--",
            "/bin/sh",
            "-c",
            "printf %s \"$GAP_INLINE\"",
        ])
        .output()
        .expect("submit inline env job");
    assert!(inline.status.success(), "{inline:?}");
    let value: serde_json::Value = serde_json::from_slice(&inline.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");
    wait_for_history(&state, job);
    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read inline env output");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(text.contains("inline-value"), "stdout missing: {text:?}");

    // --env-file KEY=VALUE lines.
    let env_file = root.path().join("gap.env");
    fs::write(&env_file, "GAP_FILE=file-value\n").expect("write env file");
    let from_file = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--env-file"])
        .arg(&env_file)
        .args(["--", "/bin/sh", "-c", "printf %s \"$GAP_FILE\""])
        .output()
        .expect("submit env-file job");
    assert!(from_file.status.success(), "{from_file:?}");
    let value: serde_json::Value = serde_json::from_slice(&from_file.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("file job ID");
    wait_for_history(&state, job);
    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read env-file output");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(text.contains("file-value"), "stdout missing: {text:?}");
}

#[test]
fn capture_env_controls_whole_environment_snapshot() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let probe = "/bin/sh -c 'printf %s \"$GAP_CAPTURE\"'";

    // Default: sanitized environment must not leak the parent's extra var.
    let sanitized = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .env("GAP_CAPTURE", "leaked")
        .args(["1s", "--", "/bin/sh", "-c"])
        .arg(probe)
        .output()
        .expect("submit sanitized job");
    assert!(sanitized.status.success(), "{sanitized:?}");
    let value: serde_json::Value = serde_json::from_slice(&sanitized.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");
    wait_for_history(&state, job);
    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read sanitized output");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(!text.contains("leaked"), "environment leaked: {text:?}");

    // --capture-env snapshots the full parent environment for this run.
    let captured = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .env("GAP_CAPTURE", "captured")
        .args(["1s", "--capture-env", "--", "/bin/sh", "-c"])
        .arg(probe)
        .output()
        .expect("submit captured job");
    assert!(captured.status.success(), "{captured:?}");
    let value: serde_json::Value = serde_json::from_slice(&captured.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");
    wait_for_history(&state, job);
    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read captured output");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(text.contains("captured"), "capture failed: {text:?}");
}

#[test]
fn name_and_description_roundtrip_through_show() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args([
            "--name",
            "gap-named-job",
            "--description",
            "gap coverage description",
            "1h",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("submit named job");
    assert!(submitted.status.success(), "{submitted:?}");
    let value: serde_json::Value = serde_json::from_slice(&submitted.stdout).expect("JSON");
    let job = value["data"]["job_id"].as_str().expect("job ID");

    let shown = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["show", job])
        .output()
        .expect("show named job");
    assert!(shown.status.success(), "{shown:?}");
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("JSON");
    assert_eq!(shown["data"]["name"], "gap-named-job");
    assert_eq!(shown["data"]["description"], "gap coverage description");

    let listed = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .arg("list")
        .output()
        .expect("list jobs");
    assert!(listed.status.success(), "{listed:?}");
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("JSON");
    assert!(
        listed.to_string().contains("gap-named-job"),
        "name missing from list: {listed}"
    );
}

#[test]
fn color_flag_toggles_ansi_markers() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");

    let always = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--color", "always", "doctor"])
        .output()
        .expect("doctor always-color");
    assert!(always.status.success(), "{always:?}");
    let text = String::from_utf8(always.stdout).expect("UTF-8");
    assert!(text.contains("\x1b["), "no ANSI codes with --color always");

    let never = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["--color", "never", "doctor"])
        .output()
        .expect("doctor never-color");
    assert!(never.status.success(), "{never:?}");
    let text = String::from_utf8(never.stdout).expect("UTF-8");
    assert!(!text.contains("\x1b["), "ANSI codes with --color never");
}
