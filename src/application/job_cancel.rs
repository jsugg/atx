//! Job cancellation through application ports.

use std::time::{Duration, Instant};

use thiserror::Error;

use super::cancel::{
    CancelRunError, CancelRunResult, CancellationStore, GroupCancellation,
    ProcessCancellationError, ProcessGroupCanceller, cancel_claimed_run,
};
use super::clock::{ClockError, WallClock};
use crate::domain::{
    ClaimToken, DurationSeconds, Job, JobId, JobState, Revision, Run, RunId, RunState, UtcTimestamp,
};

// Bound contention with a monitor or another canceller without retrying forever.
const MAX_REQUEST_ATTEMPTS: usize = 8;
const COMPLETION_PADDING: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) trait JobCancellationStore {
    fn load_job_for_cancellation(
        &self,
        id: JobId,
    ) -> Result<Option<Job>, JobCancellationStoreError>;
    fn latest_active_run_for_cancellation(
        &self,
        id: JobId,
    ) -> Result<Option<Run>, JobCancellationStoreError>;
    fn transition_cancellation(
        &mut self,
        id: JobId,
        revision: Revision,
        state: JobState,
        reason: &'static str,
        now: UtcTimestamp,
    ) -> Result<Job, JobCancellationStoreError>;
    fn finish_cancelled_run(
        &mut self,
        id: RunId,
        token: ClaimToken,
        now: UtcTimestamp,
    ) -> Result<(), JobCancellationStoreError>;
    fn load_run_for_cancellation(
        &self,
        id: RunId,
    ) -> Result<Option<Run>, JobCancellationStoreError>;
}

pub(crate) trait JobProcessCanceller {
    fn cancel_group(
        &self,
        run: &Run,
        grace: Duration,
    ) -> Result<GroupCancellation, ProcessCancellationError>;
}

/// Monotonic wait semantics match the native process-grace clock.
pub(crate) trait CancellationWait {
    fn now(&self) -> Instant;
    fn wait(&self, duration: Duration);
}

/// A best-effort notice must not prevent durable cancellation or signalling.
pub(crate) trait CancellationProgress {
    fn waiting(&self, maximum: Duration);
}

