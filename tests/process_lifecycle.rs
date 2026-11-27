//! Regression coverage for integration-test process ownership.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use support::parse_nul_arguments;
use support::{
    CensusObservation, IdentityMatch, Inspection, ProcessIdentity, ProcessRow, StableEmpty,
    classify_identity, expand_owned_descendants, fixture_monitor_process_ids,
    fixture_process_count, fixture_process_ids, kill_session_supervisor, parse_process_row,
    process_exists, process_is_terminal, retry_inspection, tempdir, tempdir_in,
};

#[test]
fn owned_child_guard_reaps_an_exited_child_and_preserves_another_fixture() {
    let root = support::tempdir().expect("root");
    let other = support::tempdir().expect("other root");
    let mut outsider = other
        .spawn_owned(
            Command::new("/bin/sh")
                .args(["-c", "read hold"])
                .stdin(Stdio::piped()),
        )
        .expect("held child");
    let pid;
    {
        let child = root
            .spawn_owned(Command::new("/bin/sh").args(["-c", "exit 9"]))
            .expect("exiting child");
        pid = i32::try_from(child.id()).expect("PID");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: zero is valid for siginfo storage, including the no-event result.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            // SAFETY: this guard owns an unreaped child; WNOWAIT observes without consuming it.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid.try_into().expect("child ID"),
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            assert_eq!(
                result,
                0,
                "observe child exit: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: initialized storage contains child-status information or all-zero no event.
            if unsafe { info.si_pid() } == pid {
                break;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    assert!(!support::process_exists(pid).expect("child census"));
    assert!(outsider.try_wait().expect("outsider status").is_none());
    let outsider_pid = i32::try_from(outsider.id()).expect("PID");
    drop(outsider);
    assert!(!support::process_exists(outsider_pid).expect("outsider census"));
}

#[test]
fn root_cleanup_cannot_reap_a_child_while_its_guard_is_live() {
    let root = support::tempdir().expect("root");
    let child = root
        .spawn_owned(
            Command::new("/bin/sh")
                .args(["-c", "read hold"])
                .stdin(Stdio::piped()),
        )
        .expect("held child");
    let error = root
        .stop_background()
        .expect_err("child guard owns reaping");
    assert!(error.contains("owned child guards"), "{error}");
    drop(child);
    root.stop_background().expect("cleanup after child reaped");
}

#[test]
fn failed_root_cleanup_preserves_diagnostics_and_runtime_evidence() {
    let root = tempdir().expect("root");
    let path = root.path().to_owned();
    let runtime = path.join("state/runtime");
    fs::create_dir_all(&runtime).expect("runtime");
    fs::write(runtime.join("supervisor.lock"), "malformed identity").expect("invalid lock");
    fs::write(path.join("daemon.err"), "useful failure evidence").expect("diagnostic");
    let result = catch_unwind(AssertUnwindSafe(|| drop(root)));
    let retained = path.exists();
    let evidence = fs::read_to_string(path.join("daemon.err"));
    if retained {
        fs::remove_dir_all(&path).expect("remove this process-free fault fixture");
    }
    assert!(
        result.is_err(),
        "malformed cleanup identity must remain an error"
    );
    assert!(
        retained,
        "failed cleanup deleted its retry/diagnostic evidence"
    );
    assert_eq!(
        evidence.expect("retained diagnostic"),
        "useful failure evidence"
    );
}

#[test]
fn owned_child_stop_forces_an_ignored_sigterm_and_is_idempotent() {
    let root = support::tempdir().expect("root");
    let ready = root.path().join("ready");
    let mut child = root
        .spawn_owned(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap '' TERM; printf ready > \"$1\"; read hold",
                    "held-child",
                ])
                .arg(&ready)
                .stdin(Stdio::piped()),
        )
        .expect("TERM-ignoring child");
    let pid = i32::try_from(child.id()).expect("PID");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not install TERM handler"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let started = Instant::now();
    let status = child.stop().expect("bounded force-stop");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(child.stop().expect("repeat stop"), status);
    drop(child);
    assert!(!support::process_exists(pid).expect("child census"));
}

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

#[test]
fn terminal_reaping_requires_matching_identity_user_and_parent() {
    let owner = rustix::process::geteuid().as_raw();
    let parent = i32::try_from(std::process::id()).expect("parent PID");
    let expected = ProcessIdentity::for_test("boot", 42, 100, 42);
    let row = ProcessRow::for_test(42, parent, 42, owner).with_state_for_test("Z");
    let cases = [
        (
            row.clone(),
            Inspection::Present(expected.clone()),
            true,
            true,
        ),
        (
            row.clone(),
            Inspection::Present(ProcessIdentity::for_test("boot", 42, 101, 42)),
            false,
            true,
        ),
        (row.clone(), Inspection::Absent, false, false),
        (row.clone(), Inspection::Transient, false, false),
        (
            ProcessRow::for_test(42, 0, 42, owner).with_state_for_test("Z"),
            Inspection::Present(expected.clone()),
            false,
            false,
        ),
        (
            ProcessRow::for_test(42, parent, 42, owner + 1).with_state_for_test("Z"),
            Inspection::Present(expected.clone()),
            false,
            true,
        ),
    ];
    for (row, current, should_reap, finished) in cases {
        let called = std::cell::Cell::new(false);
        let result = support::reap_terminal_with_identity(&expected, &row, current, |pid| {
            assert_eq!(pid, 42);
            called.set(true);
            Ok(true)
        })
        .expect("terminal disposition");
        assert_eq!(called.get(), should_reap);
        assert_eq!(result, finished);
    }
}

#[test]
fn bounded_probe_drains_both_pipes_and_preserves_a_failed_exit() {
    let output = support::bounded_probe_output(
        Command::new("/bin/sh").args(["-c", "i=0; while [ $i -lt 2000 ]; do printf 0123456789abcdef; printf fedcba9876543210 >&2; i=$((i+1)); done; exit 7"]),
        Instant::now() + Duration::from_secs(2),
    ).expect("bounded output");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"0123456789abcdef".repeat(2000));
    assert_eq!(output.stderr, b"fedcba9876543210".repeat(2000));
}

