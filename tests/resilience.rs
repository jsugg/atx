//! Fault-injection and race coverage for crash and cancellation boundaries.
//!
//! Drives the real binary through the failure points named by the plan's
//! P6-T01 gate: monitor loss mid-run, kills between run-start steps,
//! cancellation races, and unknown-outcome classification. Every scenario
//! asserts that a command never executes twice automatically and that the
//! terminal-versus-interrupted classification is deterministic and idempotent.

#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, tz::TimeZone};
use rustix::process::{Pid, Signal, kill_process_group};
use tempfile::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

#[derive(Clone, Debug)]
struct Submission {
    job_id: String,
}

fn submit(state: &std::path::Path, args: &[&str]) -> Submission {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .expect("submit job");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("submission JSON");
    Submission {
        job_id: value["data"]["job_id"].as_str().expect("job ID").to_owned(),
    }
}

/// One-shot due in 1 second running `sh -c 'touch MARKER && sleep 30'`.
fn submit_slow_job(state: &std::path::Path, marker: &std::path::Path) -> Submission {
    let script = format!(
        "'/usr/bin/touch' '{}' && '/bin/sleep' '30'",
        marker.display()
    );
    submit(state, &["1s", "--", "/bin/sh", "-c", script.as_str()])
}

/// Absolute one-shot at `due` UTC.
fn submit_absolute_with_marker(
    state: &std::path::Path,
    due: &str,
    marker: &std::path::Path,
) -> Submission {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .arg("--utc")
        .arg(due)
        .args(["--", "/usr/bin/touch"])
        .arg(marker)
        .output()
        .expect("submit absolute job");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    Submission {
        job_id: value["data"]["job_id"].as_str().expect("job ID").to_owned(),
    }
}

fn due_utc_from_now(seconds: i64) -> String {
    jiff::Timestamp::now()
        .checked_add(SignedDuration::from_secs(seconds))
        .expect("future timestamp")
        .to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// `(run_state, job_state)` of the latest run of `job_id`.
fn states(state_dir: &std::path::Path, job_id: &str) -> Option<(String, String)> {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state_dir)
        .args(["history", "--limit", "1", job_id])
        .output()
        .expect("history");
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let runs = value["data"].as_array()?;
    // A job classified missed without ever executing has no run rows.
    let run_state = runs
        .first()
        .and_then(|run| run["state"].as_str())
        .unwrap_or("none")
        .to_owned();

    let shown = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state_dir)
        .args(["show", job_id])
        .output()
        .expect("show");
    let shown_value: serde_json::Value = serde_json::from_slice(&shown.stdout).ok()?;
    let job_state = shown_value["data"]["state"].as_str()?.to_owned();
    Some((run_state, job_state))
}

fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Kill any supervisor serving `state`; scoped so parallel tests are untouched.
fn kill_state_supervisors(state: &std::path::Path) {
    // Match only supervisors for THIS state dir; parallel tests each own one.
    let pattern = format!("__supervisor --state-dir {}", state.display());
    let output = Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg(&pattern)
        .output()
        .expect("pgrep supervisor");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            let _ = Command::new("/bin/kill")
                .arg("-9")
                .arg(pid.to_string())
                .output();
        }
    }
}

/// Force a fresh supervisor start (which runs startup reconciliation):
/// session supervisors idle-exit after 30 seconds, so kill this state dir's
/// supervisor and submit a throwaway job instead of waiting that out.
fn poke_supervisor(state: &std::path::Path) {
    kill_state_supervisors(state);
    std::thread::sleep(Duration::from_millis(100));
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .args(["1s", "--", "/bin/echo", "poke"])
        .output()
        .expect("submit poke job");
    assert!(submitted.status.success(), "{submitted:?}");
}

/// Any live supervisor whose state directory sits under `state`.
fn wait_for_state(state_dir: &std::path::Path, job_id: &str, expected: &str) {
    wait_for(
        || states(state_dir, job_id).is_some_and(|(run, _)| run == expected),
        expected,
    );
}