impl CancellationProgress for () {
    fn waiting(&self, _maximum: Duration) {}
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum JobCancellationStoreError {
    #[error("job revision changed")]
    Conflict,
    #[error("domain operation failed: {0}")]
    Rejected(String),
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum CancelJobError {
    #[error("job was not found")]
    NotFound,
    #[error("{0}")]
    StateConflict(&'static str),
    #[error(transparent)]
    Store(#[from] JobCancellationStoreError),
    #[error(transparent)]
    Run(#[from] CancelRunError),
    #[error(transparent)]
    Clock(#[from] ClockError),
}

struct GraceBound<'a, Processes> {
    processes: &'a Processes,
    grace: Duration,
}

enum RequestResult {
    Acquired(Job),
    Satisfied(Job),
}

impl<Processes: JobProcessCanceller> ProcessGroupCanceller for GraceBound<'_, Processes> {
    fn cancel_group(&self, run: &Run) -> Result<GroupCancellation, ProcessCancellationError> {
        self.processes.cancel_group(run, self.grace)
    }
}

/// Cancel a resolved job, keeping native work and progress behind application ports.
///
/// The store must reject new claims after a committed cancellation request.
/// Native termination may wait one grace interval; the terminal-record wait
/// retains the existing grace-plus-two-second interval after that phase.
pub(crate) fn cancel_job<Store, Runtime, Progress>(
    store: &mut Store,
    job: Job,
    grace: DurationSeconds,
    runtime: &Runtime,
    progress: &Progress,
) -> Result<Job, CancelJobError>
where
    Store: JobCancellationStore + CancellationStore,
    Runtime: WallClock + JobProcessCanceller + CancellationWait,
    Progress: CancellationProgress,
{
    if matches!(job.state(), JobState::Cancelled | JobState::CancelRequested) {
        return Ok(job);
    }
    if job.state().is_terminal() {
        return Err(CancelJobError::StateConflict(
            "completed job cannot be cancelled",
        ));
    }
    let requested = match request_job(store, job, runtime)? {
        RequestResult::Acquired(job) => job,
        RequestResult::Satisfied(job) => return Ok(job),
    };
    let active = store.latest_active_run_for_cancellation(requested.id())?;
    let Some(run) = active else {
        return store
            .transition_cancellation(
                requested.id(),
                requested.revision(),
                JobState::Cancelled,
                "cancelled before command start",
                runtime.now_utc()?,
            )
            .map_err(CancelJobError::from);
    };
    let run = cancel_run(store, &run, grace, runtime, progress)?;
    finish_job(store, requested.id(), run.state(), runtime)
}

fn request_job<Store: JobCancellationStore, Clock: WallClock>(
    store: &mut Store,
    mut current: Job,
    clock: &Clock,
) -> Result<RequestResult, CancelJobError> {
    for _ in 0..MAX_REQUEST_ATTEMPTS {
        if matches!(
            current.state(),
            JobState::Cancelled | JobState::CancelRequested
        ) {
            return Ok(RequestResult::Satisfied(current));
        }
        if current.state().is_terminal() {
            break;
        }
        match store.transition_cancellation(
            current.id(),
            current.revision(),
            JobState::CancelRequested,
            "cancel requested",
            clock.now_utc()?,
        ) {
            Ok(requested) => return Ok(RequestResult::Acquired(requested)),
            Err(JobCancellationStoreError::Conflict | JobCancellationStoreError::Rejected(_)) => {
                current = store
                    .load_job_for_cancellation(current.id())?
                    .ok_or(CancelJobError::NotFound)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(CancelJobError::StateConflict(
        "job changed state during cancellation",
    ))
}

fn cancel_run<Store, Runtime, Progress>(
    store: &mut Store,
    run: &Run,
    grace: DurationSeconds,
    runtime: &Runtime,
    progress: &Progress,
) -> Result<Run, CancelJobError>
where
    Store: JobCancellationStore + CancellationStore,
    Runtime: WallClock + JobProcessCanceller + CancellationWait,
    Progress: CancellationProgress,
{
    let grace = Duration::from_secs(grace.get());
    let completion_wait = grace
        .checked_add(COMPLETION_PADDING)
        .ok_or(CancelJobError::StateConflict("cancel wait overflowed"))?;
    let maximum = grace
        .checked_add(completion_wait)
        .ok_or(CancelJobError::StateConflict("cancel wait overflowed"))?;
    progress.waiting(maximum);
    let result = cancel_claimed_run(
        store,
        &GraceBound {
            processes: runtime,
            grace,
        },
        run.id(),
        run.claim_token(),
    )?;
    if let CancelRunResult::CommittedBeforeSpawn(run) = result {
        store.finish_cancelled_run(run.id(), run.claim_token(), runtime.now_utc()?)?;
    }
    let deadline = runtime
        .now()
        .checked_add(completion_wait)
        .ok_or(CancelJobError::StateConflict("cancel wait overflowed"))?;
    loop {
        let current = store
            .load_run_for_cancellation(run.id())?
            .ok_or(CancelJobError::StateConflict("active run disappeared"))?;
        if current.state().is_terminal() {
            return Ok(current);
        }
        let now = runtime.now();
        if now >= deadline {
            return Err(CancelJobError::StateConflict(
                "run did not stop before the cancellation deadline",
            ));
        }
        runtime.wait(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn finish_job<Store: JobCancellationStore, Clock: WallClock>(
    store: &mut Store,
    id: JobId,
    state: RunState,
    clock: &Clock,
) -> Result<Job, CancelJobError> {
    let target = match state {
        RunState::Succeeded => JobState::Succeeded,
        RunState::Cancelled => JobState::Cancelled,
        RunState::Failed => JobState::Failed,
        RunState::Interrupted => JobState::Interrupted,
        _ => {
            return Err(CancelJobError::StateConflict(
                "run remained nonterminal after cancellation",
            ));
        }
    };
    let current = store
        .load_job_for_cancellation(id)?
        .ok_or(CancelJobError::NotFound)?;
    if current.state().is_terminal() {
        return Ok(current);
    }
    match store.transition_cancellation(
        current.id(),
        current.revision(),
        target,
        "cancellation finished",
        clock.now_utc()?,
    ) {
        Ok(finished) => Ok(finished),
        Err(JobCancellationStoreError::Conflict | JobCancellationStoreError::Rejected(_)) => {
            let final_job = store
                .load_job_for_cancellation(id)?
                .ok_or(CancelJobError::NotFound)?;
            if final_job.state().is_terminal() || final_job.state() == JobState::Waiting {
                Ok(final_job)
            } else {
                Err(CancelJobError::StateConflict(
                    "job changed state during cancellation",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::RefCell;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::{
        CancelJobError, CancellationProgress, CancellationWait, JobCancellationStore,
        JobCancellationStoreError, JobProcessCanceller, cancel_job,
    };
    use crate::application::{
        CancellationStore, CancellationStoreError, ClockError, GroupCancellation,
        ProcessCancellationError, WallClock,
    };
    use crate::domain::{
        ClaimToken, DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobId,
        JobState, MissedPolicy, ProcessIdentitySnapshot, Revision, Run, RunId, RunOutcome,
        RunState, RuntimeTier, Schedule, Sequence, TransitionActor, UtcTimestamp,
    };

    fn timestamp(second: i64) -> UtcTimestamp {
        UtcTimestamp::from_second(second).expect("timestamp")
    }

    fn fixture_job() -> Job {
        Job::new(
            timestamp(1_000),
            Schedule::one_shot_relative(
                DurationSeconds::new(30).expect("duration"),
                timestamp(1_030),
            ),
            MissedPolicy::Hold,
            RuntimeTier::Session,
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["true".to_owned()],
                "/".to_owned(),
                Environment::from_pairs([("PATH", "/bin")]).expect("environment"),
            )
            .expect("execution"),
            501,
        )
        .expect("job")
    }

    struct Store {
        job: Job,
        run: RefCell<Option<Run>>,
        terminal_at_cancel: Option<Run>,
        conflicts: usize,
        request_attempts: usize,
        peer_request_on_conflict: bool,
        request_failure: Option<JobCancellationStoreError>,
        completion_failure: Option<JobCancellationStoreError>,
        missing_job: bool,
        missing_run: bool,
        poll_failure: Option<JobCancellationStoreError>,
    }

    impl Store {
        fn new() -> Self {
            Self {
                job: fixture_job(),
                run: RefCell::new(None),
                terminal_at_cancel: None,
                conflicts: 0,
                request_attempts: 0,
                peer_request_on_conflict: false,
                request_failure: None,
                completion_failure: None,
                missing_job: false,
                missing_run: false,
                poll_failure: None,
            }
        }

        fn with_run(running: bool) -> Self {
            let mut store = Self::new();
            for (state, second) in [(JobState::Waiting, 1_000), (JobState::Starting, 1_001)] {
                store
                    .job
                    .transition(
                        state,
                        false,
                        TransitionActor::Supervisor,
                        "fixture claim",
                        timestamp(second),
                    )
                    .expect("job transition");
            }
            let mut run = Run::new(
                store.job.id(),
                Sequence::new(1).expect("sequence"),
                timestamp(1_030),
                timestamp(1_001),
                ClaimToken::from_bytes([7; 32]),
            );
            if running {
                store
                    .job
                    .transition(
                        JobState::Running,
                        false,
                        TransitionActor::Supervisor,
                        "fixture running",
                        timestamp(1_002),
                    )
                    .expect("running job");
                let identity = |pid, group| ProcessIdentitySnapshot {
                    boot_identity: "fixture-boot".to_owned(),
                    pid,
                    start_token: u64::from(pid),
                    process_group_id: group,
                };
                run = run
                    .mark_running(
                        timestamp(1_002),
                        identity(10, 10),
                        identity(11, 11),
                        "runs/out.log".to_owned(),
                        "runs/err.log".to_owned(),
                    )
                    .expect("running run");
            }
            *store.run.borrow_mut() = Some(run);
            store
        }
    }

    impl JobCancellationStore for Store {
        fn load_job_for_cancellation(
            &self,
            id: JobId,
        ) -> Result<Option<Job>, JobCancellationStoreError> {
            Ok((!self.missing_job && id == self.job.id()).then(|| self.job.clone()))
        }

        fn latest_active_run_for_cancellation(
            &self,
            id: JobId,
        ) -> Result<Option<Run>, JobCancellationStoreError> {
            assert_eq!(id, self.job.id());
            assert_eq!(
                self.job.state(),
                JobState::CancelRequested,
                "run lookup must follow the winning request"
            );
            Ok(self.run.borrow().clone())
        }

        fn transition_cancellation(
            &mut self,
            id: JobId,
            revision: Revision,
            state: JobState,
            reason: &'static str,
            now: UtcTimestamp,
        ) -> Result<Job, JobCancellationStoreError> {
            assert_eq!(id, self.job.id());
            if state != JobState::CancelRequested {
                if let Some(error) = &self.completion_failure {
                    return Err(error.clone());
                }
            }
            if state == JobState::CancelRequested {
                self.request_attempts += 1;
                if let Some(error) = &self.request_failure {
                    return Err(error.clone());
                }
                if self.conflicts > 0 {
                    self.conflicts -= 1;
                    if self.peer_request_on_conflict {
                        self.job
                            .transition(
                                JobState::CancelRequested,
                                false,
                                TransitionActor::Cli,
                                "peer cancel",
                                now,
                            )
                            .expect("peer request");
                    }
                    return Err(JobCancellationStoreError::Conflict);
                }
            }
            if revision != self.job.revision() {
                return Err(JobCancellationStoreError::Conflict);
            }
            self.job
                .transition(state, false, TransitionActor::Cli, reason, now)
                .map_err(|error| JobCancellationStoreError::Rejected(error.to_string()))?;
            Ok(self.job.clone())
        }

        fn finish_cancelled_run(
            &mut self,
            id: RunId,
            token: ClaimToken,
            now: UtcTimestamp,
        ) -> Result<(), JobCancellationStoreError> {
            let run = self.run.borrow().clone().expect("claimed run");
            assert_eq!(id, run.id());
            assert_eq!(token, run.claim_token());
            *self.run.borrow_mut() = Some(
                run.with_outcome(
                    now,
                    RunOutcome::Cancelled("cancelled before command start".to_owned()),
                )
                .expect("finish run"),
            );
            Ok(())
        }

        fn load_run_for_cancellation(
            &self,
            id: RunId,
        ) -> Result<Option<Run>, JobCancellationStoreError> {
            if let Some(error) = &self.poll_failure {
                return Err(error.clone());
            }
            if self.missing_run {
                return Ok(None);
            }
            let run = self
                .terminal_at_cancel
                .clone()
                .or_else(|| self.run.borrow().clone());
            assert_eq!(run.as_ref().map(Run::id), Some(id));
            Ok(run)
        }
    }

    impl CancellationStore for Store {
        fn load_for_cancellation(&self, id: RunId) -> Result<Option<Run>, CancellationStoreError> {
            let run = self
                .terminal_at_cancel
                .clone()
                .or_else(|| self.run.borrow().clone());
            assert_eq!(run.as_ref().map(Run::id), Some(id));
            Ok(run)
        }

        fn commit_cancellation(
            &mut self,
            id: RunId,
            token: ClaimToken,
        ) -> Result<Run, CancellationStoreError> {
            let run = self.run.borrow().clone().expect("claimed run");
            assert_eq!(id, run.id());
            assert_eq!(token, run.claim_token());
            let requested = run.request_cancellation().expect("run request");
            *self.run.borrow_mut() = Some(requested.clone());
            Ok(requested)
        }
    }

    struct Runtime {
        origin: Instant,
        now: Mutex<Instant>,
        signals: AtomicUsize,
        clock_failure: bool,
        process_failure: bool,
    }

    impl Runtime {
        fn new() -> Self {
            let origin = Instant::now();
            Self {
                origin,
                now: Mutex::new(origin),
                signals: AtomicUsize::new(0),
                clock_failure: false,
                process_failure: false,
            }
        }
    }

    impl WallClock for Runtime {
        fn now_utc(&self) -> Result<UtcTimestamp, ClockError> {
            if self.clock_failure {
                Err(ClockError::Unavailable)
            } else {
                Ok(timestamp(10_000))
            }
        }
    }

    impl CancellationWait for Runtime {
        fn now(&self) -> Instant {
            *self.now.lock().expect("clock lock")
        }
        fn wait(&self, duration: Duration) {
            let mut now = self.now.lock().expect("clock lock");
            *now = now.checked_add(duration).expect("clock advance");
        }
    }

    impl JobProcessCanceller for Runtime {
        fn cancel_group(
            &self,
            run: &Run,
            grace: Duration,
        ) -> Result<GroupCancellation, ProcessCancellationError> {
            assert_eq!(run.state(), RunState::CancelRequested);
            self.signals.fetch_add(1, Ordering::Relaxed);
            if self.process_failure {
                return Err(ProcessCancellationError(
                    "fixture process failure".to_owned(),
                ));
            }
            self.wait(grace);
            Ok(GroupCancellation::KilledAfterGrace)
        }
    }

    #[derive(Default)]
    struct Progress(Mutex<Vec<Duration>>);

    impl CancellationProgress for Progress {
        fn waiting(&self, maximum: Duration) {
            self.0.lock().expect("progress lock").push(maximum);
        }
    }

    fn grace() -> DurationSeconds {
        DurationSeconds::new(1).expect("grace")
    }

    #[test]
    fn cancellation_without_a_run_is_terminal_without_waiting() {
        let mut store = Store::new();
        let job = store.job.clone();
        let runtime = Runtime::new();
        let progress = Progress::default();
        let result = cancel_job(&mut store, job, grace(), &runtime, &progress).expect("cancel");
        assert_eq!(result.state(), JobState::Cancelled);
        assert_eq!(runtime.signals.load(Ordering::Relaxed), 0);
        assert!(progress.0.lock().expect("progress lock").is_empty());
    }

    #[test]
    fn pre_spawn_cancellation_records_the_same_claim() {
        let mut store = Store::with_run(false);
        let job = store.job.clone();
        let runtime = Runtime::new();
        let result =
            cancel_job(&mut store, job, grace(), &runtime, &Progress::default()).expect("cancel");
        assert_eq!(result.state(), JobState::Cancelled);
        assert_eq!(
            store.run.borrow().as_ref().expect("run").state(),
            RunState::Cancelled
        );
        assert_eq!(runtime.signals.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn revision_conflicts_retry_but_stop_after_eight_attempts() {
        for (conflicts, attempts, succeeds) in [(2, 3, true), (usize::MAX, 8, false)] {
            let mut store = Store::new();
            store.conflicts = conflicts;
            let job = store.job.clone();
            let result = cancel_job(
                &mut store,
                job,
                grace(),
                &Runtime::new(),
                &Progress::default(),
            );
            assert_eq!(store.request_attempts, attempts);
            if succeeds {
                assert_eq!(
                    result.expect("settled cancellation").state(),
                    JobState::Cancelled
                );
            } else {
                assert!(matches!(result, Err(CancelJobError::StateConflict(_))));
            }
        }
    }

    #[test]
    fn every_terminal_run_mapping_uses_the_application_interface() {
        for (outcome, expected) in [
            (RunOutcome::Exit(0), JobState::Succeeded),
            (RunOutcome::Exit(7), JobState::Failed),
            (RunOutcome::Signal(15), JobState::Failed),
            (RunOutcome::Failure("fixture".to_owned()), JobState::Failed),
            (
                RunOutcome::Cancelled("fixture".to_owned()),
                JobState::Cancelled,
            ),
            (
                RunOutcome::Interrupted("fixture".to_owned()),
                JobState::Interrupted,
            ),
        ] {
            let mut store = Store::with_run(true);
            let mut terminal = store.run.borrow().clone().expect("run");
            if matches!(&outcome, RunOutcome::Cancelled(_)) {
                terminal = terminal
                    .request_cancellation()
                    .expect("peer run cancellation");
            }
            store.terminal_at_cancel = Some(
                terminal
                    .with_outcome(timestamp(10_000), outcome)
                    .expect("terminal run"),
            );
            let job = store.job.clone();
            let runtime = Runtime::new();
            let result = cancel_job(&mut store, job, grace(), &runtime, &Progress::default())
                .expect("natural terminal race");
            assert_eq!(result.state(), expected);
            assert_eq!(runtime.signals.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn timeout_and_progress_preserve_both_wait_stages() {
        let mut store = Store::with_run(true);
        let job = store.job.clone();
        let runtime = Runtime::new();
        let progress = Progress::default();
        let result = cancel_job(&mut store, job, grace(), &runtime, &progress);
        assert!(matches!(
            result,
            Err(CancelJobError::StateConflict(
                "run did not stop before the cancellation deadline"
            ))
        ));
        assert_eq!(
            runtime.now().duration_since(runtime.origin),
            Duration::from_secs(4)
        );
        assert_eq!(
            *progress.0.lock().expect("progress lock"),
            [Duration::from_secs(4)]
        );
        assert_eq!(runtime.signals.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clock_failures_remain_typed_without_mutating_the_job() {
        let mut store = Store::new();
        let job = store.job.clone();
        let before = job.clone();
        let mut runtime = Runtime::new();
        runtime.clock_failure = true;
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &runtime, &Progress::default()),
            Err(CancelJobError::Clock(ClockError::Unavailable))
        ));
        assert_eq!(store.job, before);
    }

    #[test]
    fn a_concurrent_canceller_request_is_already_satisfied() {
        let mut store = Store::new();
        store.conflicts = 1;
        store.peer_request_on_conflict = true;
        let job = store.job.clone();
        let result = cancel_job(
            &mut store,
            job,
            grace(),
            &Runtime::new(),
            &Progress::default(),
        )
        .expect("peer cancellation");
        assert_eq!(result.state(), JobState::CancelRequested);
        assert_eq!(store.request_attempts, 1);
    }

    #[test]
    fn permanent_store_errors_are_typed_and_not_retried() {
        let mut store = Store::new();
        store.request_failure = Some(JobCancellationStoreError::Failed(
            "fixture failure".to_owned(),
        ));
        let job = store.job.clone();
        let result = cancel_job(
            &mut store,
            job,
            grace(),
            &Runtime::new(),
            &Progress::default(),
        );
        assert!(matches!(
            result,
            Err(CancelJobError::Store(JobCancellationStoreError::Failed(_)))
        ));
        assert_eq!(store.request_attempts, 1);
    }

    #[test]
    fn finish_mapping_and_already_terminal_revision_are_preserved() {
        for (run, expected) in [
            (RunState::Succeeded, JobState::Succeeded),
            (RunState::Cancelled, JobState::Cancelled),
            (RunState::Failed, JobState::Failed),
            (RunState::Interrupted, JobState::Interrupted),
        ] {
            let mut store = Store::new();
            store
                .job
                .transition(
                    JobState::CancelRequested,
                    false,
                    TransitionActor::Cli,
                    "fixture request",
                    timestamp(1_001),
                )
                .expect("request");
            let id = store.job.id();
            let runtime = Runtime::new();
            let finished = super::finish_job(&mut store, id, run, &runtime).expect("finish");
            assert_eq!(finished.state(), expected);
            let repeated =
                super::finish_job(&mut store, id, run, &runtime).expect("already terminal");
            assert_eq!(repeated.revision(), finished.revision());
        }
    }

    #[test]
    fn finish_refuses_nonterminal_run_states() {
        let mut store = Store::new();
        let id = store.job.id();
        for state in [
            RunState::Starting,
            RunState::Running,
            RunState::CancelRequested,
        ] {
            assert!(matches!(
                super::finish_job(&mut store, id, state, &Runtime::new()),
                Err(CancelJobError::StateConflict(
                    "run remained nonterminal after cancellation"
                ))
            ));
        }
    }

    #[test]
    fn finish_conflict_reloads_and_rejects_a_still_active_job() {
        let mut store = Store::new();
        store
            .job
            .transition(
                JobState::CancelRequested,
                false,
                TransitionActor::Cli,
                "fixture request",
                timestamp(1_001),
            )
            .expect("request");
        store.completion_failure = Some(JobCancellationStoreError::Conflict);
        let id = store.job.id();
        assert!(matches!(
            super::finish_job(&mut store, id, RunState::Succeeded, &Runtime::new()),
            Err(CancelJobError::StateConflict(
                "job changed state during cancellation"
            ))
        ));
    }

    #[test]
    fn a_missing_job_after_a_conflict_is_not_found_without_signalling() {
        let mut store = Store::new();
        store.conflicts = 1;
        store.missing_job = true;
        let job = store.job.clone();
        let runtime = Runtime::new();
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &runtime, &()),
            Err(CancelJobError::NotFound)
        ));
        assert_eq!(store.request_attempts, 1);
        assert_eq!(runtime.signals.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_disappearing_run_is_a_state_conflict() {
        let mut store = Store::with_run(false);
        store.missing_run = true;
        let job = store.job.clone();
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &Runtime::new(), &()),
            Err(CancelJobError::StateConflict("active run disappeared"))
        ));
    }

    #[test]
    fn polling_storage_errors_remain_typed() {
        let mut store = Store::with_run(false);
        store.poll_failure = Some(JobCancellationStoreError::Failed(
            "fixture read failure".to_owned(),
        ));
        let job = store.job.clone();
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &Runtime::new(), &()),
            Err(CancelJobError::Store(JobCancellationStoreError::Failed(_)))
        ));
    }

    #[test]
    fn process_errors_keep_the_durable_request_and_typed_cause() {
        let mut store = Store::with_run(true);
        let job = store.job.clone();
        let mut runtime = Runtime::new();
        runtime.process_failure = true;
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &runtime, &()),
            Err(CancelJobError::Run(super::CancelRunError::Process(_)))
        ));
        assert_eq!(store.job.state(), JobState::CancelRequested);
        assert_eq!(
            store.run.borrow().as_ref().expect("run").state(),
            RunState::CancelRequested
        );
        assert_eq!(runtime.signals.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn rejected_stale_transitions_have_the_same_bounded_reload_policy() {
        let mut store = Store::new();
        store.request_failure = Some(JobCancellationStoreError::Rejected(
            "fixture stale timestamp".to_owned(),
        ));
        let job = store.job.clone();
        assert!(matches!(
            cancel_job(&mut store, job, grace(), &Runtime::new(), &()),
            Err(CancelJobError::StateConflict(
                "job changed state during cancellation"
            ))
        ));
        assert_eq!(store.request_attempts, 8);
    }

    #[test]
    fn satisfied_requests_do_not_require_clock_or_process_ports() {
        for terminal in [false, true] {
            let mut store = Store::new();
            store
                .job
                .transition(
                    JobState::CancelRequested,
                    false,
                    TransitionActor::Cli,
                    "fixture request",
                    timestamp(1_001),
                )
                .expect("request");
            if terminal {
                store
                    .job
                    .transition(
                        JobState::Cancelled,
                        false,
                        TransitionActor::Cli,
                        "fixture completion",
                        timestamp(1_002),
                    )
                    .expect("cancelled");
            }
            let job = store.job.clone();
            let before = job.clone();
            let mut runtime = Runtime::new();
            runtime.clock_failure = true;
            let progress = Progress::default();
            assert_eq!(
                cancel_job(&mut store, job, grace(), &runtime, &progress)
                    .expect("already satisfied"),
                before
            );
            assert_eq!(store.request_attempts, 0);
            assert!(progress.0.lock().expect("progress lock").is_empty());
        }
    }
}
