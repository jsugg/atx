//! Regression coverage for integration-test process ownership.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::os::unix::process::CommandExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use support::parse_nul_arguments;
use support::{
    CensusObservation, IdentityMatch, Inspection, ProcessIdentity, ProcessRow, StableEmpty,
    classify_identity, expand_owned_descendants, fixture_monitor_process_ids,
    fixture_process_count, fixture_process_ids, kill_session_supervisor, parse_process_row,
    process_exists, process_is_terminal, retry_inspection, tempdir, tempdir_in,
};

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

fn submit(state: &Path, delay: &str, command: &[&str]) -> String {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .arg(delay)
        .arg("--")
        .args(command)
        .output()
        .expect("submit job");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("submission JSON");
    value["data"]["job_id"].as_str().expect("job ID").to_owned()
}

fn supervisor_pid(state: &Path) -> i32 {
    let lock = state.join("runtime/supervisor.lock");
    wait_for(|| lock.exists(), "supervisor lock");
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(&lock).expect("read supervisor lock"))
            .expect("supervisor identity");
    identity["pid"]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .expect("supervisor PID")
}

fn live_run_pids(state: &Path) -> Vec<i32> {
    let output = atx()
        .arg("--json")
        .arg("--state-dir")
        .arg(state)
        .arg("ps")
        .output()
        .expect("list processes");
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("process JSON");
    value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["pid"].as_i64().and_then(|pid| i32::try_from(pid).ok()))
        .collect()
}

fn assert_process_gone(pid: i32) {
    wait_for(
        || !process_exists(pid).expect("inspect process table"),
        &format!("process {pid} to exit"),
    );
}

fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn ownership_expands_only_through_same_user_parentage() {
    let effective_uid = 501;
    let rows = vec![
        ProcessRow::for_test(10, 1, 77, effective_uid),
        ProcessRow::for_test(20, 1, 77, effective_uid),
        ProcessRow::for_test(30, 10, 88, effective_uid + 1),
        ProcessRow::for_test(40, 10, 99, effective_uid),
        ProcessRow::for_test(50, 10, 99, effective_uid).with_state_for_test("Z"),
    ];

    assert_eq!(
        expand_owned_descendants(&rows, [10].into_iter().collect(), effective_uid),
        [10, 40, 50].into_iter().collect()
    );
}

#[test]
fn process_rows_accept_the_unsigned_dead_process_group_sentinel() {
    parse_process_row("5167 0 4294967295 0 X ps").expect("parse dead process row");
}

#[test]
fn signalling_requires_the_same_uid_boot_pid_and_start_token() {
    let expected = ProcessIdentity::for_test("boot-a", 42, 100, 42);
    assert_eq!(
        classify_identity(&expected, 501, Inspection::Present(expected.clone()), 501),
        IdentityMatch::Same
    );

    let replacement = ProcessIdentity::for_test("boot-a", 42, 101, 42);
    assert_eq!(
        classify_identity(&expected, 501, Inspection::Present(replacement), 501),
        IdentityMatch::Different
    );
    assert_eq!(
        classify_identity(&expected, 501, Inspection::Present(expected.clone()), 502),
        IdentityMatch::WrongUser
    );
}

