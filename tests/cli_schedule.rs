#![allow(clippy::expect_used, clippy::panic)]

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
        .args(["4s", "--", "/usr/bin/touch"])
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
    let deadline = Instant::now() + Duration::from_secs(15);
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

fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Wait until `atx show <job>` reports a terminal job state. The run row and
/// the job finalize are separate transactions, so the job state is what
/// gates rerun/remove.
fn wait_for_history(state: &std::path::Path, job: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = match atx()
            .arg("--state-dir")
            .arg(state)
            .args(["show", job])
            .output()
        {
            Ok(job_view) if job_view.status.success() => {
                // Only a terminal job counts; an empty or transiently
                // failing listing must keep waiting.
                String::from_utf8_lossy(&job_view.stdout)
                    .lines()
                    .any(|line| {
                        matches!(
                            line.trim(),
                            "State: Succeeded"
                                | "State: Failed"
                                | "State: Cancelled"
                                | "State: Interrupted"
                                | "State: Missed"
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

#[test]
fn rm_refuses_live_then_removes_terminal_jobs() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");

    // Live (waiting) job: plain rm refuses with a state conflict; --cancel
    // removes it.
    let live = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1h", "--", "/bin/true"])
        .output()
        .expect("submit live job");
    assert!(live.status.success(), "{live:?}");
    let live_job = serde_json::from_slice::<serde_json::Value>(&live.stdout).expect("JSON")["data"]
        ["job_id"]
        .as_str()
        .expect("live job ID")
        .to_owned();

    let refused = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["rm", &live_job])
        .output()
        .expect("rm live job");
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let error: serde_json::Value = serde_json::from_slice(&refused.stderr).expect("JSON error");
    assert_eq!(error["error"]["code"], "STATE_CONFLICT");

    let removed = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["rm", &live_job, "--cancel"])
        .output()
        .expect("rm --cancel live job");
    assert!(removed.status.success(), "{removed:?}");

    let listed = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .arg("list")
        .output()
        .expect("list after rm");
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("JSON");
    assert!(
        !listed.to_string().contains(&live_job),
        "removed job still listed: {listed}"
    );

    // Terminal job: removal drops history unless --keep-history.
    for keep in [false, true] {
        let done = atx()
            .arg("--json")
            .arg("--state-dir")
            .arg(&state)
            .args(["1s", "--", "/bin/true"])
            .output()
            .expect("submit terminal-candidate job");
        assert!(done.status.success(), "{done:?}");
        let job = serde_json::from_slice::<serde_json::Value>(&done.stdout).expect("JSON")["data"]
            ["job_id"]
            .as_str()
            .expect("job ID")
            .to_owned();
        wait_for_history(&state, &job);

        let mut args = vec!["rm", job.as_str()];
        if keep {
            args.push("--keep-history");
        }
        let removed = atx()
            .arg("--state-dir")
            .arg(&state)
            .args(args)
            .output()
            .expect("rm terminal job");
        assert!(removed.status.success(), "{removed:?}");

        // The job record is gone either way, so per-job lookups fail; the
        // global history listing is what shows whether runs were retained.
        let history = atx()
            .arg("--json")
            .arg("--state-dir")
            .arg(&state)
            .arg("history")
            .output()
            .expect("global history after rm");
        assert!(history.status.success(), "{history:?}");
        let history: serde_json::Value = serde_json::from_slice(&history.stdout).expect("JSON");
        let rows = history["data"]
            .as_array()
            .expect("history rows")
            .iter()
            .filter(|row| row["job_id"] == job.as_str())
            .count();
        if keep {
            assert_eq!(rows, 1, "--keep-history must retain runs: {history}");
        } else {
            assert_eq!(rows, 0, "history must be dropped: {history}");
        }
    }
}

#[test]
fn run_reruns_completed_job_and_appends_history() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--", "/bin/true"])
        .output()
        .expect("submit rerun candidate");
    assert!(submitted.status.success(), "{submitted:?}");
    let job = serde_json::from_slice::<serde_json::Value>(&submitted.stdout).expect("JSON")["data"]
        ["job_id"]
        .as_str()
        .expect("job ID")
        .to_owned();
    wait_for_history(&state, &job);

    // A completed job reruns without confirmation.
    let rerun = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["run", &job])
        .output()
        .expect("run completed job again");
    assert!(rerun.status.success(), "{rerun:?}");

    wait_for(
        || {
            let history = atx()
                .arg("--json")
                .arg("--state-dir")
                .arg(&state)
                .args(["history", &job])
                .output()
                .expect("history count");
            let history: serde_json::Value = serde_json::from_slice(&history.stdout).expect("JSON");
            history["data"]
                .as_array()
                .is_some_and(|runs| runs.len() >= 2)
        },
        "second run to appear",
    );
}

#[test]
fn cancel_grace_escalates_to_kill_for_term_immune_process() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("started");
    // TERM is trapped away, so only the post-grace KILL can stop this run.
    let script = format!("trap '' TERM; touch '{}'; sleep 30", marker.display());
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--", "/bin/sh", "-c"])
        .arg(script)
        .output()
        .expect("submit term-immune job");
    assert!(submitted.status.success(), "{submitted:?}");
    let job = serde_json::from_slice::<serde_json::Value>(&submitted.stdout).expect("JSON")["data"]
        ["job_id"]
        .as_str()
        .expect("job ID")
        .to_owned();

    wait_for_file(&marker);
    let cancelled = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", &job, "--grace", "2s"])
        .output()
        .expect("cancel term-immune job");
    assert!(cancelled.status.success(), "{cancelled:?}");
    wait_for_history(&state, &job);

    let history = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["history", &job])
        .output()
        .expect("history of cancelled run");
    let history: serde_json::Value = serde_json::from_slice(&history.stdout).expect("JSON");
    assert_eq!(history["data"][0]["state"], "cancelled");
}

