//! Per-run command monitor.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use thiserror::Error;

mod capture;
mod lifecycle;

pub(crate) use lifecycle::RunMonitor;

use crate::application::{ElapsedClock, SupervisorAcknowledger, WallClock};
use crate::domain::{JobId, JobState, RunId, RunState, Schedule, TransitionActor};
use crate::infrastructure::process::{NativeProcessInspector, NativeProcessRunner};
use crate::infrastructure::sqlite::{Database, JobStore};
use crate::infrastructure::time::NativeClock;
use crate::supervisor::SocketAcknowledger;

const DATABASE_TIMEOUT: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_MAX_LOG_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn run_monitor_process(
    state_directory: &Path,
    runtime_directory: &Path,
    job_id: &str,
    run_id: &str,
) -> Result<(), MonitorProcessError> {
    let job_id = JobId::from_str(job_id).map_err(|_| MonitorProcessError::InvalidIdentity)?;
    let run_id = RunId::from_str(run_id).map_err(|_| MonitorProcessError::InvalidIdentity)?;
    let mut store = JobStore::new(Database::open(
        &state_directory.join("atx.db"),
        DATABASE_TIMEOUT,
    )?);
    let job = store.load(job_id)?.ok_or(MonitorProcessError::MissingJob)?;
    let run = store
        .load_run(run_id)?
        .ok_or(MonitorProcessError::MissingRun)?;
    if run.job_id() != job.id() {
        return Err(MonitorProcessError::IdentityMismatch);
    }

    let clock = NativeClock;
    let inspector = NativeProcessInspector::new(clock.boot_identity()?);
    let runner = NativeProcessRunner::new(inspector.clone());
    // Operator-configured log caps reach the monitor only through the shared
    // config boundary; the fallback constant only guards a 16-bit platform.
    let config = crate::infrastructure::config::load_process_config(state_directory)
        .map_err(MonitorProcessError::Config)?;
    let max_log_bytes =
        usize::try_from(config.max_log_bytes_per_stream()).unwrap_or(DEFAULT_MAX_LOG_BYTES);
    let completed = RunMonitor::new(
        &mut store,
        runner,
        inspector,
        clock,
        state_directory,
        max_log_bytes,
    )
    .execute(&run, job.execution())?;
    finish_job(&mut store, &job, completed.state(), clock)?;

    if matches!(job.schedule(), Schedule::RecurringInterval { .. }) {
        let current = store
            .load(job.id())?
            .ok_or(MonitorProcessError::MissingJob)?;
        if current.state() == JobState::Waiting {
            let acknowledger =
                SocketAcknowledger::new(runtime_directory.join("supervisor.sock"), ACK_TIMEOUT);
            if let Err(error) = acknowledger.acknowledge(current.id(), current.revision()) {
                eprintln!("atx monitor: supervisor wake failed: {error}");
            }
        }
    }
    Ok(())
}

