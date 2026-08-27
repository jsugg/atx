//! Run-monitor lifecycle.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;

use thiserror::Error;

use super::capture::capture_streams;
use crate::application::{ClockError, WallClock};
use crate::domain::{ExecutionSpec, Run, RunOutcome, RunState};
use crate::infrastructure::process::{NativeProcessInspector, NativeProcessRunner};
use crate::infrastructure::runtime::create_run_artifacts;
use crate::infrastructure::sqlite::JobStore;

pub(crate) struct RunMonitor<'a, Clock> {
    store: &'a mut JobStore,
    runner: NativeProcessRunner,
    inspector: NativeProcessInspector,
    clock: Clock,
    state_directory: &'a Path,
    max_log_bytes: usize,
}

impl<'a, Clock: WallClock> RunMonitor<'a, Clock> {
    pub(crate) const fn new(
        store: &'a mut JobStore,
        runner: NativeProcessRunner,
        inspector: NativeProcessInspector,
        clock: Clock,
        state_directory: &'a Path,
        max_log_bytes: usize,
    ) -> Self {
        Self {
            store,
            runner,
            inspector,
            clock,
            state_directory,
            max_log_bytes,
        }
    }

    pub(crate) fn execute(
        &mut self,
        run: &Run,
        execution: &ExecutionSpec,
    ) -> Result<Run, MonitorError> {
        let stored = self
            .store
            .load_run(run.id())
            .map_err(|error| MonitorError::Store(error.to_string()))?
            .ok_or(MonitorError::MissingRun)?;
        if stored.claim_token() != run.claim_token() {
            return Err(MonitorError::InvalidClaim);
        }
        if stored.state() != RunState::Starting || &stored != run {
            return Err(MonitorError::AlreadyStarted);
        }

        let artifacts = match create_run_artifacts(self.state_directory, run.id()) {
            Ok(artifacts) => artifacts,
            Err(error) => return self.fail_run(run, format!("log setup failed: {error}")),
        };
        let (stdout_path, stderr_path, stdout_file, stderr_file) = artifacts.into_parts();
        let monitor_identity = match self.inspector.inspect(std::process::id()) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return self.fail_run(run, "monitor identity disappeared".to_owned());
            }
            Err(error) => {
                return self.fail_run(run, format!("monitor identity failed: {error}"));
            }
        };
        let mut child = match self.runner.spawn(execution) {
            Ok(child) => child,
            Err(error) => return self.fail_run(run, format!("spawn failed: {error}")),
        };
        let (stdout, stderr) = match child.take_output() {
            Ok(streams) => streams,
            Err(error) => {
                child.terminate_and_wait();
                return self.fail_run(run, format!("output setup failed: {error}"));
            }
        };
        let started_at_utc = self.clock.now_utc()?;
        let stdout_path = stdout_path.to_string_lossy().into_owned();
        let stderr_path = stderr_path.to_string_lossy().into_owned();
        let running = match self.store.mark_run_running(
            run.id(),
            run.claim_token(),
            started_at_utc,
            monitor_identity,
            child.identity().clone(),
            &stdout_path,
            &stderr_path,
        ) {
            Ok(running) => running,
            Err(error) => {
                child.terminate_and_wait();
                return Err(MonitorError::Store(error.to_string()));
            }
        };

        let captured =
            capture_streams(stdout, stderr, stdout_file, stderr_file, self.max_log_bytes);
        let status = child.wait();
        let finished_at_utc = self.clock.now_utc()?;

        let (stdout_truncated, stderr_truncated, capture_failed, echo) = match captured {
            Ok(captured) => (
                captured.stdout.summary().truncated(),
                captured.stderr.summary().truncated(),
                captured.stdout.error().is_some() || captured.stderr.error().is_some(),
                Some(captured),
            ),
            Err(_) => (false, false, true, None),
        };
        self.store
            .record_log_truncation(
                running.id(),
                running.claim_token(),
                stdout_truncated,
                stderr_truncated,
            )
            .map_err(|error| MonitorError::Store(error.to_string()))?;

        let cancellation_requested = self
            .store
            .load_run(running.id())
            .map_err(|error| MonitorError::Store(error.to_string()))?
            .is_some_and(|run| run.state() == RunState::CancelRequested);
        let outcome = completion_outcome(status, capture_failed, cancellation_requested);
        let terminal = self.store.record_run_terminal(
            running.id(),
            running.claim_token(),
            finished_at_utc,
            outcome.clone(),
        );
        // Fire-and-forget echo to the submitting terminal: purely additive,
        // never affects the recorded outcome. Any failure (terminal closed,
        // rebooted, revoked) is silently skipped by design.
        if let (Some(echo), Some(tty)) = (echo, execution.notify_tty()) {
            echo_tty(
                tty,
                &self.state_directory.join(&stdout_path),
                &self.state_directory.join(&stderr_path),
                echo.stderr.summary().truncated() || echo.stdout.summary().truncated(),
            );
        }
        terminal.map_err(|error| MonitorError::Store(error.to_string()))
    }

    fn fail_run(&mut self, run: &Run, message: String) -> Result<Run, MonitorError> {
        let finished_at_utc = self.clock.now_utc()?;
        self.store
            .record_run_terminal(
                run.id(),
                run.claim_token(),
                finished_at_utc,
                RunOutcome::Failure(message),
            )
            .map_err(|error| MonitorError::Store(error.to_string()))
    }
}

