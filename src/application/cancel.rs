//! Cancellation application service.

use thiserror::Error;

use crate::domain::{ClaimToken, Run, RunId};

pub(crate) trait CancellationStore {
    fn load_for_cancellation(&self, id: RunId) -> Result<Option<Run>, CancellationStoreError>;

    fn commit_cancellation(
        &mut self,
        id: RunId,
        claim_token: ClaimToken,
    ) -> Result<Run, CancellationStoreError>;
}

pub(crate) trait ProcessGroupCanceller {
    fn cancel_group(&self, run: &Run) -> Result<GroupCancellation, ProcessCancellationError>;
}

pub(crate) fn cancel_claimed_run<Store, Canceller>(
    store: &mut Store,
    canceller: &Canceller,
    id: RunId,
    claim_token: ClaimToken,
) -> Result<CancelRunResult, CancelRunError>
where
    Store: CancellationStore,
    Canceller: ProcessGroupCanceller,
{
    let current = store
        .load_for_cancellation(id)?
        .ok_or(CancelRunError::NotFound)?;
    if current.claim_token() != claim_token {
        return Err(CancelRunError::InvalidClaim);
    }
    if current.state().is_terminal() {
        return Ok(CancelRunResult::AlreadyTerminal(current));
    }

    // The durable state change must happen before the first signal.
    let committed = store.commit_cancellation(id, claim_token)?;
    if committed.command_identity().is_none() {
        return Ok(CancelRunResult::CommittedBeforeSpawn(committed));
    }
    let result = canceller.cancel_group(&committed)?;
    Ok(CancelRunResult::Signalled {
        run: committed,
        result,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CancelRunResult {
    AlreadyTerminal(Run),
    CommittedBeforeSpawn(Run),
    Signalled { run: Run, result: GroupCancellation },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GroupCancellation {
    AlreadyDead,
    IdentityChanged,
    TerminatedDuringGrace,
    KilledAfterGrace,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("cancellation storage failed: {0}")]
pub(crate) struct CancellationStoreError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("process-group cancellation failed: {0}")]
pub(crate) struct ProcessCancellationError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum CancelRunError {
    #[error("run was not found")]
    NotFound,
    #[error("run claim token did not match")]
    InvalidClaim,
    #[error(transparent)]
    Store(#[from] CancellationStoreError),
    #[error(transparent)]
    Process(#[from] ProcessCancellationError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::Cell;

    use super::{
        CancelRunError, CancelRunResult, CancellationStore, CancellationStoreError,
        GroupCancellation, ProcessCancellationError, ProcessGroupCanceller, cancel_claimed_run,
    };
    use crate::domain::{
        ClaimToken, JobId, ProcessIdentitySnapshot, Run, RunId, RunOutcome, Sequence, UtcTimestamp,
    };

    struct Store {
        run: Run,
        committed: bool,
    }

    impl CancellationStore for Store {
        fn load_for_cancellation(&self, _id: RunId) -> Result<Option<Run>, CancellationStoreError> {
            Ok(Some(self.run.clone()))
        }

        fn commit_cancellation(
            &mut self,
            _id: RunId,
            _claim_token: ClaimToken,
        ) -> Result<Run, CancellationStoreError> {
            self.committed = true;
            self.run = self
                .run
                .clone()
                .request_cancellation()
                .map_err(|error| CancellationStoreError(error.to_string()))?;
            Ok(self.run.clone())
        }
    }

    struct FlagCanceller<'a>(&'a Cell<bool>);

    impl ProcessGroupCanceller for FlagCanceller<'_> {
        fn cancel_group(&self, run: &Run) -> Result<GroupCancellation, ProcessCancellationError> {
            self.0.set(matches!(
                run.state(),
                crate::domain::RunState::CancelRequested
            ));
            Ok(GroupCancellation::KilledAfterGrace)
        }
    }

    #[test]
    fn cancellation_is_committed_before_signalling() {
        let timestamp = UtcTimestamp::from_second(1_000).expect("timestamp");
        let token = ClaimToken::from_bytes([7; 32]);
        let run = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            timestamp,
            timestamp,
            token,
        )
        .mark_running(
            timestamp,
            identity(10, 100, 10),
            identity(11, 101, 20),
            "runs/out.log".to_owned(),
            "runs/err.log".to_owned(),
        )
        .expect("running");
        let id = run.id();
        let mut store = Store {
            run,
            committed: false,
        };
        let committed = Cell::new(false);
        let result =
            cancel_claimed_run(&mut store, &FlagCanceller(&committed), id, token).expect("cancel");
        assert!(committed.get());
        assert!(matches!(result, CancelRunResult::Signalled { .. }));
    }

    struct MissingStore;

    impl CancellationStore for MissingStore {
        fn load_for_cancellation(&self, _id: RunId) -> Result<Option<Run>, CancellationStoreError> {
            Ok(None)
        }

        fn commit_cancellation(
            &mut self,
            _id: RunId,
            _claim_token: ClaimToken,
        ) -> Result<Run, CancellationStoreError> {
            Err(CancellationStoreError("nothing to commit".to_owned()))
        }
    }

    #[test]
    fn cancellation_rejects_missing_runs_and_stale_claims() {
        let timestamp = UtcTimestamp::from_second(2_000).expect("timestamp");
        let token = ClaimToken::from_bytes([7; 32]);
        let run = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            timestamp,
            timestamp,
            token,
        );
        let id = run.id();

        let mut missing = MissingStore;
        assert!(matches!(
            cancel_claimed_run(&mut missing, &FlagCanceller(&Cell::new(false)), id, token),
            Err(CancelRunError::NotFound)
        ));

        let mut store = Store {
            run,
            committed: false,
        };
        let stale = ClaimToken::from_bytes([9; 32]);
        assert!(matches!(
            cancel_claimed_run(&mut store, &FlagCanceller(&Cell::new(false)), id, stale),
            Err(CancelRunError::InvalidClaim)
        ));
    }

    #[test]
    fn terminal_runs_and_prespawn_commits_short_circuit() {
        let timestamp = UtcTimestamp::from_second(3_000).expect("timestamp");
        let token = ClaimToken::from_bytes([7; 32]);
        let finished = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            timestamp,
            timestamp,
            token,
        )
        .mark_running(
            timestamp,
            identity(10, 100, 10),
            identity(11, 101, 20),
            "runs/out.log".to_owned(),
            "runs/err.log".to_owned(),
        )
        .and_then(|run| {
            run.with_outcome(
                UtcTimestamp::from_second(3_002).expect("finished"),
                RunOutcome::Exit(0),
            )
        })
        .expect("terminal");
        let id = finished.id();
        let mut store = Store {
            run: finished,
            committed: false,
        };
        let result = cancel_claimed_run(&mut store, &FlagCanceller(&Cell::new(false)), id, token)
            .expect("already terminal");
        assert!(matches!(result, CancelRunResult::AlreadyTerminal(_)));
        assert!(!store.committed);

        // A run committed before the command spawned has no process group to
        // signal; the canceller must never run.
        let fresh = Run::new(
            JobId::new(),
            Sequence::new(2).expect("sequence"),
            timestamp,
            timestamp,
            token,
        );
        let fresh_id = fresh.id();
        let mut prespawn = Store {
            run: fresh,
            committed: false,
        };
        let signalled = Cell::new(false);
        let result = cancel_claimed_run(&mut prespawn, &FlagCanceller(&signalled), fresh_id, token)
            .expect("pre-spawn");
        assert!(matches!(result, CancelRunResult::CommittedBeforeSpawn(_)));
        assert!(prespawn.committed);
        assert!(!signalled.get());
    }

    fn identity(pid: u32, start_token: u64, process_group_id: i32) -> ProcessIdentitySnapshot {
        ProcessIdentitySnapshot {
            boot_identity: "boot".to_owned(),
            pid,
            start_token,
            process_group_id,
        }
    }
}
