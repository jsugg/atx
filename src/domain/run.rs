//! Run aggregate.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::id::{JobId, RunId};
use super::primitives::{Sequence, UtcTimestamp};
use super::state::RunState;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ClaimToken([u8; 32]);

impl ClaimToken {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ClaimToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Serialize for ClaimToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum RunOutcome {
    Exit(i32),
    Signal(i32),
    Failure(String),
    Interrupted(String),
    Cancelled(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    claim_token: ClaimToken,
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
        claim_token: ClaimToken,
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
            claim_token,
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

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) const fn job_id(&self) -> JobId {
        self.job_id
    }

    pub(crate) const fn sequence(&self) -> Sequence {
        self.sequence
    }

    pub(crate) const fn state(&self) -> RunState {
        self.state
    }

    pub(crate) const fn claim_token(&self) -> ClaimToken {
        self.claim_token
    }

    pub(crate) const fn finished_at_utc(&self) -> Option<UtcTimestamp> {
        self.finished_at_utc
    }

    pub(crate) fn command_identity(&self) -> Option<&ProcessIdentitySnapshot> {
        self.command_identity.as_ref()
    }

    pub(crate) fn request_cancellation(mut self) -> Result<Self, RunError> {
        match self.state {
            RunState::Starting | RunState::Running => {
                self.state = RunState::CancelRequested;
                Ok(self)
            }
            RunState::CancelRequested => Ok(self),
            _ => Err(RunError::InvalidState),
        }
    }

    pub(crate) fn mark_running(
        mut self,
        started_at_utc: UtcTimestamp,
        monitor_identity: ProcessIdentitySnapshot,
        command_identity: ProcessIdentitySnapshot,
        stdout_path: String,
        stderr_path: String,
    ) -> Result<Self, RunError> {
        if self.state != RunState::Starting {
            return Err(RunError::InvalidState);
        }
        if started_at_utc < self.created_at_utc {
            return Err(RunError::StartBeforeCreation);
        }
        validate_identity(&monitor_identity)?;
        validate_identity(&command_identity)?;
        validate_log_path(&stdout_path)?;
        validate_log_path(&stderr_path)?;
        self.started_at_utc = Some(started_at_utc);
        self.state = RunState::Running;
        self.monitor_identity = Some(monitor_identity);
        self.command_identity = Some(command_identity);
        self.stdout_path = Some(stdout_path);
        self.stderr_path = Some(stderr_path);
        Ok(self)
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
        if self.state.is_terminal()
            || (matches!(&outcome, RunOutcome::Cancelled(_))
                && self.state != RunState::CancelRequested)
        {
            return Err(RunError::InvalidState);
        }

        self.state = outcome_state(&outcome);
        self.finished_at_utc = Some(finished_at_utc);
        self.outcome = Some(outcome);
        Ok(self)
    }

    pub(crate) fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            id: self.id,
            job_id: self.job_id,
            sequence: self.sequence,
            scheduled_for_utc: self.scheduled_for_utc,
            created_at_utc: self.created_at_utc,
            started_at_utc: self.started_at_utc,
            finished_at_utc: self.finished_at_utc,
            state: self.state,
            claim_token: self.claim_token,
            monitor_identity: self.monitor_identity.clone(),
            command_identity: self.command_identity.clone(),
            outcome: self.outcome.clone(),
            stdout_path: self.stdout_path.clone(),
            stderr_path: self.stderr_path.clone(),
        }
    }

    pub(crate) fn rehydrate(snapshot: RunSnapshot) -> Result<Self, RunError> {
        if snapshot
            .started_at_utc
            .is_some_and(|started| started < snapshot.created_at_utc)
            || snapshot
                .finished_at_utc
                .is_some_and(|finished| finished < snapshot.created_at_utc)
            || matches!(
                (snapshot.started_at_utc, snapshot.finished_at_utc),
                (Some(started), Some(finished)) if finished < started
            )
        {
            return Err(RunError::InvalidTimeline);
        }
        let terminal = snapshot.state.is_terminal();
        if terminal != snapshot.outcome.is_some()
            || terminal != snapshot.finished_at_utc.is_some()
            || snapshot
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome_state(outcome) != snapshot.state)
        {
            return Err(RunError::InvalidOutcome);
        }
        if let Some(identity) = &snapshot.monitor_identity {
            validate_identity(identity)?;
        }
        if let Some(identity) = &snapshot.command_identity {
            validate_identity(identity)?;
        }
        if let Some(path) = &snapshot.stdout_path {
            validate_log_path(path)?;
        }
        if let Some(path) = &snapshot.stderr_path {
            validate_log_path(path)?;
        }
        Ok(Self {
            id: snapshot.id,
            job_id: snapshot.job_id,
            sequence: snapshot.sequence,
            scheduled_for_utc: snapshot.scheduled_for_utc,
            created_at_utc: snapshot.created_at_utc,
            started_at_utc: snapshot.started_at_utc,
            finished_at_utc: snapshot.finished_at_utc,
            state: snapshot.state,
            claim_token: snapshot.claim_token,
            monitor_identity: snapshot.monitor_identity,
            command_identity: snapshot.command_identity,
            outcome: snapshot.outcome,
            stdout_path: snapshot.stdout_path,
            stderr_path: snapshot.stderr_path,
        })
    }
}

