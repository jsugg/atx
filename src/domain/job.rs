//! Job aggregate.

use serde::Serialize;
use thiserror::Error;

use super::execution::ExecutionSpec;
use super::id::JobId;
use super::primitives::{Description, Name, PrimitiveError, Revision, UtcTimestamp};
use super::schedule::{MissedPolicy, RuntimeTier, Schedule, ScheduleError};
use super::state::JobState;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Job {
    id: JobId,
    revision: Revision,
    name: Option<Name>,
    description: Option<Description>,
    created_at_utc: UtcTimestamp,
    updated_at_utc: UtcTimestamp,
    state: JobState,
    runtime_tier: RuntimeTier,
    schedule: Schedule,
    missed_policy: MissedPolicy,
    execution: ExecutionSpec,
    next_due_utc: UtcTimestamp,
    timezone_database_version: String,
    owner_uid: u32,
}

impl Job {
    pub(crate) fn new(
        now: UtcTimestamp,
        schedule: Schedule,
        missed_policy: MissedPolicy,
        runtime_tier: RuntimeTier,
        execution: ExecutionSpec,
        owner_uid: u32,
    ) -> Result<Self, JobError> {
        let next_due_utc = schedule.next_due_utc();
        if next_due_utc <= now {
            return Err(JobError::Schedule(ScheduleError::DeadlineNotFuture));
        }

        Ok(Self {
            id: JobId::new(),
            revision: Revision::new(1).map_err(JobError::Primitive)?,
            name: None,
            description: None,
            created_at_utc: now,
            updated_at_utc: now,
            state: JobState::Scheduled,
            runtime_tier,
            timezone_database_version: schedule.timezone_database_version().to_owned(),
            schedule,
            missed_policy,
            execution,
            next_due_utc,
            owner_uid,
        })
    }

    pub(crate) fn id(&self) -> JobId {
        self.id
    }

    pub(crate) fn revision(&self) -> Revision {
        self.revision
    }

    pub(crate) fn state(&self) -> JobState {
        self.state
    }

    pub(crate) fn next_due_utc(&self) -> UtcTimestamp {
        self.next_due_utc
    }

    pub(crate) fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    pub(crate) fn execution(&self) -> &ExecutionSpec {
        &self.execution
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum JobError {
    #[error(transparent)]
    Primitive(PrimitiveError),
    #[error(transparent)]
    Schedule(ScheduleError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::Job;
    use crate::domain::execution::{Environment, ExecutionMode, ExecutionSpec};
    use crate::domain::primitives::UtcTimestamp;
    use crate::domain::schedule::{DurationSeconds, MissedPolicy, RuntimeTier, Schedule};

    #[test]
    fn new_job_starts_scheduled_at_revision_one() {
        let now = UtcTimestamp::from_second(1_784_204_100).expect("valid timestamp");
        let due = UtcTimestamp::from_second(1_784_204_130).expect("valid timestamp");
        let schedule =
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("valid duration"), due);
        let execution = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid execution");

        let job = Job::new(
            now,
            schedule,
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("valid job");

        assert_eq!(job.revision().get(), 1);
        assert_eq!(job.next_due_utc(), due);
        assert_eq!(job.state(), crate::domain::state::JobState::Scheduled);
        assert_eq!(job.schedule().next_due_utc(), due);
        assert_eq!(job.execution().argv(), ["true"]);
        assert_eq!(job.id().as_uuid().get_version_num(), 7);
    }

    #[test]
    fn rejects_nonfuture_deadline() {
        let now = UtcTimestamp::from_second(100).expect("valid timestamp");
        let schedule =
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("valid duration"), now);
        let execution = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid execution");
        assert!(
            Job::new(
                now,
                schedule,
                MissedPolicy::Hold,
                RuntimeTier::Session,
                execution,
                501,
            )
            .is_err()
        );
    }
}