/// Best-effort append of captured log files to a terminal device.
fn echo_tty(tty: &Path, stdout_log: &Path, stderr_log: &Path, truncated: bool) {
    use std::io::Write;
    let (Ok(stdout), Ok(stderr)) = (
        std::fs::read(stdout_log),
        std::fs::OpenOptions::new().append(true).open(tty),
    ) else {
        return;
    };
    let mut file = stderr;
    if truncated {
        let _ = writeln!(file, "[atx: output truncated at capture cap]");
    }
    let _ = writeln!(file);
    if !stdout.is_empty() {
        let _ = file.write_all(&stdout);
    }
    if let Ok(stderr) = std::fs::read(stderr_log) {
        if !stderr.is_empty() {
            let _ = file.write_all(&stderr);
        }
    }
    let _ = file.flush();
}

fn completion_outcome(
    status: std::io::Result<std::process::ExitStatus>,
    capture_failed: bool,
    cancellation_requested: bool,
) -> RunOutcome {
    if capture_failed {
        return RunOutcome::Failure("output capture failed".to_owned());
    }
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(0), _) => RunOutcome::Exit(0),
            _ if cancellation_requested => {
                RunOutcome::Cancelled("cancel request stopped command".to_owned())
            }
            (Some(code), _) => RunOutcome::Exit(code),
            (None, Some(signal)) => RunOutcome::Signal(signal),
            (None, None) => RunOutcome::Interrupted("unknown child status".to_owned()),
        },
        Err(error) => RunOutcome::Interrupted(format!("child wait failed: {error}")),
    }
}

#[derive(Debug, Error)]
pub(crate) enum MonitorError {
    #[error("claimed run was not found")]
    MissingRun,
    #[error("run claim token did not match")]
    InvalidClaim,
    #[error("run has already started")]
    AlreadyStarted,
    #[error("run storage failed: {0}")]
    Store(String),
    #[error(transparent)]
    Clock(#[from] ClockError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{MonitorError, RunMonitor};
    use crate::application::ElapsedClock;
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, MissedPolicy, RunOutcome,
        RunState, RuntimeTier, Schedule, UtcTimestamp,
    };
    use crate::infrastructure::process::{NativeProcessInspector, NativeProcessRunner};
    use rusqlite::params;

    use crate::infrastructure::sqlite::{Database, JobStore};
    use crate::infrastructure::time::NativeClock;

    fn job(execution: ExecutionSpec) -> Job {
        let now = UtcTimestamp::from_second(1_000).expect("now");
        Job::new(
            now,
            Schedule::one_shot_relative(
                DurationSeconds::new(30).expect("duration"),
                UtcTimestamp::from_second(1_030).expect("due"),
            ),
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("job")
    }

    fn execution(mode: ExecutionMode, arguments: &[&str]) -> ExecutionSpec {
        ExecutionSpec::new(
            mode,
            arguments.iter().map(ToString::to_string).collect(),
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin"), ("TOKEN", "swordfish")])
                .expect("environment"),
        )
        .expect("execution")
    }

    #[test]
    fn terminal_run_echoes_streams_to_notify_tty() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let tty = root.path().join("fake-tty");
        fs::write(&tty, b"").expect("seed tty file");
        let mut execution = execution(ExecutionMode::Shell, &["printf out; printf err >&2"]);
        execution
            .set_notify_tty(tty.clone())
            .expect("notify tty path");
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(completed.state(), RunState::Succeeded);
        let echoed = fs::read_to_string(&tty).expect("echoed output");
        assert!(echoed.contains("out"), "stdout missing: {echoed:?}");
        assert!(echoed.contains("err"), "stderr missing: {echoed:?}");
    }