#[test]
fn transient_inspection_is_bounded_and_cannot_produce_an_empty_census() {
    let mut attempts = 0;
    let inspected = retry_inspection(3, || {
        attempts += 1;
        Ok(if attempts < 3 {
            Inspection::Transient
        } else {
            Inspection::Present(7)
        })
    })
    .expect("retry inspection");
    assert_eq!(inspected, Inspection::Present(7));
    assert_eq!(attempts, 3);

    let mut stable = StableEmpty::default();
    assert!(!stable.observe(CensusObservation::Empty));
    assert!(!stable.observe(CensusObservation::Unknown));
    assert!(!stable.observe(CensusObservation::Empty));
    assert!(stable.observe(CensusObservation::Empty));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_cmdline_parser_preserves_interior_empty_arguments() {
    assert_eq!(
        parse_nul_arguments(b"atx\0__monitor\0\0--state-dir\0/tmp/state\0"),
        vec!["atx", "__monitor", "", "--state-dir", "/tmp/state"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>()
    );
}

#[test]
fn repeated_fixture_teardown_stops_session_supervisors() {
    for _ in 0..10 {
        let root = tempdir().expect("root");
        let fixture_path = root.path().to_owned();
        let state = root.path().join("state");
        submit(&state, "30m", &["/bin/true"]);
        let pid = supervisor_pid(&state);

        drop(root);

        assert_process_gone(pid);
        assert_eq!(
            fixture_process_count(&fixture_path).expect("fixture process census"),
            0
        );
    }
}

#[test]
fn hard_stop_is_scoped_to_one_fixture() {
    let first = tempdir().expect("first root");
    let first_state = first.path().join("state");
    submit(&first_state, "30m", &["/bin/true"]);
    let first_pid = supervisor_pid(&first_state);

    let second = tempdir().expect("second root");
    let second_state = second.path().join("state");
    submit(&second_state, "30m", &["/bin/true"]);
    let second_pid = supervisor_pid(&second_state);

    kill_session_supervisor(&first_state).expect("kill first supervisor");

    assert_process_gone(first_pid);
    assert!(
        process_exists(second_pid).expect("inspect second supervisor"),
        "second supervisor must stay alive"
    );
    drop(first);
    drop(second);
    assert_process_gone(second_pid);
}

#[test]
fn cleanup_preserves_argv_boundaries_for_whitespace_paths() {
    let parent = tempfile::tempdir_in("/tmp").expect("parent root");
    let spaced_parent = parent.path().join("path with spaces");
    fs::create_dir(&spaced_parent).expect("spaced parent");
    let root = tempdir_in(&spaced_parent).expect("root");
    let fixture_path = root.path().to_owned();
    let state = root.path().join("state");
    submit(&state, "30m", &["/bin/true"]);
    let pid = supervisor_pid(&state);

    drop(root);

    assert_process_gone(pid);
    assert_eq!(
        fixture_process_count(&fixture_path).expect("fixture process census"),
        0
    );
}

#[test]
fn teardown_covers_a_monitor_that_finishes_before_cleanup() {
    let root = tempdir().expect("root");
    let fixture_path = root.path().to_owned();
    let state = root.path().join("state");
    let started = root.path().join("started");
    let marker = root.path().join("finished");
    let release = root.path().join("release");
    let script = format!(
        "touch '{}' && while [ ! -f '{}' ]; do sleep 0.01; done && touch '{}'",
        started.display(),
        release.display(),
        marker.display()
    );
    submit(&state, "1s", &["/bin/sh", "-c", &script]);
    submit(&state, "30m", &["/bin/true"]);
    wait_for(|| started.exists(), "short job to start");

    let mut monitor_pids = Vec::new();
    wait_for(
        || {
            monitor_pids =
                fixture_monitor_process_ids(&fixture_path).expect("fixture monitor process census");
            monitor_pids.len() == 1
        },
        "short job monitor",
    );

    let mut owned_pids = Vec::new();
    wait_for(
        || {
            owned_pids = fixture_process_ids(&fixture_path).expect("fixture process census");
            owned_pids.len() >= 3
        },
        "supervisor, monitor, and command processes",
    );
    fs::write(&release, b"").expect("release command");
    wait_for(|| marker.exists(), "short job to finish");
    let monitor_pid = monitor_pids[0];
    wait_for(
        || process_is_terminal(monitor_pid).expect("inspect monitor state"),
        "short job monitor to become terminal",
    );

    drop(root);

    for pid in owned_pids {
        assert_process_gone(pid);
    }
    assert_eq!(
        fixture_process_count(&fixture_path).expect("fixture process census"),
        0
    );
}

#[test]
fn unwind_stops_supervisor_monitor_and_command_processes() {
    let mut owned_pids = Vec::new();
    let mut fixture_path = None;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let root = tempdir().expect("root");
        fixture_path = Some(root.path().to_owned());
        let state = root.path().join("state");
        submit(&state, "1s", &["/bin/sleep", "30"]);
        owned_pids.push(supervisor_pid(&state));
        wait_for(
            || {
                let pids = live_run_pids(&state);
                if pids.len() >= 2 {
                    owned_pids.extend(pids);
                    true
                } else {
                    false
                }
            },
            "monitor and command processes",
        );
        panic!("exercise fixture cleanup during unwind");
    }));

    assert!(result.is_err());
    owned_pids.sort_unstable();
    owned_pids.dedup();
    assert_eq!(owned_pids.len(), 3, "expected supervisor, monitor, command");
    for pid in owned_pids {
        assert_process_gone(pid);
    }
    assert_eq!(
        fixture_process_count(&fixture_path.expect("fixture path"))
            .expect("fixture process census"),
        0
    );
}

