//! `SQLite` adapter for job cancellation ports.

use super::{JobStore, StoreError};
use crate::application::{JobCancellationStore, JobCancellationStoreError};
use crate::domain::{
    ClaimToken, Job, JobId, JobState, Revision, Run, RunId, RunOutcome, TransitionActor,
    UtcTimestamp,
};

impl From<StoreError> for JobCancellationStoreError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Conflict => Self::Conflict,
            StoreError::Domain(message) => Self::Rejected(message),
            error => Self::Failed(error.to_string()),
        }
    }
}

impl JobCancellationStore for JobStore {
    fn load_job_for_cancellation(
        &self,
        id: JobId,
    ) -> Result<Option<Job>, JobCancellationStoreError> {
        self.load(id).map_err(JobCancellationStoreError::from)
    }

    fn latest_active_run_for_cancellation(
        &self,
        id: JobId,
    ) -> Result<Option<Run>, JobCancellationStoreError> {
        super::management::latest_active_run(self, id).map_err(JobCancellationStoreError::from)
    }

    fn transition_cancellation(
        &mut self,
        id: JobId,
        revision: Revision,
        state: JobState,
        reason: &'static str,
        now: UtcTimestamp,
    ) -> Result<Job, JobCancellationStoreError> {
        self.transition_job(
            id,
            revision,
            state,
            false,
            TransitionActor::Cli,
            reason,
            now,
        )
        .map_err(JobCancellationStoreError::from)
    }

    fn finish_cancelled_run(
        &mut self,
        id: RunId,
        token: ClaimToken,
        now: UtcTimestamp,
    ) -> Result<(), JobCancellationStoreError> {
        self.record_run_terminal(
            id,
            token,
            now,
            RunOutcome::Cancelled("cancelled before command start".to_owned()),
        )
        .map_err(JobCancellationStoreError::from)?;
        Ok(())
    }

    fn load_run_for_cancellation(
        &self,
        id: RunId,
    ) -> Result<Option<Run>, JobCancellationStoreError> {
        self.load_run(id).map_err(JobCancellationStoreError::from)
    }
}
