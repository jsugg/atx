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
mod support;

use support::tempdir;

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
fn spawn_daemon(root: &support::TestRoot) -> support::OwnedChild<'_> {
    // Point the daemon at not-yet-existing subdirectories: atx creates them
    // with its required 0700 mode, while a bare tempdir is world-readable.
    let stderr_log = fs::File::create(root.path().join("daemon.err")).expect("err log");
    root.spawn_owned(
        Command::new(env!("CARGO_BIN_EXE_atx"))
            .arg("__supervisor")
            .arg("--state-dir")
            .arg(root.path().join("state"))
            .arg("--runtime-dir")
            .arg(runtime_directory(root.path()))
            .arg("--service-managed")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_log),
    )
    .expect("start service-managed supervisor")
}

#[test]
fn service_fixture_reaps_its_direct_child_when_the_body_panics() {
    let pid = std::cell::Cell::new(None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = tempdir().expect("root");
        let mut daemon = spawn_daemon(&root);
        support::wait_for_service(
            &mut daemon,
            &root.path().join("state"),
            &runtime_directory(root.path()),
            &root.path().join("daemon.err"),
        )
        .expect("ready before failing body");
        pid.set(Some(i32::try_from(daemon.id()).expect("PID")));
        panic!("injected service test-body failure");
    }));
    let pid = pid.get().expect("started child");
    let remained = support::process_exists(pid).expect("owned child existence");
    assert!(result.is_err(), "injected body must fail");
    assert!(!remained, "fixture left service child PID {pid} unreaped");
}

#[test]
fn service_managed_daemon_stops_cleanly_on_sigint() {
    let root = tempdir().expect("root");
    let mut daemon = spawn_daemon(&root);
    support::wait_for_service(
        &mut daemon,
        &root.path().join("state"),
        &runtime_directory(root.path()),
        &root.path().join("daemon.err"),
    )
    .expect("service supervisor readiness");
    kill_process(
        Pid::from_raw(daemon.id().try_into().expect("PID")).expect("valid PID"),
        Signal::INT,
    )
    .expect("interrupt owned daemon");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = daemon.try_wait().expect("daemon status") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "SIGINT did not stop daemon"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "unclean SIGINT exit: {status}");
    assert!(
        !runtime_directory(root.path())
            .join("supervisor.sock")
            .exists()
    );
}

#[test]
fn stuck_readiness_probe_is_bounded_and_reaped_without_stopping_the_service() {
    let root = tempdir().expect("root");
    let mut daemon = spawn_daemon(&root);
    let state = root.path().join("state");
    let runtime = runtime_directory(root.path());
    let log = root.path().join("daemon.err");
    support::wait_for_service(&mut daemon, &state, &runtime, &log).expect("healthy service");
    let probe_pid = root.path().join("probe.pid");
    let started = std::time::Instant::now();
    let result = support::wait_for_service_with_probe_command(
        &mut daemon,
        &state,
        &runtime,
        &log,
        Duration::from_millis(200),
        |_| {
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "printf '%s' $$ > \"$1\"; printf probe-started >&2; exec /bin/sleep 2",
                    "held-probe",
                ])
                .arg(&probe_pid);
            command
        },
    );
    let elapsed = started.elapsed();
    let error = result.expect_err("held probe must time out");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    assert!(error.contains("deadline"), "{error}");
    assert!(
        error.contains("probe-started"),
        "probe stderr was lost: {error}"
    );
    let pid: i32 = fs::read_to_string(probe_pid)
        .expect("probe PID")
        .parse()
        .expect("PID");
    assert!(!support::process_exists(pid).expect("probe census"));
    assert!(daemon.try_wait().expect("service status").is_none());
}

#[test]
fn service_unwind_stops_its_supervisor_monitor_and_running_command() {
    let owned = std::cell::RefCell::new(Vec::new());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = tempdir().expect("root");
        let mut daemon = spawn_daemon(&root);
        support::wait_for_service(
            &mut daemon,
            &root.path().join("state"),
            &runtime_directory(root.path()),
            &root.path().join("daemon.err"),
        )
        .expect("service readiness");
        let started = root.path().join("started");
        let release = root.path().join("release");
        let submitted = atx()
            .arg("--state-dir")
            .arg(root.path().join("state"))
            .args([
                "1s",
                "--",
                "/bin/sh",
                "-c",
                "touch \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done",
                "held-command",
            ])
            .arg(&started)
            .arg(&release)
            .output()
            .expect("submit held command");
        assert!(submitted.status.success(), "{submitted:?}");
        wait_for(|| started.exists(), "service command barrier");
        owned.replace(support::fixture_process_ids(root.path()).expect("live owned processes"));
        assert!(
            owned.borrow().len() >= 3,
            "supervisor, monitor and command must be live"
        );
        panic!("injected body failure with a live service command");
    }));
    assert!(result.is_err());
    let owned = owned.into_inner();
    assert!(owned.len() >= 3);
    for pid in owned {
        assert!(
            !support::process_exists(pid).expect("owned process census"),
            "owned PID {pid} survived service unwind"
        );
    }
}

#[test]
fn failed_service_startup_keeps_diagnostics_and_reaps_during_unwind() {
    let pid = std::cell::Cell::new(None);
    let diagnostic = std::cell::RefCell::new(None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = tempdir().expect("root");
        fs::write(root.path().join("state"), "not a directory").expect("invalid state");
        let mut daemon = spawn_daemon(&root);
        pid.set(Some(i32::try_from(daemon.id()).expect("PID")));
        diagnostic.replace(Some(
            support::wait_for_service(
                &mut daemon,
                &root.path().join("state"),
                &runtime_directory(root.path()),
                &root.path().join("daemon.err"),
            )
            .expect_err("invalid state must fail startup"),
        ));
        panic!("injected assertion after failed startup");
    }));
    let diagnostic = diagnostic.into_inner().expect("startup diagnostic");
    assert!(
        diagnostic.contains("exited before readiness"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("path is not a real directory"),
        "{diagnostic}"
    );
    assert!(result.is_err());
    assert!(!support::process_exists(pid.get().expect("started child")).expect("child census"));
}

#[test]
fn service_managed_daemon_executes_due_jobs_and_stops_cleanly_on_sigterm() {
    let root = tempdir().expect("root");
    let mut daemon = spawn_daemon(&root);
    let socket = runtime_directory(root.path()).join("supervisor.sock");

    support::wait_for_service(
        &mut daemon,
        &root.path().join("state"),
        &runtime_directory(root.path()),
        &root.path().join("daemon.err"),
    )
    .expect("service supervisor readiness");

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
    let status = daemon.stop().expect("stop and reap daemon");
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
    let mut daemon = spawn_daemon(&root);
    support::wait_for_service(
        &mut daemon,
        &root.path().join("state"),
        &runtime_directory(root.path()),
        &root.path().join("daemon.err"),
    )
    .expect("service supervisor readiness");

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

    let status = daemon.stop().expect("stop and reap daemon after the fault");
    assert!(
        status.success(),
        "a broken store at execution time must not prevent a clean stop"
    );
}