#[test]
fn service_readiness_requires_a_healthy_initialized_database() {
    let ready = serde_json::json!({
        "schema_version":1, "ok":true,
        "data":{"healthy":true, "schema_version":1},
    });
    assert!(support::service_report_is_ready(&ready));
    for database_schema in [
        serde_json::Value::Null,
        serde_json::json!(0),
        serde_json::json!("1"),
    ] {
        let mut report = ready.clone();
        report["data"]["schema_version"] = database_schema;
        assert!(!support::service_report_is_ready(&report));
    }
    let mut failed = ready.clone();
    failed["data"]["healthy"] = serde_json::json!(false);
    assert!(!support::service_report_is_ready(&failed));
    failed = ready;
    failed["ok"] = serde_json::json!(false);
    assert!(!support::service_report_is_ready(&failed));
    assert!(!support::service_report_is_ready(&serde_json::json!({})));
}

#[test]
fn service_readiness_reports_early_exit_and_reaps_the_child() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let log = root.path().join("daemon.err");
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "echo startup-fault >&2; exit 9"])
        .stderr(fs::File::create(&log).expect("log"))
        .spawn()
        .expect("failing service");
    let error = support::wait_for_service(&mut child, &state, &state.join("runtime"), &log)
        .expect_err("early service exit");
    assert!(error.contains("startup-fault"), "{error}");
    assert!(error.contains("exited before readiness"), "{error}");
    assert_eq!(
        child
            .try_wait()
            .expect("reaped child")
            .expect("exit")
            .code(),
        Some(9)
    );
}

#[test]
fn terminal_aging_rejects_live_and_unclaimed_fixtures_without_changes() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let job_id = submit(&state, "30m", &["/bin/true"]);
    assert!(support::age_stopped_terminal_job(&state, &job_id, 2).is_err());
    assert!(support::age_stopped_terminal_job(&state, &job_id, 0).is_err());
    root.stop_background().expect("stop fixture");
    let before = fs::read(state.join("atx.db")).expect("database");
    assert!(support::age_stopped_terminal_job(&state, &job_id, 2).is_err());
    assert_eq!(
        fs::read(state.join("atx.db")).expect("database after guard"),
        before
    );
}

#[test]
fn service_readiness_timeout_is_bounded_and_does_not_signal_the_child() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let log = root.path().join("daemon.err");
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "read token"])
        .stdin(std::process::Stdio::piped())
        .stderr(fs::File::create(&log).expect("log"))
        .spawn()
        .expect("held service");
    let started = Instant::now();
    let result = support::wait_for_service_with_timeout(
        &mut child,
        &state,
        &state.join("runtime"),
        &log,
        Duration::from_millis(1),
    );
    let elapsed = started.elapsed();
    let still_running = child.try_wait().expect("child status").is_none();
    if still_running {
        child.kill().expect("stop held child");
        child.wait().expect("reap held child");
    }
    assert!(still_running);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert!(
        result
            .expect_err("readiness timeout")
            .contains("readiness timed out")
    );
}

#[test]
fn terminal_aging_never_creates_a_missing_database() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    fs::create_dir(&state).expect("state directory");
    assert!(support::age_stopped_terminal_job(&state, "00000000000000000000000000", 2).is_err());
    assert!(!state.join("atx.db").exists());
}

fn assert_supervisor_reaps_monitors(service_managed: bool) {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let mut daemon = service_managed.then(|| {
        let log = fs::File::create(root.path().join("daemon.err")).expect("daemon log");
        atx()
            .arg("__supervisor")
            .arg("--state-dir")
            .arg(&state)
            .arg("--runtime-dir")
            .arg(state.join("runtime"))
            .arg("--service-managed")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start service supervisor")
    });
    if let Some(daemon) = daemon.as_mut() {
        support::wait_for_service(
            daemon,
            &state,
            &state.join("runtime"),
            &root.path().join("daemon.err"),
        )
        .expect("service supervisor readiness");
    }
    submit(&state, "30m", &["/bin/true"]);
    let supervisor = supervisor_pid(&state);

    let long_started = root.path().join("long-started");
    let long_release = root.path().join("long-release");
    let long_script = format!(
        "touch '{}' && while [ ! -f '{}' ]; do sleep 0.01; done",
        long_started.display(),
        long_release.display()
    );
    submit(&state, "1s", &["/bin/sh", "-c", &long_script]);
    wait_for(|| long_started.exists(), "older monitor to start");
    let long_monitors = fixture_monitor_process_ids(root.path()).expect("older monitor census");
    assert_eq!(long_monitors.len(), 1);
    let long_monitor = long_monitors[0];

    for iteration in 0..3 {
        let mut releases = Vec::new();
        for child in 0..3 {
            let started = root.path().join(format!("started-{iteration}-{child}"));
            let release = root.path().join(format!("release-{iteration}-{child}"));
            let script = format!(
                "touch '{}' && while [ ! -f '{}' ]; do sleep 0.01; done",
                started.display(),
                release.display()
            );
            submit(&state, "1s", &["/bin/sh", "-c", &script]);
            wait_for(|| started.exists(), "monitor to start its command");
            releases.push(release);
        }
        let monitors = fixture_monitor_process_ids(root.path()).expect("monitor census");
        assert_eq!(monitors.len(), 4, "{monitors:?}");
        for release in releases {
            fs::write(&release, b"").expect("release command");
        }
        for monitor in monitors.into_iter().filter(|pid| *pid != long_monitor) {
            assert_process_gone(monitor);
        }
        assert!(process_exists(long_monitor).expect("older monitor census"));
        assert!(process_exists(supervisor).expect("supervisor census"));
        assert_eq!(supervisor_pid(&state), supervisor);
    }
    fs::write(&long_release, b"").expect("release older command");
    assert_process_gone(long_monitor);
    if let Some(daemon) = daemon.as_mut() {
        assert!(daemon.try_wait().expect("daemon status").is_none());
    }
}