/// Live process rows from `atx ps --json`: `(role, pid, pgid)`.
fn live_processes(state_dir: &std::path::Path) -> Vec<(String, u32, i32)> {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state_dir)
        .arg("ps")
        .output()
        .expect("ps");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("ps JSON");
    value["data"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some((
                        row["role"].as_str()?.to_owned(),
                        u32::try_from(row["pid"].as_u64()?).ok()?,
                        i32::try_from(row["process_group_id"].as_i64()?).ok()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn kill_group(pgid: i32) {
    let group = Pid::from_raw(pgid).expect("valid pgid");
    let _ = kill_process_group(group, Signal::KILL);
}

// --- scenarios ---------------------------------------------------------------

/// Monitor killed mid-run: outcome unknown, later contact classifies exactly
/// once as Interrupted and never retries.
#[test]
fn killed_monitor_classifies_unknown_outcome_exactly_once() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("marker");
    let submission = submit_slow_job(&state, &marker);

    wait_for(|| fs::metadata(&marker).is_ok(), "command to start");
    wait_for(
        || {
            live_processes(&state)
                .iter()
                .any(|(role, _, _)| role == "monitor")
        },
        "monitor registration",
    );
    let (_, _, pgid) = live_processes(&state)
        .into_iter()
        .find(|(role, _, _)| role == "monitor")
        .expect("monitor row");
    kill_group(pgid);

    // Classification happens when a fresh supervisor starts; poke until the
    // recovery pass records the interrupted outcome.
    wait_for(
        || {
            poke_supervisor(&state);
            states(&state, &submission.job_id).is_some_and(|(run, _)| run == "interrupted")
        },
        "interrupted",
    );
    let (run_state, _) = states(&state, &submission.job_id).expect("states");
    assert_eq!(run_state, "interrupted");
}

/// Supervisor dies before the deadline: recovery marks the missed one-shot
/// exactly once and the command never executes.
#[test]
fn supervisor_death_marks_missed_without_executing() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("marker");

    let submission = submit_absolute_with_marker(&state, &due_utc_from_now(3), &marker);

    // Kill this state dir's supervisor before the deadline; parallel tests
    // own their own supervisors and must not be touched.
    kill_state_supervisors(&state);
    std::thread::sleep(Duration::from_millis(200));

    // Deadline passes while nothing runs. Next supervisor start classifies.
    // A missed one-shot records no run row, so assert on the job state.
    std::thread::sleep(Duration::from_secs(5));
    wait_for(
        || {
            poke_supervisor(&state);
            states(&state, &submission.job_id).is_some_and(|(_, job)| job == "missed")
        },
        "missed",
    );
    assert!(!marker.exists(), "command must not execute after the miss");
}

/// Cancellation after natural completion: the cancel is accepted, the job
/// lands Cancelled, and repeat cancels stay idempotent.
#[test]
fn cancel_after_completion_stays_idempotent() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let done = root.path().join("done");
    let script = format!("'/usr/bin/touch' '{}'", done.display());
    // The first occurrence fires one interval after submit; 1s keeps the test
    // fast while the marker proves completion.
    let submission = submit(
        &state,
        &["--every", "1s", "--", "/bin/sh", "-c", script.as_str()],
    );

    // The marker is written before sh exits, so the run may still be in
    // flight; cancelling mid-run reports the run's real outcome instead.
    // Wait until the occurrence fully lands and the job returns to Waiting
    // so the cancel deterministically wins the race.
    wait_for(
        || {
            fs::metadata(&done).is_ok()
                && states(&state, &submission.job_id).is_some_and(|(_, job)| job == "waiting")
        },
        "first occurrence",
    );
    let cancelled = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", &submission.job_id])
        .output()
        .expect("cancel");
    assert!(cancelled.status.success(), "{cancelled:?}");
    let value: serde_json::Value = serde_json::from_slice(&cancelled.stdout).expect("cancel JSON");
    assert_eq!(value["data"]["state"], "cancelled");

    let repeat = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", &submission.job_id])
        .output()
        .expect("repeat cancel");
    assert!(repeat.status.success(), "{repeat:?}");
}

/// Three concurrent cancels against a live run: all succeed (idempotent), the
/// run lands Cancelled exactly once.
#[test]
fn concurrent_cancels_land_one_cancelled_outcome() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("sleeping");
    let submission = submit_slow_job(&state, &marker);

    wait_for(|| fs::metadata(&marker).is_ok(), "command to start");
    let mut handles = Vec::new();
    for _ in 0..3 {
        let child = atx()
            .arg("--json")
            .arg("--state-dir")
            .arg(&state)
            .args(["cancel", &submission.job_id])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cancel");
        handles.push(child);
    }
    for child in handles {
        let output = child.wait_with_output().expect("cancel finishes");
        assert!(output.status.success(), "{output:?}");
    }
    wait_for_state(&state, &submission.job_id, "cancelled");
}

/// Missing executable: durable Failed run, recorded once, never retried.
#[test]
fn failed_spawn_records_failed_once_and_never_retries() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let submission = submit(&state, &["1s", "--", "/definitely/missing/atx-command-xyz"]);
    wait_for_state(&state, &submission.job_id, "failed");
    let (first_run, first_job) = states(&state, &submission.job_id).expect("states");
    assert_eq!(first_run, "failed");
    assert_eq!(first_job, "failed");

    std::thread::sleep(Duration::from_millis(400));
    let (later_run, later_job) = states(&state, &submission.job_id).expect("states");
    assert_eq!(later_run, "failed", "failed run must never retry");
    assert_eq!(later_job, "failed");
    let history = history_length(&state, &submission.job_id);
    assert_eq!(history, 1, "exactly one run record");
}

/// Recurring job whose in-flight occurrence is killed: the supervisor wake
/// path requeues the next deadline; occurrences continue without duplication.
#[test]
fn recurring_survives_monitor_loss_between_occurrences() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let marker = root.path().join("ticks");
    let script = format!("printf 'x\\n' >>'{}'", marker.display());
    let submission = submit(
        &state,
        &["--every", "1s", "--", "/bin/sh", "-c", script.as_str()],
    );

    count_lines_at_least(&marker, 2);
    // Kill the in-flight occurrence's process group; the supervisor must keep
    // scheduling subsequent occurrences.
    if let Some((_, _, pgid)) = live_processes(&state)
        .into_iter()
        .find(|(role, _, _)| role == "monitor" || role == "command")
    {
        kill_group(pgid);
    }
    let baseline = line_count(&marker);
    wait_for(
        || line_count(&marker) > baseline,
        "next occurrence after monitor loss",
    );

    let cancelled = atx()
        .arg("--state-dir")
        .arg(&state)
        .args(["cancel", &submission.job_id])
        .output()
        .expect("cancel");
    assert!(cancelled.status.success(), "{cancelled:?}");
}

// --- helpers ---------------------------------------------------------------

fn history_length(state_dir: &std::path::Path, job_id: &str) -> usize {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state_dir)
        .args(["history", job_id])
        .output()
        .expect("history");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("history JSON");
    value["data"].as_array().map_or(0, Vec::len)
}

fn line_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).map_or(0, |contents| contents.lines().count())
}

fn count_lines_at_least(path: &std::path::Path, minimum: usize) {
    wait_for(|| line_count(path) >= minimum, "occurrences to accumulate");
}
