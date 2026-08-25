//! Service-managed supervisor coverage.
//!
//! Drives the real binary as an init-style daemon (`__supervisor
//! --service-managed`) — the mode launchd/systemd run — and asserts the
//! pipeline that only that mode exercises end to end: startup recovery,
//! Wake acknowledgement over the runtime socket, deadline firing, run-monitor
//! spawning, and a clean stop when the unit is signalled.

#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};
use tempfile::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
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

/// The runtime path a client with `--state-dir S` resolves to: submissions
/// look for the supervisor socket there, so the pre-started daemon must own
/// it for its Wake handling and execution path to be exercised.
fn runtime_directory(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state").join("runtime")
}

/// Spawn a service-managed supervisor over fresh state/runtime directories.
fn spawn_daemon(root: &std::path::Path) -> std::process::Child {
    // Point the daemon at not-yet-existing subdirectories: atx creates them
    // with its required 0700 mode, while a bare tempdir is world-readable.
    let stderr_log = fs::File::create(root.join("daemon.err")).expect("err log");
    Command::new(env!("CARGO_BIN_EXE_atx"))
        .arg("__supervisor")
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--runtime-dir")
        .arg(runtime_directory(root))
        .arg("--service-managed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_log)
        .spawn()
        .expect("start service-managed supervisor")
}

#[test]
fn service_managed_daemon_executes_due_jobs_and_stops_cleanly_on_sigterm() {
    let root = tempdir().expect("root");
    let mut daemon = spawn_daemon(root.path());
    let socket = runtime_directory(root.path()).join("supervisor.sock");

    // The socket binds before the database finishes migrating; wait for both
    // so submission cannot race first-time initialization and its Wake is
    // guaranteed to reach this daemon rather than a replacement supervisor.
    wait_for(
        || {
            socket.exists()
                && atx()
                    .arg("--json")
                    .arg("--state-dir")
                    .arg(root.path().join("state"))
                    .arg("list")
                    .output()
                    .expect("probe list")
                    .status
                    .success()
        },
        "daemon to finish initializing",
    );

    let marker = root.path().join("state").join("daemon-ran.marker");
    let script = format!("'/usr/bin/touch' '{}'", marker.display());
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(root.path().join("state"))
        .args(["1s", "--", "/bin/sh", "-c", script.as_str()])
        .output()
        .expect("submit job");
    assert!(submitted.status.success(), "{submitted:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&submitted.stdout).expect("submission JSON");
    let job_id = value["data"]["job_id"].as_str().expect("job ID");

    wait_for(|| marker.exists(), "daemon to execute the due job");
    wait_for(
        || {
            let output = atx()
                .arg("--json")
                .arg("--state-dir")
                .arg(root.path().join("state"))
                .args(["show", job_id])
                .output()
                .expect("status");
            let value: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("status JSON");
            value["data"]["state"] == "succeeded"
        },
        "job to reach succeeded",
    );

    // An init replacement stops the unit with SIGTERM; with signals handled,
    // the daemon must exit cleanly instead of dying on the disposition.
    kill_process(
        Pid::from_raw(daemon.id().try_into().expect("pid")).expect("valid pid"),
        Signal::TERM,
    )
    .unwrap_or_else(|error| {
        panic!(
            "SIGTERM daemon ({error}, already exited: {:?}): {:?}",
            daemon.try_wait(),
            fs::read_to_string(root.path().join("daemon.err"))
        )
    });
    let status = daemon.wait().expect("reap daemon");
    assert!(
        status.success(),
        "daemon did not stop cleanly on SIGTERM: {status:?}"
    );
    // The stopped unit leaves no stale socket for the next start to trip on.
    assert!(!socket.exists(), "runtime socket survived daemon stop");
}

/// When the deadline fires but the store has been destroyed underneath it,
/// the supervisor must report the failure and keep running, never crash.
#[test]
fn service_managed_daemon_survives_a_broken_store_at_execution_time() {
    let root = tempdir().expect("root");
    let mut daemon = spawn_daemon(root.path());
    let socket = runtime_directory(root.path()).join("supervisor.sock");
    wait_for(
        || {
            socket.exists()
                && atx()
                    .arg("--json")
                    .arg("--state-dir")
                    .arg(root.path().join("state"))
                    .arg("list")
                    .output()
                    .expect("probe list")
                    .status
                    .success()
        },
        "daemon to finish initializing",
    );

    // Submit while the store is healthy so the Wake is acknowledged, then
    // destroy the store before the deadline fires. The live daemon keeps its
    // WAL sidecars open, so the path itself must become unopenable for the
    // next fresh connection to fail.
    let submitted = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(root.path().join("state"))
        .args(["2s", "--", "/bin/sh", "-c", "true"])
        .output()
        .expect("submit job");
    assert!(submitted.status.success(), "{submitted:?}");
    for entry in ["atx.db", "atx.db-wal", "atx.db-shm"] {
        match fs::remove_file(root.path().join("state").join(entry)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {entry}: {error}"),
        }
    }
    fs::create_dir(root.path().join("state").join("atx.db")).expect("block the store path");

    wait_for(
        || {
            fs::read_to_string(root.path().join("daemon.err"))
                .is_ok_and(|log| log.contains("atx supervisor:"))
        },
        "the daemon to report the execution failure",
    );

    kill_process(
        Pid::from_raw(daemon.id().try_into().expect("pid")).expect("valid pid"),
        Signal::TERM,
    )
    .expect("signal the daemon after the fault");
    let status = daemon.wait().expect("reap daemon");
    assert!(
        status.success(),
        "a broken store at execution time must not prevent a clean stop"
    );
}
