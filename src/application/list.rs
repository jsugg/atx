//! Job management application services.

use thiserror::Error;

use crate::domain::{Job, JobId, JobState, Revision, Run, UtcTimestamp};

pub(crate) const MAX_MANAGEMENT_RESULTS: usize = 1_000;

pub(crate) trait ManagementStore {
    fn list_jobs(
        &self,
        state: Option<JobState>,
        after: Option<JobId>,
        limit: usize,
    ) -> Result<Vec<Job>, ManagementStoreError>;

    fn find_jobs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<Job>, ManagementStoreError>;

    fn list_runs(
        &self,
        job_id: Option<JobId>,
        limit: usize,
    ) -> Result<Vec<Run>, ManagementStoreError>;

    fn active_runs(&self) -> Result<Vec<Run>, ManagementStoreError>;

    fn latest_active_run(&self, job_id: JobId) -> Result<Option<Run>, ManagementStoreError>;

    fn hide_job(
        &mut self,
        job_id: JobId,
        expected_revision: Revision,
        keep_history: bool,
    ) -> Result<Job, ManagementStoreError>;

    fn prepare_rerun(
        &mut self,
        job_id: JobId,
        expected_revision: Revision,
        now: UtcTimestamp,
    ) -> Result<Job, ManagementStoreError>;
}

pub(crate) fn list_jobs<Store: ManagementStore>(
    store: &Store,
    state: Option<JobState>,
    after: Option<JobId>,
    limit: usize,
) -> Result<Vec<Job>, ManagementError> {
    validate_limit(limit)?;
    store
        .list_jobs(state, after, limit)
        .map_err(ManagementError::from)
}

pub(crate) fn list_runs<Store: ManagementStore>(
    store: &Store,
    job_id: Option<JobId>,
    limit: usize,
) -> Result<Vec<Run>, ManagementError> {
    validate_limit(limit)?;
    store
        .list_runs(job_id, limit)
        .map_err(ManagementError::from)
}

pub(crate) fn resolve_job<Store: ManagementStore>(
    store: &Store,
    prefix: &str,
) -> Result<Job, ManagementError> {
    if prefix.is_empty() || prefix.len() > 26 || !prefix.bytes().all(is_identifier_byte) {
        return Err(ManagementError::InvalidPrefix);
    }
    let matches = store.find_jobs_by_prefix(prefix, 2)?;
    match matches.as_slice() {
        [] => Err(ManagementError::NotFound),
        [job] => Ok(job.clone()),
        _ => Err(ManagementError::Ambiguous(
            matches.into_iter().map(|job| job.id()).collect(),
        )),
    }
}

pub(crate) fn remove_job<Store: ManagementStore>(
    store: &mut Store,
    prefix: &str,
    keep_history: bool,
) -> Result<Job, ManagementError> {
    let job = resolve_job(store, prefix)?;
    if !job.state().is_terminal() {
        return Err(ManagementError::StateConflict(
            "only terminal jobs can be removed",
        ));
    }
    store
        .hide_job(job.id(), job.revision(), keep_history)
        .map_err(ManagementError::from)
}

pub(crate) fn rerun_job<Store: ManagementStore>(
    store: &mut Store,
    prefix: &str,
    confirm_interrupted: bool,
    now: UtcTimestamp,
) -> Result<Job, ManagementError> {
    let job = resolve_job(store, prefix)?;
    if !job.state().is_terminal() {
        return Err(ManagementError::StateConflict(
            "only terminal jobs can be run again",
        ));
    }
    if job.state() == JobState::Interrupted && !confirm_interrupted {
        return Err(ManagementError::ConfirmationRequired);
    }
    store
        .prepare_rerun(job.id(), job.revision(), now)
        .map_err(ManagementError::from)
}

fn validate_limit(limit: usize) -> Result<(), ManagementError> {
    if (1..=MAX_MANAGEMENT_RESULTS).contains(&limit) {
        Ok(())
    } else {
        Err(ManagementError::InvalidLimit)
    }
}

