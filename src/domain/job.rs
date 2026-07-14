//! Job aggregate.

use serde::Serialize;
use thiserror::Error;

use super::execution::ExecutionSpec;
use super::id::JobId;
use super::primitives::{Description, Name, PrimitiveError, Revision, UtcTimestamp};
use super::schedule::{MissedPolicy, RuntimeTier, Schedule, ScheduleError};
use super::state::JobState;
use super::transition::{Transition, TransitionActor, TransitionError, job_transition};

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

    pub(crate) const fn missed_policy(&self) -> MissedPolicy {
        self.missed_policy
    }

    pub(crate) fn execution(&self) -> &ExecutionSpec {
        &self.execution
    }

    pub(crate) fn set_metadata(&mut self, name: Option<Name>, description: Option<Description>) {
        self.name = name;
        self.description = description;
    }

    pub(crate) fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            id: self.id,
            revision: self.revision,
            name: self.name.clone(),
            description: self.description.clone(),
            created_at_utc: self.created_at_utc,
            updated_at_utc: self.updated_at_utc,
            state: self.state,
            runtime_tier: self.runtime_tier,
            schedule: self.schedule.clone(),
            missed_policy: self.missed_policy,
            execution: self.execution.clone(),
            next_due_utc: self.next_due_utc,
            timezone_database_version: self.timezone_database_version.clone(),
            owner_uid: self.owner_uid,
        }
    }

    pub(crate) fn rehydrate(snapshot: JobSnapshot) -> Result<Self, JobError> {
        if snapshot.created_at_utc > snapshot.updated_at_utc
            || snapshot.timezone_database_version.is_empty()
            || snapshot.timezone_database_version.contains('\0')
        {
            return Err(JobError::CorruptSnapshot);
        }
        Ok(Self {
            id: snapshot.id,
            revision: snapshot.revision,
            name: snapshot.name,
            description: snapshot.description,
            created_at_utc: snapshot.created_at_utc,
            updated_at_utc: snapshot.updated_at_utc,
            state: snapshot.state,
            runtime_tier: snapshot.runtime_tier,
            schedule: snapshot.schedule,
            missed_policy: snapshot.missed_policy,
            execution: snapshot.execution,
            next_due_utc: snapshot.next_due_utc,
            timezone_database_version: snapshot.timezone_database_version,
            owner_uid: snapshot.owner_uid,
        })
    }

    pub(crate) fn transition(
        &mut self,
        to: JobState,
        recurring: bool,
        actor: TransitionActor,
        reason: &str,
        now: UtcTimestamp,
    ) -> Result<Transition<JobState>, JobError> {
        if now < self.updated_at_utc {
            return Err(JobError::TimeMovedBackward);
        }
        let transition = job_transition(self.state, to, recurring, actor, reason)
            .map_err(JobError::Transition)?;
        self.revision = self.revision.next().map_err(JobError::Primitive)?;
        self.state = to;
        self.updated_at_utc = now;
        Ok(transition)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JobSnapshot {
    pub(crate) id: JobId,
    pub(crate) revision: Revision,
    pub(crate) name: Option<Name>,
    pub(crate) description: Option<Description>,
    pub(crate) created_at_utc: UtcTimestamp,
    pub(crate) updated_at_utc: UtcTimestamp,
    pub(crate) state: JobState,
    pub(crate) runtime_tier: RuntimeTier,
    pub(crate) schedule: Schedule,
    pub(crate) missed_policy: MissedPolicy,
    pub(crate) execution: ExecutionSpec,
    pub(crate) next_due_utc: UtcTimestamp,
    pub(crate) timezone_database_version: String,
    pub(crate) owner_uid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum JobError {
    #[error(transparent)]
    Primitive(PrimitiveError),
    #[error(transparent)]
    Schedule(ScheduleError),
    #[error(transparent)]
    Transition(TransitionError),
    #[error("stored job snapshot violates domain invariants")]
    CorruptSnapshot,
    #[error("job update time cannot move backward")]
    TimeMovedBackward,
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
