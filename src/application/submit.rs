//! Job-submission application service.

use thiserror::Error;

use crate::domain::{Job, JobId, Revision};

pub(crate) trait SubmissionStore {
    fn create_job(&mut self, job: &Job) -> Result<(), SubmissionStoreError>;
}

pub(crate) trait SupervisorAcknowledger {
    fn acknowledge(&self, job_id: JobId, revision: Revision) -> Result<(), SupervisorAckError>;
}

pub(crate) fn submit_job<Store: SubmissionStore, Acknowledger: SupervisorAcknowledger>(
    store: &mut Store,
    acknowledger: &Acknowledger,
    job: Job,
    dry_run: bool,
) -> Result<SubmissionOutcome, SubmitError> {
    if dry_run {
        return Ok(SubmissionOutcome::DryRun(job));
    }

    store.create_job(&job)?;
    match acknowledger.acknowledge(job.id(), job.revision()) {
        Ok(()) => Ok(SubmissionOutcome::Supervised(job)),
        Err(error) => Ok(SubmissionOutcome::CommittedUnsupervised { job, error }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SubmissionOutcome {
    DryRun(Job),
    Supervised(Job),
    CommittedUnsupervised { job: Job, error: SupervisorAckError },
}

impl SubmissionOutcome {
    pub(crate) const fn job(&self) -> &Job {
        match self {
            Self::DryRun(job) | Self::Supervised(job) | Self::CommittedUnsupervised { job, .. } => {
                job
            }
        }
    }

    pub(crate) const fn is_supervised(&self) -> bool {
        matches!(self, Self::Supervised(_))
    }

    pub(crate) const fn is_dry_run(&self) -> bool {
        matches!(self, Self::DryRun(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("submission storage failed: {0}")]
pub(crate) struct SubmissionStoreError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("supervisor did not acknowledge job: {0}")]
pub(crate) struct SupervisorAckError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum SubmitError {
    #[error(transparent)]
    Store(#[from] SubmissionStoreError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::Cell;

    use super::{
        SubmissionOutcome, SubmissionStore, SubmissionStoreError, SupervisorAckError,
        SupervisorAcknowledger, submit_job,
    };
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobId, MissedPolicy,
        Revision, RuntimeTier, Schedule, UtcTimestamp,
    };

    struct Store(Cell<usize>);

    impl SubmissionStore for Store {
        fn create_job(&mut self, _job: &Job) -> Result<(), SubmissionStoreError> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }

    struct Ack {
        calls: Cell<usize>,
        fail: bool,
    }

    impl SupervisorAcknowledger for Ack {
        fn acknowledge(
            &self,
            _job_id: JobId,
            _revision: Revision,
        ) -> Result<(), SupervisorAckError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                Err(SupervisorAckError("socket unavailable".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn dry_run_never_calls_storage_or_supervisor() {
        let mut store = Store(Cell::new(0));
        let ack = Ack {
            calls: Cell::new(0),
            fail: false,
        };
        let outcome = submit_job(&mut store, &ack, job(), true).expect("dry-run");
        assert!(matches!(outcome, SubmissionOutcome::DryRun(_)));
        assert_eq!(store.0.get(), 0);
        assert_eq!(ack.calls.get(), 0);
    }

    #[test]
    fn committed_job_reports_missing_ack_without_rolling_back() {
        let mut store = Store(Cell::new(0));
        let ack = Ack {
            calls: Cell::new(0),
            fail: true,
        };
        let outcome = submit_job(&mut store, &ack, job(), false).expect("commit");
        assert!(matches!(
            outcome,
            SubmissionOutcome::CommittedUnsupervised { .. }
        ));
        assert_eq!(store.0.get(), 1);
        assert_eq!(ack.calls.get(), 1);
    }

    #[test]
    fn acknowledged_commit_reports_a_supervised_job() {
        let mut store = Store(Cell::new(0));
        let ack = Ack {
            calls: Cell::new(0),
            fail: false,
        };
        let outcome = submit_job(&mut store, &ack, job(), false).expect("commit");
        assert!(matches!(outcome, SubmissionOutcome::Supervised(_)));
        assert_eq!(store.0.get(), 1);
        assert_eq!(ack.calls.get(), 1);
    }

    fn job() -> Job {
        let now = UtcTimestamp::from_second(100).expect("timestamp");
        Job::new(
            now,
            Schedule::one_shot_relative(
                DurationSeconds::new(30).expect("duration"),
                UtcTimestamp::from_second(130).expect("timestamp"),
            ),
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
        .expect("job")
    }
}