#[test]
fn session_supervisors_reap_monitors_while_future_work_remains() {
    assert_supervisor_reaps_monitors(false);
}

#[test]
fn service_supervisors_reap_monitors_without_restarting() {
    assert_supervisor_reaps_monitors(true);
}

#[test]
fn offline_aging_rejects_a_live_supervisor() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let job_id = submit(&state, "30m", &["/usr/bin/true"]);
    let error = support::age_stopped_one_shot(&state, &job_id).expect_err("live supervisor");
    assert_eq!(error, "cannot age a fixture while its supervisor is alive");
}

#[test]
fn offline_aging_rejects_a_claimed_job() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    submit(&state, "30m", &["/usr/bin/true"]);
    let marker = root.path().join("executed");
    let job_id = submit(
        &state,
        "1s",
        &["/usr/bin/touch", marker.to_str().expect("marker")],
    );
    wait_for(|| marker.exists(), "command to execute");
    kill_session_supervisor(&state).expect("stop supervisor");
    let error = support::age_stopped_one_shot(&state, &job_id).expect_err("claimed job");
    assert!(
        error.starts_with("offline fixture requires an unclaimed job:"),
        "{error}"
    );
}

fn block_probe_signal(command: &mut Command) {
    // SAFETY: only async-signal-safe signal operations use stack pointers after fork.
    unsafe {
        command.pre_exec(|| {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            if libc::sigemptyset(&raw mut mask) == -1
                || libc::sigaddset(&raw mut mask, libc::SIGUSR1) == -1
                || libc::sigprocmask(libc::SIG_BLOCK, &raw const mask, std::ptr::null_mut()) == -1
            {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[test]
fn monitors_restore_the_inherited_signal_mask_for_commands() {
    let root = tempdir().expect("root");
    let source = root.path().join("mask.c");
    let probe = root.path().join("mask");
    fs::write(
        &source,
        r#"
#define _POSIX_C_SOURCE 200809L
#include <signal.h>
#include <stdio.h>
int main(int argc, char **argv) {
    sigset_t mask;
    if (argc != 2 || sigprocmask(SIG_SETMASK, NULL, &mask) != 0) return 2;
    FILE *file = fopen(argv[1], "w");
    if (file == NULL) return 3;
    int written = fprintf(file, "%d %d %d %d\n", sigismember(&mask, SIGCHLD),
        sigismember(&mask, SIGTERM), sigismember(&mask, SIGINT), sigismember(&mask, SIGUSR1));
    int closed = fclose(file);
    return written < 0 || closed != 0;
}
"#,
    )
    .expect("write signal probe");
    let compiled = Command::new("cc")
        .arg(&source)
        .arg("-o")
        .arg(&probe)
        .output()
        .expect("compile signal probe");
    assert!(compiled.status.success(), "{compiled:?}");
    let baseline_path = root.path().join("baseline");
    let mut baseline = Command::new(&probe);
    baseline.arg(&baseline_path);
    block_probe_signal(&mut baseline);
    assert!(baseline.status().expect("baseline probe").success());
    let expected = fs::read_to_string(&baseline_path).expect("baseline mask");

    let state = root.path().join("state");
    let result_path = root.path().join("scheduled");
    let mut submitted = atx();
    submitted
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .args(["1s", "--missed", "run-latest", "--"])
        .arg(&probe)
        .arg(&result_path);
    block_probe_signal(&mut submitted);
    let output = submitted.output().expect("submit signal probe");
    assert!(output.status.success(), "{output:?}");
    wait_for(
        || fs::read_to_string(&result_path).is_ok_and(|value| value.ends_with('\n')),
        "scheduled signal probe",
    );
    assert_eq!(
        fs::read_to_string(&result_path).expect("scheduled mask"),
        expected
    );
}
