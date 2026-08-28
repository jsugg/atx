//! Regression coverage for integration-test process ownership.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use support::parse_nul_arguments;
use support::{
    CensusObservation, IdentityMatch, Inspection, ProcessIdentity, ProcessRow, StableEmpty,
    classify_identity, expand_owned_descendants, fixture_monitor_process_ids,
    fixture_process_count, fixture_process_ids, kill_session_supervisor, process_exists,
    process_is_terminal, retry_inspection, tempdir, tempdir_in,
};

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

fn submit(state: &Path, delay: &str, command: &[&str]) {
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
fn ownership_never_expands_from_a_stale_process_group_or_wrong_uid() {
    let effective_uid = 501;
    let rows = vec![
        ProcessRow::for_test(10, 1, 77, effective_uid),
        ProcessRow::for_test(20, 1, 77, effective_uid),
        ProcessRow::for_test(30, 10, 88, effective_uid + 1),
        ProcessRow::for_test(40, 10, 99, effective_uid),
    ];

    assert_eq!(
        expand_owned_descendants(&rows, [10].into_iter().collect(), effective_uid),
        [10, 40].into_iter().collect()
    );
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
    let script = format!(
        "'/usr/bin/touch' '{}' && '/bin/sleep' '1' && '/usr/bin/touch' '{}'",
        started.display(),
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