fn finish_job(
    store: &mut JobStore,
    original: &crate::domain::Job,
    run_state: RunState,
    clock: NativeClock,
) -> Result<(), MonitorProcessError> {
    let current = store
        .load(original.id())?
        .ok_or(MonitorProcessError::MissingJob)?;
    if current.state().is_terminal() {
        return Ok(());
    }
    let now = clock.now_utc()?;
    if current.state() == JobState::CancelRequested || run_state == RunState::Cancelled {
        store.transition_job(
            current.id(),
            current.revision(),
            JobState::Cancelled,
            false,
            TransitionActor::Monitor,
            "cancelled run finished",
            now,
        )?;
    } else if matches!(current.schedule(), Schedule::RecurringInterval { .. }) {
        store.advance_recurring_job(current.id(), current.revision(), now)?;
    } else {
        let (target, reason) = match run_state {
            RunState::Succeeded => (JobState::Succeeded, "command exited successfully"),
            RunState::Failed => (JobState::Failed, "command failed"),
            RunState::Interrupted => (JobState::Interrupted, "command outcome is unknown"),
            RunState::Cancelled => (JobState::Cancelled, "command was cancelled"),
            RunState::Starting | RunState::Running | RunState::CancelRequested => {
                return Err(MonitorProcessError::NonterminalRun);
            }
        };
        store.transition_job(
            current.id(),
            current.revision(),
            target,
            false,
            TransitionActor::Monitor,
            reason,
            now,
        )?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum MonitorProcessError {
    #[error("invalid job or run identifier")]
    InvalidIdentity,
    #[error(transparent)]
    Config(#[from] crate::infrastructure::config::ConfigError),
    #[error("claimed job was not found")]
    MissingJob,
    #[error("claimed run was not found")]
    MissingRun,
    #[error("run does not belong to the claimed job")]
    IdentityMismatch,
    #[error("run monitor returned a nonterminal state")]
    NonterminalRun,
    #[error(transparent)]
    Store(#[from] crate::infrastructure::sqlite::StoreError),
    #[error(transparent)]
    Process(#[from] crate::infrastructure::process::ProcessError),
    #[error(transparent)]
    Clock(#[from] crate::application::ClockError),
    #[error(transparent)]
    Monitor(#[from] lifecycle::MonitorError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{MonitorProcessError, finish_job, run_monitor_process};
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobState, MissedPolicy,
        RunState, RuntimeTier, Schedule, TransitionActor, UtcTimestamp,
    };
    use crate::infrastructure::sqlite::{Database, JobStore};
    use crate::infrastructure::time::NativeClock;

    fn shell_job(store: &mut JobStore, root: &std::path::Path, script: &str) -> crate::domain::Job {
        let now = UtcTimestamp::from_second(1_000).expect("now");
        let schedule = Schedule::one_shot_relative(
            DurationSeconds::new(30).expect("duration"),
            UtcTimestamp::from_second(1_030).expect("due"),
        );
        let execution = ExecutionSpec::new(
            ExecutionMode::Shell,
            vec![script.to_owned()],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution");
        let job = Job::new(
            now,
            schedule,
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("job");
        store.create(&job).expect("create job");
        // Mirror the supervisor: Scheduled -> Waiting -> Starting -> Running
        // before the monitor ever sees the job.
        let waiting = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "supervisor loaded deadline",
                now,
            )
            .expect("waiting transition");
        let starting = store
            .transition_job(
                waiting.id(),
                waiting.revision(),
                JobState::Starting,
                false,
                TransitionActor::Supervisor,
                "deadline became due",
                now,
            )
            .expect("start transition");
        let running = store
            .transition_job(
                starting.id(),
                starting.revision(),
                JobState::Running,
                false,
                TransitionActor::Supervisor,
                "run monitor claimed command",
                now,
            )
            .expect("run transition");
        let _ = root;
        running
    }

    fn private_state_root() -> tempfile::TempDir {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        root
    }

    #[test]
    fn monitor_process_rejects_malformed_and_mismatched_identities() {
        let root = private_state_root();
        let runtime = tempdir().expect("runtime");
        assert!(matches!(
            run_monitor_process(root.path(), runtime.path(), "not-a-job", "not-a-run"),
            Err(MonitorProcessError::InvalidIdentity)
        ));

        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let job = shell_job(&mut store, root.path(), "true");
        let other = shell_job(&mut store, root.path(), "true");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        assert!(matches!(
            run_monitor_process(
                root.path(),
                runtime.path(),
                &other.id().to_string(),
                &run.id().to_string()
            ),
            Err(MonitorProcessError::IdentityMismatch)
        ));
        assert!(matches!(
            run_monitor_process(
                root.path(),
                runtime.path(),
                &job.id().to_string(),
                "01aaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            Err(MonitorProcessError::MissingRun)
        ));
    }

    #[test]
    fn monitor_process_runs_command_and_finishes_one_shot_job() {
        let root = private_state_root();
        let runtime = tempdir().expect("runtime");
        let marker = root.path().join("marker");
        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let job = shell_job(
            &mut store,
            root.path(),
            &format!("'/usr/bin/touch' '{}'", marker.display()),
        );
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        run_monitor_process(
            root.path(),
            runtime.path(),
            &job.id().to_string(),
            &run.id().to_string(),
        )
        .expect("monitor process");

        let finished = store.load(job.id()).expect("reload").expect("job");
        assert_eq!(finished.state(), crate::domain::JobState::Succeeded);
        let completed = store.load_run(run.id()).expect("run").expect("run");
        assert_eq!(
            completed.outcome(),
            Some(&crate::domain::RunOutcome::Exit(0))
        );
        assert_eq!(completed.state(), crate::domain::RunState::Succeeded);
        assert!(marker.exists(), "command did not run");
    }

    #[test]
    fn monitor_process_is_idempotent_for_terminal_runs() {
        let root = private_state_root();
        let runtime = tempdir().expect("runtime");
        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let job = shell_job(&mut store, root.path(), "exit 3");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let identity = (job.id().to_string(), run.id().to_string());
        run_monitor_process(root.path(), runtime.path(), &identity.0, &identity.1)
            .expect("first monitor run");
        // A repeat monitor must be rejected, never re-execute the command.
        assert!(matches!(
            run_monitor_process(root.path(), runtime.path(), &identity.0, &identity.1),
            Err(MonitorProcessError::Monitor(
                super::lifecycle::MonitorError::AlreadyStarted
            ))
        ));

        let finished = store.load(job.id()).expect("reload").expect("job");
        assert_eq!(finished.state(), crate::domain::JobState::Failed);
        let completed = store.load_run(run.id()).expect("run").expect("run");
        assert_eq!(completed.state(), crate::domain::RunState::Failed);
    }

    fn recurring_shell_job(store: &mut JobStore, script: &str) -> crate::domain::Job {
        let now = UtcTimestamp::from_second(1_000).expect("now");
        let schedule = Schedule::RecurringInterval {
            interval: DurationSeconds::new(60).expect("duration"),
            persisted_anchor_utc: UtcTimestamp::from_second(1_030).expect("anchor"),
        };
        let execution = ExecutionSpec::new(
            ExecutionMode::Shell,
            vec![script.to_owned()],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution");
        let job = Job::new(
            now,
            schedule,
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("job");
        store.create(&job).expect("create job");
        let waiting = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                true,
                TransitionActor::Supervisor,
                "supervisor loaded deadline",
                now,
            )
            .expect("waiting transition");
        let starting = store
            .transition_job(
                waiting.id(),
                waiting.revision(),
                JobState::Starting,
                true,
                TransitionActor::Supervisor,
                "deadline became due",
                now,
            )
            .expect("start transition");
        store
            .transition_job(
                starting.id(),
                starting.revision(),
                JobState::Running,
                true,
                TransitionActor::Supervisor,
                "run monitor claimed command",
                now,
            )
            .expect("run transition");
        job
    }

    #[test]
    fn finish_job_leaves_already_terminal_jobs_untouched() {
        let root = private_state_root();
        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let mut job = shell_job(&mut store, root.path(), "true");
        let cancel_requested = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::CancelRequested,
                false,
                TransitionActor::Supervisor,
                "cancelling",
                UtcTimestamp::from_second(1_100).expect("timestamp"),
            )
            .expect("cancel requested");
        job = store
            .transition_job(
                cancel_requested.id(),
                cancel_requested.revision(),
                JobState::Cancelled,
                false,
                TransitionActor::Supervisor,
                "cancelled",
                UtcTimestamp::from_second(1_110).expect("timestamp"),
            )
            .expect("cancelled");

        finish_job(&mut store, &job, RunState::Succeeded, NativeClock)
            .expect("terminal job short-circuits");
        let unchanged = store.load(job.id()).expect("reload").expect("job");
        assert_eq!(unchanged.state(), JobState::Cancelled);
        assert_eq!(unchanged.revision(), job.revision());
    }

    #[test]
    fn finish_job_refuses_to_cancel_jobs_outside_cancel_requested() {
        let root = private_state_root();
        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let now = UtcTimestamp::from_second(1_000).expect("now");
        let execution = ExecutionSpec::new(
            ExecutionMode::Shell,
            vec!["true".to_owned()],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution");
        let job = Job::new(
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
        .expect("job");
        store.create(&job).expect("create job");
        // Only CancelRequested jobs may move to Cancelled, so a cancelled run
        // over any other live state must surface the domain rejection instead
        // of silently rewriting the lifecycle.
        let waiting = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "supervisor loaded deadline",
                now,
            )
            .expect("waiting transition");

        assert!(matches!(
            finish_job(&mut store, &waiting, RunState::Cancelled, NativeClock),
            Err(MonitorProcessError::Store(
                crate::infrastructure::sqlite::StoreError::Domain(_)
            ))
        ));
    }

    #[test]
    fn recurring_monitor_finishes_and_tolerates_a_missing_supervisor_socket() {
        let root = private_state_root();
        let runtime = tempdir().expect("runtime");
        let mut store = JobStore::new(
            Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
                .expect("database"),
        );
        let job = recurring_shell_job(&mut store, "true");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");

        // The runtime directory has no supervisor socket; the failed wake is
        // reported on stderr but must not fail the monitor process.
        run_monitor_process(
            root.path(),
            runtime.path(),
            &job.id().to_string(),
            &run.id().to_string(),
        )
        .expect("monitor process tolerates missing supervisor");

        let advanced = store.load(job.id()).expect("reload").expect("job");
        assert_eq!(advanced.state(), JobState::Waiting);
        let completed = store.load_run(run.id()).expect("run").expect("run");
        assert_eq!(
            completed.outcome(),
            Some(&crate::domain::RunOutcome::Exit(0))
        );
    }
}