#[test]
fn concurrent_census_requests_share_one_fresh_capture() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let provider = Arc::new(support::CensusProvider::default());
    let barrier = Arc::new(Barrier::new(8));
    let captures = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    let result = provider.snapshot(Instant::now() + Duration::from_secs(5), || {
                        if captures.fetch_add(1, Ordering::SeqCst) == 0 {
                            let deadline = Instant::now() + Duration::from_secs(2);
                            while provider.waiter_count().expect("waiters") != 7 {
                                assert!(
                                    Instant::now() < deadline,
                                    "followers did not join capture"
                                );
                                std::thread::yield_now();
                            }
                        }
                        Ok(Vec::new())
                    });
                    assert!(result.expect("shared census").is_empty());
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker");
        }
    });
    let batched = captures.load(Ordering::SeqCst);
    assert!((1..=2).contains(&batched));
    provider
        .snapshot(Instant::now() + Duration::from_secs(2), || {
            captures.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
        .expect("next observation must be fresh");
    assert_eq!(captures.load(Ordering::SeqCst), batched + 1);
}

#[test]
fn census_requests_cannot_use_a_snapshot_started_before_their_request() {
    use std::sync::{Arc, mpsc};
    let provider = Arc::new(support::CensusProvider::default());
    let (started, ready) = mpsc::channel();
    let (release, permission) = mpsc::channel();
    let owner = Arc::clone(&provider);
    let worker = std::thread::spawn(move || {
        owner.snapshot(Instant::now() + Duration::from_secs(2), || {
            started.send(()).expect("old capture started");
            permission
                .recv_timeout(Duration::from_secs(2))
                .expect("release");
            Ok(vec![ProcessRow::for_test(42, 1, 42, 502)])
        })
    });
    ready
        .recv_timeout(Duration::from_secs(2))
        .expect("owner active");
    let watched = Arc::clone(&provider);
    let releaser = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while watched.waiter_count().expect("waiters") == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        release.send(()).expect("release old capture");
    });
    let result = provider
        .snapshot(Instant::now() + Duration::from_secs(2), || {
            Ok(vec![ProcessRow::for_test(43, 1, 43, 502)])
        })
        .expect("fresh subsequent capture");
    worker.join().expect("owner").expect("owner result");
    releaser.join().expect("releaser");
    assert_eq!(result, vec![ProcessRow::for_test(43, 1, 43, 502)]);
}

