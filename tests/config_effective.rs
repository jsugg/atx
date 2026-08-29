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
/// hardcoded default: an ancient terminal job is purged when the configured
/// window is one day.
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

    // Backdate every row of that job beyond any default window.
    let db = state.join("atx.db");
    let ancient = "2020-01-01T00:00:00Z";
    for statement in [
        format!("UPDATE runs SET finished_at_utc = '{ancient}' WHERE job_id = '{job_id}';"),
        format!("UPDATE transitions SET occurred_at_utc = '{ancient}' WHERE job_id = '{job_id}';"),
        format!("UPDATE jobs SET updated_at_utc = '{ancient}' WHERE id = '{job_id}';"),
    ] {
        let result = Command::new("sqlite3")
            .arg(&db)
            .arg(statement)
            .output()
            .expect("sqlite3 available");
        assert!(
            result.status.success(),
            "backdate failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // A one-day retention window deletes the ancient job on next startup.
    fs::write(
        state.join("config.toml"),
        "history_days = 1\nterminal_job_days = 1\n",
    )
    .expect("write config");

    let runtime = root.path().join("runtime");
    let stderr_log = fs::File::create(root.path().join("daemon.err")).expect("daemon log");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_atx"))
        .arg("__supervisor")
        .arg("--state-dir")
        .arg(&state)
        .arg("--runtime-dir")
        .arg(&runtime)
        .arg("--service-managed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_log)
        .spawn()
        .expect("start service-managed supervisor");

    wait_for(
        || {
            let output = atx()
                .arg("--json")
                .arg("--state-dir")
                .arg(&state)
                .args(["show", &job_id])
                .output()
                .expect("show");
            !output.status.success()
        },
        "retention to purge the ancient terminal job",
    );

    daemon.kill().expect("stop daemon");
    daemon.wait().expect("reap daemon");
}