const fn is_identifier_byte(byte: u8) -> bool {
    matches!(
        byte.to_ascii_lowercase(),
        b'0'..=b'9'
            | b'a'
            | b'b'
            | b'c'
            | b'd'
            | b'e'
            | b'f'
            | b'g'
            | b'h'
            | b'j'
            | b'k'
            | b'm'
            | b'n'
            | b'p'
            | b'q'
            | b'r'
            | b's'
            | b't'
            | b'v'
            | b'w'
            | b'x'
            | b'y'
            | b'z'
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("management storage failed: {0}")]
pub(crate) struct ManagementStoreError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ManagementError {
    #[error("job was not found")]
    NotFound,
    #[error("job prefix is ambiguous: {0:?}")]
    Ambiguous(Vec<JobId>),
    #[error("job prefix is not valid")]
    InvalidPrefix,
    #[error("result limit must be between 1 and 1000")]
    InvalidLimit,
    #[error("{0}")]
    StateConflict(&'static str),
    #[error("running an interrupted job again requires --yes")]
    ConfirmationRequired,
    #[error(transparent)]
    Store(#[from] ManagementStoreError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        ManagementError, ManagementStore, ManagementStoreError, list_jobs, list_runs, remove_job,
        rerun_job, resolve_job,
    };
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobId, JobState,
        MissedPolicy, Revision, Run, RuntimeTier, Schedule, TransitionActor, UtcTimestamp,
    };

    struct Store {
        jobs: Vec<Job>,
        hidden: bool,
        rerun: bool,
    }

    impl ManagementStore for Store {
        fn list_jobs(
            &self,
            _state: Option<JobState>,
            _after: Option<JobId>,
            limit: usize,
        ) -> Result<Vec<Job>, ManagementStoreError> {
            Ok(self.jobs.iter().take(limit).cloned().collect())
        }

        fn find_jobs_by_prefix(
            &self,
            _prefix: &str,
            limit: usize,
        ) -> Result<Vec<Job>, ManagementStoreError> {
            Ok(self.jobs.iter().take(limit).cloned().collect())
        }

        fn list_runs(
            &self,
            _job_id: Option<JobId>,
            _limit: usize,
        ) -> Result<Vec<Run>, ManagementStoreError> {
            Ok(Vec::new())
        }

        fn active_runs(&self) -> Result<Vec<Run>, ManagementStoreError> {
            Ok(Vec::new())
        }

        fn latest_active_run(&self, _job_id: JobId) -> Result<Option<Run>, ManagementStoreError> {
            Ok(None)
        }

        fn hide_job(
            &mut self,
            job_id: JobId,
            _expected_revision: Revision,
            _keep_history: bool,
        ) -> Result<Job, ManagementStoreError> {
            self.hidden = true;
            self.jobs
                .iter()
                .find(|job| job.id() == job_id)
                .cloned()
                .ok_or_else(|| ManagementStoreError("missing".to_owned()))
        }

        fn prepare_rerun(
            &mut self,
            job_id: JobId,
            _expected_revision: Revision,
            _now: UtcTimestamp,
        ) -> Result<Job, ManagementStoreError> {
            self.rerun = true;
            self.jobs
                .iter()
                .find(|job| job.id() == job_id)
                .cloned()
                .ok_or_else(|| ManagementStoreError("missing".to_owned()))
        }
    }

    #[test]
    fn prefixes_report_not_found_and_ambiguity() {
        let empty = Store {
            jobs: Vec::new(),
            hidden: false,
            rerun: false,
        };
        assert_eq!(resolve_job(&empty, "0"), Err(ManagementError::NotFound));

        let ambiguous = Store {
            jobs: vec![terminal(JobState::Succeeded), terminal(JobState::Failed)],
            hidden: false,
            rerun: false,
        };
        assert!(matches!(
            resolve_job(&ambiguous, "0"),
            Err(ManagementError::Ambiguous(ids)) if ids.len() == 2
        ));
    }

    #[test]
    fn removal_requires_a_terminal_job() {
        let mut active = Store {
            jobs: vec![waiting()],
            hidden: false,
            rerun: false,
        };
        assert!(matches!(
            remove_job(&mut active, "0", false),
            Err(ManagementError::StateConflict(_))
        ));
        assert!(!active.hidden);

        let mut done = Store {
            jobs: vec![terminal(JobState::Succeeded)],
            hidden: false,
            rerun: false,
        };
        assert!(remove_job(&mut done, "0", true).is_ok());
        assert!(done.hidden);
    }

    #[test]
    fn interrupted_rerun_needs_confirmation() {
        let mut store = Store {
            jobs: vec![terminal(JobState::Interrupted)],
            hidden: false,
            rerun: false,
        };
        assert_eq!(
            rerun_job(&mut store, "0", false, timestamp(10)),
            Err(ManagementError::ConfirmationRequired)
        );
        assert!(rerun_job(&mut store, "0", true, timestamp(10)).is_ok());
        assert!(store.rerun);
    }

    #[test]
    fn result_limits_are_bounded_before_storage() {
        let store = Store {
            jobs: vec![terminal(JobState::Succeeded)],
            hidden: false,
            rerun: false,
        };
        assert_eq!(
            list_jobs(&store, None, None, 1_001),
            Err(ManagementError::InvalidLimit)
        );
        assert_eq!(
            list_runs(&store, None, 0),
            Err(ManagementError::InvalidLimit)
        );
    }

    fn terminal(state: JobState) -> Job {
        let mut job = waiting();
        job.transition(
            JobState::Starting,
            false,
            TransitionActor::Supervisor,
            "test",
            timestamp(3),
        )
        .expect("starting");
        job.transition(
            JobState::Running,
            false,
            TransitionActor::Supervisor,
            "test",
            timestamp(4),
        )
        .expect("running");
        job.transition(state, false, TransitionActor::Monitor, "test", timestamp(5))
            .expect("terminal");
        job
    }

    fn waiting() -> Job {
        let mut job = Job::new(
            timestamp(1),
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("duration"), timestamp(30)),
            MissedPolicy::Hold,
            RuntimeTier::Session,
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["true".to_owned()],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .expect("execution"),
            501,
        )
        .expect("job");
        job.transition(
            JobState::Waiting,
            false,
            TransitionActor::Supervisor,
            "test",
            timestamp(2),
        )
        .expect("waiting");
        job
    }

    fn timestamp(second: i64) -> UtcTimestamp {
        UtcTimestamp::from_second(second).expect("timestamp")
    }
}