#[test]
fn census_follower_timeout_does_not_cancel_the_capture_owner() {
    use std::sync::{Arc, mpsc};
    let provider = Arc::new(support::CensusProvider::default());
    let (started, ready) = mpsc::channel();
    let (release, permission) = mpsc::channel();
    let owner = Arc::clone(&provider);
    let worker = std::thread::spawn(move || {
        owner.snapshot(Instant::now() + Duration::from_secs(2), || {
            started.send(()).expect("started");
            permission
                .recv_timeout(Duration::from_secs(2))
                .expect("release");
            Ok(Vec::new())
        })
    });
    ready
        .recv_timeout(Duration::from_secs(2))
        .expect("owner running");
    let result = provider.snapshot(Instant::now() + Duration::from_millis(5), || {
        panic!("a follower must not start another capture");
    });
    release.send(()).expect("release capture");
    assert!(
        worker
            .join()
            .expect("owner")
            .expect("owner result")
            .is_empty()
    );
    assert!(result.expect_err("follower timeout").contains("deadline"));
    assert_eq!(provider.waiter_count().expect("waiters"), 0);
}

#[test]
fn census_capture_failure_is_visible_and_does_not_leave_an_inflight_owner() {
    let provider = support::CensusProvider::default();
    let error = provider
        .snapshot(Instant::now() + Duration::from_secs(2), || {
            panic!("injected census capture failure");
        })
        .expect_err("capture panic must be visible");
    assert!(error.contains("capture panicked"), "{error}");
    assert_eq!(provider.waiter_count().expect("waiters"), 0);
    assert!(
        provider
            .snapshot(Instant::now() + Duration::from_secs(2), || Ok(Vec::new()))
            .expect("next fresh capture")
            .is_empty()
    );
}

#[test]
fn targeted_process_observers_reject_group_and_kernel_ids() {
    for pid in [0, -1] {
        assert!(support::process_exists(pid).is_err());
        assert!(support::process_is_terminal(pid).is_err());
    }
}

#[test]
fn process_census_uses_the_accounting_name_before_argv_validation() {
    for pid in [None, Some(i32::try_from(std::process::id()).expect("PID"))] {
        let command = support::process_census_command(pid).expect("census command");
        assert_eq!(command.get_program(), "/bin/ps");
        assert_eq!(
            command.get_args().last().expect("columns"),
            "pid=,ppid=,pgid=,uid=,state=,ucomm=",
            "the ownership prefilter must not read executable paths or process arguments"
        );
    }
    for pid in [0, -1] {
        assert!(support::process_census_command(Some(pid)).is_err());
    }
}

#[test]
fn terminal_identity_rechecks_a_process_that_exits_during_state_observation() {
    let expected = ProcessIdentity::for_test("boot", 42, 100, 42);
    let mut inspections = [Inspection::Present(expected.clone()), Inspection::Absent].into_iter();
    let result = support::terminal_identity_with_probes(
        &expected,
        || Ok(inspections.next().expect("two identity observations")),
        || Ok(false),
    )
    .expect("terminal observation");
    assert!(
        result,
        "a departed process was treated as live from an older state sample"
    );
}

#[test]
fn completed_probe_does_not_wait_for_another_process_holding_its_pipes() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{ChildStderr, ChildStdout};

    let root = tempdir().expect("root");
    let (stdout_reader, stdout_writer) = UnixStream::pair().expect("stdout pair");
    let (stderr_reader, stderr_writer) = UnixStream::pair().expect("stderr pair");
    let mut holder = root
        .spawn_owned(
            Command::new("/bin/sh")
                .args(["-c", "read hold"])
                .stdin(Stdio::piped())
                .stdout(Stdio::from(OwnedFd::from(
                    stdout_writer.try_clone().expect("holder stdout"),
                )))
                .stderr(Stdio::from(OwnedFd::from(
                    stderr_writer.try_clone().expect("holder stderr"),
                ))),
        )
        .expect("owned pipe holder");
    let mut producer = root
        .spawn_owned(
            Command::new("/bin/sh")
                .args(["-c", "printf producer-done; exit 7"])
                .stdout(Stdio::from(OwnedFd::from(stdout_writer)))
                .stderr(Stdio::from(OwnedFd::from(stderr_writer))),
        )
        .expect("owned producer");
    producer.stdout = Some(ChildStdout::from(OwnedFd::from(stdout_reader)));
    producer.stderr = Some(ChildStderr::from(OwnedFd::from(stderr_reader)));
    let started = Instant::now();
    let result = support::bounded_owned_probe_output(
        &mut producer,
        Instant::now() + Duration::from_millis(500),
    );
    let elapsed = started.elapsed();
    let holder_was_alive = holder.try_wait().expect("holder status").is_none();
    let holder_pid = i32::try_from(holder.id()).expect("holder PID");
    drop(holder);
    assert!(!process_exists(holder_pid).expect("reaped holder"));
    assert!(
        holder_was_alive,
        "the probe must not stop the other pipe holder"
    );
    let output = result.expect("completed producer must not wait for another writer's EOF");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"producer-done");
    assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
}