    #[test]
    fn unwritable_notify_tty_leaves_outcome_untouched() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        // Path can never exist as a terminal: a directory cannot be appended.
        let mut execution = execution(ExecutionMode::Shell, &["true"]);
        execution
            .set_notify_tty(root.path().join("no-such-dir/tty"))
            .expect("notify tty path");
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(completed.state(), RunState::Succeeded);
        assert_eq!(completed.outcome(), Some(&RunOutcome::Exit(0)));
    }

    #[test]
    fn execute_rejects_substituted_claims_and_double_starts() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let execution = execution(ExecutionMode::Shell, &["true"]);
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let first = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let second = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(2_030).expect("scheduled"),
                UtcTimestamp::from_second(2_001).expect("created"),
            )
            .expect("claim");
        store
            .database()
            .connection()
            .execute(
                "UPDATE runs SET claim_token = ?1 WHERE id = ?2",
                params![vec![9_u8; 32], second.id().to_string()],
            )
            .expect("substitute token");

        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        assert!(matches!(
            monitor.execute(&second, &execution),
            Err(MonitorError::InvalidClaim)
        ));

        monitor.execute(&first, &execution).expect("first start");
        assert!(matches!(
            monitor.execute(&first, &execution),
            Err(MonitorError::AlreadyStarted)
        ));
    }

    #[test]
    fn truncated_output_appends_a_notice_to_the_tty() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let tty = root.path().join("fake-tty");
        fs::write(&tty, b"").expect("seed tty file");
        let mut execution = execution(
            ExecutionMode::Shell,
            &["i=0; while [ \"$i\" -lt 4096 ]; do printf x; i=$((i+1)); done"],
        );
        execution
            .set_notify_tty(tty.clone())
            .expect("notify tty path");
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(completed.state(), RunState::Succeeded);
        let echoed = fs::read_to_string(&tty).expect("echoed output");
        assert!(
            echoed.to_lowercase().contains("truncated"),
            "truncation notice missing: {echoed:?}"
        );
    }

    #[test]
    fn echo_tty_skips_unreadable_streams_without_touching_the_terminal() {
        let root = tempdir().expect("root");
        let tty = root.path().join("tty");
        fs::write(&tty, b"before").expect("seed tty");

        // Unreadable stdout log: nothing may be appended.
        super::echo_tty(
            &tty,
            &root.path().join("missing-stdout.log"),
            &root.path().join("also-missing.log"),
            false,
        );
        assert_eq!(fs::read_to_string(&tty).expect("tty"), "before");

        // Empty stdout, unreadable stderr log, no truncation: only the newline.
        let stdout_log = root.path().join("stdout.log");
        fs::write(&stdout_log, b"").expect("empty stdout");
        super::echo_tty(
            &tty,
            &stdout_log,
            &root.path().join("missing-stderr.log"),
            false,
        );
        assert_eq!(fs::read_to_string(&tty).expect("tty"), "before\n");
    }

    #[test]
    fn monitor_runs_direct_child_and_persists_output_summary() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let execution = execution(
            ExecutionMode::Shell,
            &["i=0; while [ \"$i\" -lt 4096 ]; do printf x; i=$((i+1)); done"],
        );
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(completed.state(), RunState::Succeeded);
        assert_eq!(completed.outcome(), Some(&RunOutcome::Exit(0)));
        assert!(matches!(
            monitor.execute(&run, &execution),
            Err(MonitorError::AlreadyStarted)
        ));
        let stdout_truncated: bool = store
            .database()
            .connection()
            .query_row(
                "SELECT stdout_truncated FROM runs WHERE id = ?1",
                [completed.id().to_string()],
                |row| row.get(0),
            )
            .expect("truncation flag");
        assert!(stdout_truncated);
    }

    #[test]
    fn spawn_and_nonzero_exit_failures_become_terminal_runs() {
        for (execution, expected) in [
            (
                execution(ExecutionMode::Direct, &["/definitely/missing/atx-command"]),
                None,
            ),
            (
                execution(ExecutionMode::Shell, &["exit 7"]),
                Some(RunOutcome::Exit(7)),
            ),
            (
                execution(ExecutionMode::Shell, &["kill -TERM $$"]),
                Some(RunOutcome::Signal(15)),
            ),
        ] {
            let root = tempdir().expect("root");
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
            let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database");
            let mut store = JobStore::new(database);
            let job = job(execution.clone());
            store.create(&job).expect("job");
            let run = store
                .claim_run(
                    job.id(),
                    UtcTimestamp::from_second(1_030).expect("scheduled"),
                    UtcTimestamp::from_second(1_001).expect("created"),
                )
                .expect("claim");
            let clock = NativeClock;
            let inspector =
                NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
            let runner = NativeProcessRunner::new(inspector.clone());
            let mut monitor =
                RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 1_024);
            let completed = monitor.execute(&run, &execution).expect("terminal result");
            assert_eq!(completed.state(), RunState::Failed);
            if let Some(expected) = expected {
                assert_eq!(completed.outcome(), Some(&expected));
            } else {
                assert!(matches!(completed.outcome(), Some(RunOutcome::Failure(_))));
                assert!(!format!("{:?}", completed.outcome()).contains("swordfish"));
            }
        }
    }

    #[test]
    fn a_starting_run_that_drifted_from_its_claim_is_rejected() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let execution = execution(ExecutionMode::Shell, &["true"]);
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        // Mutate a non-identity column so the stored run no longer equals the
        // claimed run while both remain Starting with matching claim tokens.
        store
            .database()
            .connection()
            .execute(
                "UPDATE runs SET created_at_utc = ?1 WHERE id = ?2",
                params![
                    UtcTimestamp::from_second(1_002)
                        .expect("drifted")
                        .to_string(),
                    run.id().to_string()
                ],
            )
            .expect("drift run row");

        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        assert!(matches!(
            monitor.execute(&run, &execution),
            Err(MonitorError::AlreadyStarted)
        ));
    }

    #[test]
    fn stderr_only_overflow_marks_the_echo_as_truncated() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let tty = root.path().join("fake-tty");
        fs::write(&tty, b"").expect("seed tty file");
        let mut execution = execution(
            ExecutionMode::Shell,
            &["i=0; while [ \"$i\" -lt 4096 ]; do printf x >&2; i=$((i+1)); done"],
        );
        execution
            .set_notify_tty(tty.clone())
            .expect("notify tty path");
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 128);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(completed.state(), RunState::Succeeded);
        let echoed = fs::read_to_string(&tty).expect("echoed output");
        assert!(
            echoed.to_lowercase().contains("truncated"),
            "truncation notice missing: {echoed:?}"
        );
    }

    #[test]
    fn cancel_requested_before_completion_is_recorded_as_cancelled() {
        use std::thread;
        use std::time::{Duration, Instant};

        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        // A long-sleeping child leaves a wide window to cancel mid-run.
        let execution = execution(ExecutionMode::Shell, &["/bin/sleep 20"]);
        let job = job(execution.clone());
        store.create(&job).expect("create");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");

        // Mirror a concurrent `atx cancel`: once the run is live, flip it to
        // CancelRequested and stop the child's process group. The monitor must
        // record the cancellation instead of the raw signal death.
        let canceller_store =
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("canceller database");
        thread::spawn(move || {
            let canceller = canceller_store.connection();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "run never reached the running state"
                );
                let started: i64 = canceller
                    .query_row(
                        "SELECT COUNT(*) FROM runs WHERE state = 'running'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("poll running state");
                if started > 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            canceller
                .execute(
                    "UPDATE runs SET state = 'cancel_requested' WHERE state = 'running'",
                    [],
                )
                .expect("mark cancel_requested");
            let process_group_id: i64 = canceller
                .query_row(
                    "SELECT process_group_id FROM runs WHERE state = 'cancel_requested'",
                    [],
                    |row| row.get(0),
                )
                .expect("read process group");
            // SAFETY: kill takes only scalar arguments.
            unsafe { libc::kill(i32::try_from(process_group_id).expect("pid"), libc::SIGTERM) };
        });

        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 1_024);
        let completed = monitor.execute(&run, &execution).expect("execute");

        assert_eq!(
            completed.outcome(),
            Some(&RunOutcome::Cancelled(
                "cancel request stopped command".to_owned()
            )),
            "the cancel request must win over the child's signal death"
        );
        assert_eq!(
            store
                .load_run(run.id())
                .expect("reload")
                .expect("run")
                .state(),
            RunState::Cancelled
        );
    }

    #[test]
    fn a_zero_capture_budget_fails_the_run_without_executing_twice() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let execution = execution(ExecutionMode::Shell, &["true"]);
        let job = job(execution.clone());
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let runner = NativeProcessRunner::new(inspector.clone());
        // A zero-byte budget makes stream capture impossible; the run must
        // finish as a failure instead of being recorded as a success.
        let mut monitor = RunMonitor::new(&mut store, runner, inspector, clock, root.path(), 0);
        let completed = monitor.execute(&run, &execution).expect("execute");
        assert_eq!(completed.state(), RunState::Failed);
        assert_eq!(
            completed.outcome(),
            Some(&RunOutcome::Failure("output capture failed".to_owned()))
        );
    }
}
