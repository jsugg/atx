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

        let (stdout_truncated, stderr_truncated, capture_failed) = match captured {
            Ok(captured) => (
                captured.stdout.summary().truncated(),
                captured.stderr.summary().truncated(),
                captured.stdout.error().is_some() || captured.stderr.error().is_some(),
            ),
            Err(_) => (false, false, true),
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
        self.store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished_at_utc,
                outcome,
            )
            .map_err(|error| MonitorError::Store(error.to_string()))
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
}