#[test]
fn cwd_runs_child_in_requested_directory() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let workdir = root.path().join("workdir");
    fs::create_dir(&workdir).expect("create workdir");
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--cwd"])
        .arg(&workdir)
        .args(["--", "/bin/pwd"])
        .output()
        .expect("submit cwd job");
    assert!(submitted.status.success(), "{submitted:?}");
    let job = serde_json::from_slice::<serde_json::Value>(&submitted.stdout).expect("JSON")["data"]
        ["job_id"]
        .as_str()
        .expect("job ID")
        .to_owned();
    wait_for_history(&state, &job);

    let shown = atx()
        .arg("--state-dir")
        .arg(&state)
        .arg("output")
        .arg(job)
        .output()
        .expect("read pwd output");
    let text = String::from_utf8(shown.stdout).expect("UTF-8");
    assert!(
        text.contains(
            workdir
                .canonicalize()
                .expect("canonical workdir")
                .to_str()
                .expect("UTF-8 path")
        ),
        "child did not run in --cwd directory: {text:?}"
    );
}

#[test]
fn dst_policy_resolves_fold_and_reject_is_usage_error() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    // 2099-11-01 01:30 America/New_York is inside the fall-back fold: local
    // 01:30 happens twice (EDT -04:00 then EST -05:00).
    let due = |policy: &str| {
        atx()
            .arg("--json")
            .arg("--state-dir")
            .arg(&state)
            .args([
                "--dry-run",
                "--tz",
                "America/New_York",
                "--dst",
                policy,
                "2099-11-01 01:30",
                "--",
                "/bin/true",
            ])
            .output()
            .expect("dry-run fold time")
    };

    let earlier = due("earlier");
    assert!(earlier.status.success(), "{earlier:?}");
    let earlier: serde_json::Value = serde_json::from_slice(&earlier.stdout).expect("JSON");
    let later = due("later");
    assert!(later.status.success(), "{later:?}");
    let later: serde_json::Value = serde_json::from_slice(&later.stdout).expect("JSON");
    assert_ne!(
        earlier["data"]["next_due_utc"], later["data"]["next_due_utc"],
        "fold policies must resolve to distinct instants"
    );

    let rejected = due("reject");
    assert_eq!(rejected.status.code(), Some(2), "{rejected:?}");
    assert!(!state.exists());
}

#[test]
fn completions_emit_scripts_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "power-shell"] {
        let output = atx()
            .args(["completions", "--shell", shell])
            .output()
            .expect("generate completions");
        assert!(output.status.success(), "{shell}: {output:?}");
        let script = String::from_utf8(output.stdout).expect("UTF-8 script");
        assert!(
            !script.is_empty(),
            "{shell} completion script must not be empty"
        );
    }
}

#[test]
fn ps_lists_monitor_and_command_roles_while_job_runs() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("started");
    // A short lead keeps the suite fast; 5s leaves room for a cold supervisor
    // start on loaded runners, where startup reconciliation would otherwise
    // mark the overdue one-shot missed before the wake lands.
    let script = format!("touch '{}'; sleep 30", marker.display());
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["5s", "--", "/bin/sh", "-c"])
        .arg(script)
        .output()
        .expect("submit ps candidate");
    assert!(submitted.status.success(), "{submitted:?}");
    let job = serde_json::from_slice::<serde_json::Value>(&submitted.stdout).expect("JSON")["data"]
        ["job_id"]
        .as_str()
        .expect("job ID")
        .to_owned();

    wait_for_file(&marker);

    let mut monitor_seen = false;
    let mut command_seen = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(monitor_seen && command_seen) {
        let listed = atx()
            .arg("--json")
            .arg("--state-dir")
            .arg(&state)
            .arg("ps")
            .output()
            .expect("run ps");
        assert!(listed.status.success(), "{listed:?}");
        let value: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("ps JSON");
        for row in value["data"].as_array().into_iter().flatten() {
            monitor_seen |= row["role"] == "monitor";
            command_seen |= row["role"] == "command";
        }
        assert!(
            Instant::now() < deadline,
            "ps missing roles (monitor={monitor_seen} command={command_seen})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let cancelled = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", &job])
        .output()
        .expect("cancel ps candidate");
    assert!(cancelled.status.success(), "{cancelled:?}");
}