#[test]
fn process_census_capture_accepts_large_tables_but_bounds_a_held_ps() {
    let rows = support::process_table_with_command(
        Command::new("/bin/sh").args(["-c", "i=0; while [ $i -lt 3000 ]; do printf '42 1 42 502 S /bin/test-command\\n'; i=$((i+1)); done"]),
        Instant::now() + Duration::from_secs(2),
    ).expect("large bounded process table");
    assert_eq!(rows.len(), 3000);
    let started = Instant::now();
    let error = support::process_table_with_command(
        Command::new("/bin/sleep").arg("2"),
        Instant::now() + Duration::from_millis(100),
    )
    .expect_err("held census command");
    assert!(error.contains("deadline"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[test]
fn bounded_probe_reaps_either_stream_after_an_output_limit() {
    let root = tempdir().expect("root");
    for descriptor in ["1", "2"] {
        let pid_file = root.path().join(format!("writer-{descriptor}.pid"));
        let error = support::bounded_probe_output(
            Command::new("/bin/sh")
                .args(["-c", "printf '%s' $$ > \"$1\"; i=0; while [ $i -lt 9000 ]; do printf 0123456789abcdef >&\"$2\"; i=$((i+1)); done", "writer"])
                .arg(&pid_file).arg(descriptor),
            Instant::now() + Duration::from_secs(2),
        ).expect_err("oversized output");
        assert!(error.contains("output limit"), "{error}");
        assert!(error.len() < 20_000, "fault output must use bounded tails");
        let pid: i32 = fs::read_to_string(pid_file)
            .expect("writer PID")
            .parse()
            .expect("PID");
        assert!(!process_exists(pid).expect("writer census"));
    }
}

#[test]
fn expired_probe_deadline_does_not_spawn_a_child() {
    let root = tempdir().expect("root");
    let marker = root.path().join("unexpected-spawn");
    let error = support::bounded_probe_output(
        Command::new("/bin/sh")
            .args(["-c", "printf spawned > \"$1\"", "probe"])
            .arg(&marker),
        Instant::now(),
    )
    .expect_err("expired deadline");
    assert!(error.contains("deadline"), "{error}");
    assert!(!marker.exists());
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 64,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            "tests/fixtures/process-lifecycle.proptest-regressions",
        ))),
        ..proptest::test_runner::Config::default()
    })]
    #[test]
    fn diagnostic_tails_are_bounded_for_arbitrary_bytes(
        bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..32_768),
    ) {
        let log = tempfile::NamedTempFile::new().expect("log");
        fs::write(log.path(), &bytes).expect("diagnostic bytes");
        let tail = support::bounded_log_tail(log.path()).expect("tail");
        let expected = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(8192)..]);
        proptest::prop_assert!(tail.ends_with(expected.as_ref()));
        proptest::prop_assert!(tail.len() <= 8192 * 3 + 80);
        proptest::prop_assert_eq!(tail.starts_with("[truncated"), bytes.len() > 8192);
    }
}