fn outcome_state(outcome: &RunOutcome) -> RunState {
    match outcome {
        RunOutcome::Exit(0) => RunState::Succeeded,
        RunOutcome::Exit(_) | RunOutcome::Signal(_) | RunOutcome::Failure(_) => RunState::Failed,
        RunOutcome::Interrupted(_) => RunState::Interrupted,
        RunOutcome::Cancelled(_) => RunState::Cancelled,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunSnapshot {
    pub(crate) id: RunId,
    pub(crate) job_id: JobId,
    pub(crate) sequence: Sequence,
    pub(crate) scheduled_for_utc: UtcTimestamp,
    pub(crate) created_at_utc: UtcTimestamp,
    pub(crate) started_at_utc: Option<UtcTimestamp>,
    pub(crate) finished_at_utc: Option<UtcTimestamp>,
    pub(crate) state: RunState,
    pub(crate) claim_token: ClaimToken,
    pub(crate) monitor_identity: Option<ProcessIdentitySnapshot>,
    pub(crate) command_identity: Option<ProcessIdentitySnapshot>,
    pub(crate) outcome: Option<RunOutcome>,
    pub(crate) stdout_path: Option<String>,
    pub(crate) stderr_path: Option<String>,
}

fn validate_identity(identity: &ProcessIdentitySnapshot) -> Result<(), RunError> {
    if identity.boot_identity.is_empty()
        || identity.boot_identity.contains('\0')
        || identity.pid == 0
        || identity.start_token == 0
        || identity.process_group_id <= 0
    {
        return Err(RunError::InvalidIdentity);
    }
    Ok(())
}

fn validate_log_path(path: &str) -> Result<(), RunError> {
    let path = std::path::Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(RunError::InvalidLogPath);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum RunError {
    #[error("run outcome is already recorded")]
    OutcomeAlreadyRecorded,
    #[error("run cannot finish before it was created")]
    FinishBeforeCreation,
    #[error("run cannot start before it was created")]
    StartBeforeCreation,
    #[error("run state does not allow this update")]
    InvalidState,
    #[error("process identity is incomplete")]
    InvalidIdentity,
    #[error("log path must stay below the run directory")]
    InvalidLogPath,
    #[error("stored run timeline is inconsistent")]
    InvalidTimeline,
    #[error("stored run outcome does not match its state")]
    InvalidOutcome,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ClaimToken, Run, RunOutcome};
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
            ClaimToken::from_bytes([1; 32]),
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

    #[test]
    fn finish_time_and_outcome_kinds_are_checked() {
        let created = UtcTimestamp::from_second(100).expect("valid timestamp");
        let earlier = UtcTimestamp::from_second(99).expect("valid timestamp");
        let run = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            created,
            created,
            ClaimToken::from_bytes([1; 32]),
        );
        assert!(
            run.clone()
                .with_outcome(earlier, RunOutcome::Failure("spawn".to_owned()))
                .is_err()
        );
        assert!(
            run.with_outcome(created, RunOutcome::Interrupted("unknown".to_owned()))
                .is_ok()
        );
    }

    #[test]
    fn claim_tokens_are_redacted() {
        let token = ClaimToken::from_bytes([42; 32]);
        assert_eq!(format!("{token:?}"), "[REDACTED]");
        assert_eq!(
            serde_json::to_string(&token).expect("serialize token"),
            "\"[REDACTED]\""
        );
    }
}
