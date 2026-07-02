//! Run aggregate.

use serde::Serialize;
use thiserror::Error;

use super::id::{JobId, RunId};
use super::primitives::{Sequence, UtcTimestamp};
use super::state::RunState;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum RunOutcome {
    Exit(i32),
    Signal(i32),
    Failure(String),
    Interrupted(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProcessIdentitySnapshot {
    pub(crate) boot_identity: String,
    pub(crate) pid: u32,
    pub(crate) start_token: u64,
    pub(crate) process_group_id: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Run {
    id: RunId,
    job_id: JobId,
    sequence: Sequence,
    scheduled_for_utc: UtcTimestamp,
    created_at_utc: UtcTimestamp,
    started_at_utc: Option<UtcTimestamp>,
    finished_at_utc: Option<UtcTimestamp>,
    state: RunState,
    monitor_identity: Option<ProcessIdentitySnapshot>,
    command_identity: Option<ProcessIdentitySnapshot>,
    outcome: Option<RunOutcome>,
    stdout_path: Option<String>,
    stderr_path: Option<String>,
}

impl Run {
    pub(crate) fn new(
        job_id: JobId,
        sequence: Sequence,
        scheduled_for_utc: UtcTimestamp,
        created_at_utc: UtcTimestamp,
    ) -> Self {
        Self {
            id: RunId::new(),
            job_id,
            sequence,
            scheduled_for_utc,
            created_at_utc,
            started_at_utc: None,
            finished_at_utc: None,
            state: RunState::Starting,
            monitor_identity: None,
            command_identity: None,
            outcome: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    pub(crate) fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    pub(crate) fn with_outcome(
        mut self,
        finished_at_utc: UtcTimestamp,
        outcome: RunOutcome,
    ) -> Result<Self, RunError> {
        if self.outcome.is_some() {
            return Err(RunError::OutcomeAlreadyRecorded);
        }
        if finished_at_utc < self.created_at_utc {
            return Err(RunError::FinishBeforeCreation);
        }

        self.state = match &outcome {
            RunOutcome::Exit(0) => RunState::Succeeded,
            RunOutcome::Exit(_) | RunOutcome::Signal(_) | RunOutcome::Failure(_) => {
                RunState::Failed
            }
            RunOutcome::Interrupted(_) => RunState::Interrupted,
        };
        self.finished_at_utc = Some(finished_at_utc);
        self.outcome = Some(outcome);
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum RunError {
    #[error("run outcome is already recorded")]
    OutcomeAlreadyRecorded,
    #[error("run cannot finish before it was created")]
    FinishBeforeCreation,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{Run, RunOutcome};
    use crate::domain::id::JobId;
    use crate::domain::primitives::{Sequence, UtcTimestamp};

    #[test]
    fn new_run_has_one_outcome_slot() {
        let timestamp = UtcTimestamp::from_second(1_784_204_100).expect("valid timestamp");
        let run = Run::new(
            JobId::new(),
            Sequence::new(1).expect("valid sequence"),
            timestamp,
            timestamp,
        );
        assert!(run.outcome().is_none());

        let completed = run
            .with_outcome(timestamp, RunOutcome::Exit(0))
            .expect("first outcome is valid");
        assert_eq!(completed.outcome(), Some(&RunOutcome::Exit(0)));
        assert!(
            completed
                .with_outcome(timestamp, RunOutcome::Signal(15))
                .is_err()
        );
    }
}