#[test]
fn diagnostic_tail_reads_large_sparse_files_and_reports_missing_logs() {
    use std::io::{Seek, SeekFrom, Write};
    let mut log = tempfile::NamedTempFile::new().expect("log");
    log.as_file()
        .set_len(128 * 1024 * 1024)
        .expect("sparse diagnostic");
    log.seek(SeekFrom::End(-13)).expect("tail position");
    log.write_all(b"startup-fault").expect("tail");
    let tail = support::bounded_log_tail(log.path()).expect("bounded sparse tail");
    assert!(tail.ends_with("startup-fault"));
    assert!(tail.len() < 10_000);
    let missing = log.path().with_extension("missing");
    assert_eq!(
        support::bounded_log_tail(&missing)
            .expect_err("missing log")
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn diagnostic_tail_rejects_symlinks_directories_and_fifos() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().expect("root");
    let file = root.path().join("log");
    fs::write(&file, "private log").expect("log");
    let link = root.path().join("link");
    symlink(&file, &link).expect("symlink");
    assert!(support::bounded_log_tail(&link).is_err());
    assert!(support::bounded_log_tail(root.path()).is_err());
    let fifo = root.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
    // SAFETY: the owned path is NUL-terminated and `mkfifo` takes an integer mode.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    assert!(support::bounded_log_tail(&fifo).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
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

#[cfg(target_os = "linux")]
#[test]
fn linux_cmdline_read_classifies_exit_and_preserves_real_errors() {
    use std::io;
    use support::{Inspection, classify_linux_cmdline_read};

    let path = Path::new("/proc/42/cmdline");
    for code in [libc::ESRCH, libc::ENOENT] {
        assert_eq!(
            classify_linux_cmdline_read(path, Err(io::Error::from_raw_os_error(code))),
            Ok(Inspection::Absent)
        );
    }
    for code in [libc::EACCES, libc::EPERM] {
        assert_eq!(
            classify_linux_cmdline_read(path, Err(io::Error::from_raw_os_error(code))),
            Ok(Inspection::Transient)
        );
    }
    assert_eq!(
        classify_linux_cmdline_read(path, Ok(Vec::new())),
        Ok(Inspection::Transient)
    );
    assert_eq!(
        classify_linux_cmdline_read(path, Ok(b"atx\0\0job\0".to_vec())),
        Ok(Inspection::Present(
            vec!["atx", "", "job"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect()
        ))
    );
    for error in [
        io::Error::from_raw_os_error(libc::EIO),
        io::Error::other("read fault"),
    ] {
        let error =
            classify_linux_cmdline_read(path, Err(error)).expect_err("unexpected read failure");
        assert!(error.contains("/proc/42/cmdline"), "{error}");
    }
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

    assert!(process_is_terminal(first_pid).expect("first supervisor terminal"));
    first.stop_background().expect("reap first fixture");
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
    let mut child = root
        .spawn_owned(
            std::process::Command::new("/bin/sh")
                .args(["-c", "echo startup-fault >&2; exit 9"])
                .stderr(fs::File::create(&log).expect("log")),
        )
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
fn early_service_failure_retains_only_a_bounded_diagnostic_tail() {
    let root = tempdir().expect("root");
    let state = root.path().join("state");
    let log = root.path().join("daemon.err");
    fs::write(&log, "old startup noise\n".repeat(20_000)).expect("large log");
    let mut child = root
        .spawn_owned(
            Command::new("/bin/sh")
                .args(["-c", "printf useful-startup-fault >&2; exit 9"])
                .stderr(
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&log)
                        .expect("append log"),
                ),
        )
        .expect("failing child");
    let error = support::wait_for_service(&mut child, &state, &state.join("runtime"), &log)
        .expect_err("startup failure");
    assert!(
        error.contains("useful-startup-fault"),
        "tail lost the fault"
    );
    assert!(
        error.len() < 10_000,
        "diagnostic allocated {} bytes",
        error.len()
    );
    assert!(error.contains("truncated"), "truncation must be explicit");
    assert_eq!(
        child.try_wait().expect("status").expect("reaped").code(),
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
    let mut child = root
        .spawn_owned(
            std::process::Command::new("/bin/sh")
                .args(["-c", "read token"])
                .stdin(std::process::Stdio::piped())
                .stderr(fs::File::create(&log).expect("log")),
        )
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
        child.stop().expect("stop and reap held child");
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
        root.spawn_owned(
            atx()
                .arg("__supervisor")
                .arg("--state-dir")
                .arg(&state)
                .arg("--runtime-dir")
                .arg(state.join("runtime"))
                .arg("--service-managed")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log),
        )
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
