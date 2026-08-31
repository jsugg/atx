//! Operator configuration must change observable background-process behavior.
//!
//! Regression pack for the config wiring contract: `history_days`,
//! `terminal_job_days`, and `max_log_bytes_per_stream` are honored by the
//! supervisor and run monitor processes through
//! `infrastructure::config::load_process_config`, not silently replaced by
//! hardcoded defaults.

#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod support;

use support::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

/// Private state directory: atx rejects anything looser than 0700.
fn private_dir(root: &std::path::Path) -> std::path::PathBuf {
    let dir = root.join("state");
    fs::create_dir_all(&dir).expect("state dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    dir
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

/// Run `atx --json ...` and return the parsed `data` payload.
fn json(state: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .expect("run atx");
    assert!(
        output.status.success(),
        "atx {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    value.get_mut("data").map(std::mem::take).unwrap_or(value)
}

/// The configured per-stream cap bounds stored run artifacts.
#[test]
fn configured_log_cap_bounds_stored_streams() {
    const CAP: u64 = 4_096;
    let root = tempdir().expect("root");
    let state = private_dir(root.path());
    fs::write(
        state.join("config.toml"),
        format!("max_log_bytes_per_stream = {CAP}\n"),
    )
    .expect("write config");

    let submission = json(
        &state,
        &["1s", "--", "/bin/sh", "-c", "yes x | head -c 200000"],
    );
    let job_id = submission["job_id"].as_str().expect("job id").to_owned();

    wait_for(
        || {
            let rows = json(&state, &["history"])
                .as_array()
                .cloned()
                .unwrap_or_default();
            rows.iter().any(|r| {
                r["job_id"] == job_id.as_str()
                    && (r["state"] == "succeeded" || r["state"] == "failed")
            })
        },
        "flooded job to reach a terminal state",
    );

    let entries =
        fs::read_dir(state.join("runs")).expect("runs directory exists after a completed run");
    let mut checked = 0;
    for entry in entries.flatten() {
        for stream in ["stdout.log", "stderr.log"] {
            let path = entry.path().join(stream);
            if !path.exists() {
                continue;
            }
            let len = fs::metadata(&path).expect("stream metadata").len();
            assert!(
                len <= CAP,
                "{} stored {len} bytes despite the {CAP}-byte cap",
                path.display()
            );
            checked += 1;
        }
    }
    assert!(checked >= 1, "no captured streams found under state/runs");
}

/// Supervisor startup applies the operator's retention window instead of a
/// hardcoded default: a two-day-old terminal job survives the default window
/// and is purged when the configured window is one day.
#[test]
fn configured_retention_purges_old_terminal_jobs_at_startup() {
    let root = tempdir().expect("root");
    let state = private_dir(root.path());

    // Produce one real terminal job with real run/transitions rows.
    let submission = json(&state, &["1s", "--", "/bin/true"]);
    let job_id = submission["job_id"].as_str().expect("job id").to_owned();
    wait_for(
        || {
            json(&state, &["show", &job_id])["state"] == "succeeded"
                || json(&state, &["show", &job_id])["state"] == "failed"
        },
        "job to reach a terminal state",
    );

    let original_job = json(&state, &["show", &job_id]);
    let original_history = json(&state, &["history", &job_id]);
    root.stop_background()
        .expect("stop the original session supervisor");
    support::age_stopped_terminal_job(&state, &job_id, 2).expect("age a valid terminal timeline");
    let aged = json(&state, &["show", &job_id]);
    assert_eq!(aged["job_id"], job_id);
    assert_eq!(aged["state"], original_job["state"]);
    assert_aged_timestamps(&original_job, &aged, &["next_due_utc"]);
    let history = json(&state, &["history", &job_id]);
    assert_eq!(history[0]["outcome"], original_history[0]["outcome"]);
    assert_eq!(
        history.as_array().expect("aged history").len(),
        original_history.as_array().expect("original history").len()
    );
    assert_aged_timestamps(
        &original_history[0],
        &history[0],
        &["scheduled_for_utc", "started_at_utc", "finished_at_utc"],
    );

    let runtime = state.join("runtime");
    let log = root.path().join("daemon.err");
    let mut daemon = spawn_retention_daemon(&state, &runtime, &log);
    support::wait_for_service(&mut daemon, &state, &runtime, &log)
        .expect("default service readiness");
    let _ = json(&state, &["30m", "--", "/bin/true"]);
    assert_eq!(
        json(&state, &["show", &job_id])["state"],
        original_job["state"]
    );
    assert!(daemon.try_wait().expect("default daemon status").is_none());
    daemon.kill().expect("stop the default-retention daemon");
    daemon.wait().expect("reap the default-retention daemon");

    fs::write(
        state.join("config.toml"),
        "history_days = 1\nterminal_job_days = 1\n",
    )
    .expect("write config");
    let mut daemon = spawn_retention_daemon(&state, &runtime, &log);
    support::wait_for_service(&mut daemon, &state, &runtime, &log)
        .expect("configured service readiness");
    let _ = json(&state, &["30m", "--", "/bin/true"]);
    wait_for(
        || {
            let output = atx()
                .arg("--json")
                .arg("--state-dir")
                .arg(&state)
                .args(["show", &job_id])
                .output()
                .expect("show");
            if output.status.success() {
                return false;
            }
            assert_eq!(output.status.code(), Some(3), "{output:?}");
            let error: serde_json::Value =
                serde_json::from_slice(&output.stderr).expect("purge error");
            assert_eq!(error["error"]["code"], "JOB_NOT_FOUND");
            true
        },
        "configured retention to purge the two-day-old terminal job",
    );
    daemon.kill().expect("stop daemon");
    daemon.wait().expect("reap daemon");
}

fn assert_aged_timestamps(original: &serde_json::Value, aged: &serde_json::Value, fields: &[&str]) {
    for field in fields {
        let before = original.get(field).expect("original timestamp field");
        let after = aged.get(field).expect("aged timestamp field");
        if before.is_null() {
            assert!(after.is_null());
        } else {
            let before = before
                .as_str()
                .expect("original timestamp")
                .parse::<jiff::Timestamp>()
                .expect("valid timestamp");
            let after = after
                .as_str()
                .expect("aged timestamp")
                .parse::<jiff::Timestamp>()
                .expect("valid timestamp");
            assert_eq!(
                after,
                before
                    .checked_sub(jiff::SignedDuration::from_secs(172_800))
                    .expect("shift")
            );
        }
    }
}

fn spawn_retention_daemon(
    state: &std::path::Path,
    runtime: &std::path::Path,
    log: &std::path::Path,
) -> std::process::Child {
    let stderr = fs::File::create(log).expect("daemon log");
    atx()
        .arg("__supervisor")
        .arg("--state-dir")
        .arg(state)
        .arg("--runtime-dir")
        .arg(runtime)
        .arg("--service-managed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("start service-managed supervisor")
}
