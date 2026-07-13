//! `SQLite` startup-recovery adapter.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::job_store::{JOB_COLUMNS, decode_job_row};
use super::retention::RetentionPolicy;
use super::run_store::{RUN_COLUMNS, decode_run_row};
use super::{JobStore, StoreError, map_read_error, map_write_error};
use crate::application::{
    CommandFate, RecoveryAction, RecoveryRecord, RecoveryStore, RecoveryStoreError,
};
use crate::domain::{JobId, Revision, Run, RunOutcome, UtcTimestamp};

const MAX_RECOVERY_RECORDS: usize = 100_000;

pub(crate) struct StartupStore<'a> {
    store: &'a mut JobStore,
    retention_policy: RetentionPolicy,
}

impl<'a> StartupStore<'a> {
    pub(crate) const fn new(store: &'a mut JobStore, retention_policy: RetentionPolicy) -> Self {
        Self {
            store,
            retention_policy,
        }
    }
}

impl RecoveryStore for StartupStore<'_> {
    fn load_nonterminal(&self) -> Result<Vec<RecoveryRecord>, RecoveryStoreError> {
        load_nonterminal(self.store).map_err(|error| recovery_error(&error))
    }

    fn apply_recovery(
        &mut self,
        actions: &[RecoveryAction],
        now: UtcTimestamp,
    ) -> Result<(), RecoveryStoreError> {
        apply_recovery(self.store, actions, now).map_err(|error| recovery_error(&error))
    }

    fn cleanup_recovery(&mut self, now: UtcTimestamp) -> Result<(), RecoveryStoreError> {
        self.store
            .cleanup_retention(now, self.retention_policy)
            .map(|_| ())
            .map_err(|error| recovery_error(&error))
    }
}

fn load_nonterminal(store: &JobStore) -> Result<Vec<RecoveryRecord>, StoreError> {
    let connection = store.database.connection();
    let run_sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs
         WHERE state IN ('starting', 'running', 'cancel_requested')
         LIMIT {}",
        MAX_RECOVERY_RECORDS + 1
    );
    let mut run_statement = connection.prepare(&run_sql)?;
    let runs = run_statement.query_map([], decode_run_row)?;
    let mut active_runs = HashMap::<JobId, Run>::new();
    for run in runs {
        let run = run.map_err(map_read_error)?;
        if active_runs.insert(run.job_id(), run).is_some() {
            return Err(StoreError::Corrupt(
                "job has more than one active run".to_owned(),
            ));
        }
        if active_runs.len() > MAX_RECOVERY_RECORDS {
            return Err(StoreError::Corrupt(
                "too many active runs to reconcile".to_owned(),
            ));
        }
    }

    let job_sql = format!(
        "SELECT {JOB_COLUMNS} FROM jobs
         WHERE state IN ('scheduled', 'waiting', 'starting', 'running', 'cancel_requested')
         ORDER BY id
         LIMIT {}",
        MAX_RECOVERY_RECORDS + 1
    );
    let mut job_statement = connection.prepare(&job_sql)?;
    let jobs = job_statement.query_map([], decode_job_row)?;
    let mut records = Vec::new();
    for job in jobs {
        let job = job.map_err(map_read_error)?;
        let active_run = active_runs.remove(&job.id());
        records.push(RecoveryRecord { job, active_run });
        if records.len() > MAX_RECOVERY_RECORDS {
            return Err(StoreError::Corrupt("too many jobs to reconcile".to_owned()));
        }
    }
    if !active_runs.is_empty() {
        return Err(StoreError::Corrupt(
            "active run belongs to a terminal or missing job".to_owned(),
        ));
    }
    Ok(records)
}

fn apply_recovery(
    store: &mut JobStore,
    actions: &[RecoveryAction],
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let transaction = store
        .database
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_write_error)?;
    for action in actions {
        match action {
            RecoveryAction::Interrupt {
                job_id,
                expected_revision,
                run,
                command_fate,
            } => {
                let reason = interruption_reason(*command_fate);
                if let Some((run_id, claim_token)) = run {
                    let outcome = RunOutcome::Interrupted(reason.to_owned());
                    let outcome_json = serde_json::to_string(&outcome)
                        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
                    let changed = transaction
                        .execute(
                            "UPDATE runs SET
                                state = 'interrupted', finished_at_utc = ?1,
                                failure = ?2, outcome_json = ?3
                             WHERE id = ?4
                               AND state IN ('starting', 'running', 'cancel_requested')
                               AND claim_token = ?5",
                            params![
                                now.to_string(),
                                reason,
                                outcome_json,
                                run_id.to_string(),
                                claim_token.as_bytes().as_slice(),
                            ],
                        )
                        .map_err(map_write_error)?;
                    if changed != 1 {
                        return Err(StoreError::Conflict);
                    }
                }
                transition_job(
                    &transaction,
                    *job_id,
                    *expected_revision,
                    "interrupted",
                    now,
                    reason,
                    &["starting", "running", "cancel_requested"],
                )?;
            }
            RecoveryAction::MarkMissed {
                job_id,
                expected_revision,
            } => transition_job(
                &transaction,
                *job_id,
                *expected_revision,
                "missed",
                now,
                "deadline passed while no supervisor owned the job",
                &["scheduled", "waiting"],
            )?,
            RecoveryAction::AdvanceRecurring {
                job_id,
                expected_revision,
                next_due_utc,
            } => {
                let next_revision = next_revision(*expected_revision)?;
                let changed = transaction
                    .execute(
                        "UPDATE jobs SET
                            revision = ?1, updated_at_utc = ?2, next_due_utc = ?3
                         WHERE id = ?4 AND revision = ?5
                           AND state IN ('scheduled', 'waiting')",
                        params![
                            next_revision,
                            now.to_string(),
                            next_due_utc.to_string(),
                            job_id.to_string(),
                            revision_i64(*expected_revision)?,
                        ],
                    )
                    .map_err(map_write_error)?;
                if changed != 1 {
                    return Err(StoreError::Conflict);
                }
            }
        }
    }
    transaction.commit().map_err(map_write_error)
}

fn transition_job(
    transaction: &Transaction<'_>,
    job_id: JobId,
    expected_revision: Revision,
    target: &str,
    now: UtcTimestamp,
    reason: &str,
    allowed_states: &[&str],
) -> Result<(), StoreError> {
    let current = transaction
        .query_row(
            "SELECT state FROM jobs WHERE id = ?1 AND revision = ?2",
            params![job_id.to_string(), revision_i64(expected_revision)?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_read_error)?
        .ok_or(StoreError::Conflict)?;
    if !allowed_states.contains(&current.as_str()) {
        return Err(StoreError::Conflict);
    }
    let revision = next_revision(expected_revision)?;
    let changed = transaction
        .execute(
            "UPDATE jobs SET revision = ?1, state = ?2, updated_at_utc = ?3
             WHERE id = ?4 AND revision = ?5 AND state = ?6",
            params![
                revision,
                target,
                now.to_string(),
                job_id.to_string(),
                revision_i64(expected_revision)?,
                current,
            ],
        )
        .map_err(map_write_error)?;
    if changed != 1 {
        return Err(StoreError::Conflict);
    }
    transaction
        .execute(
            "INSERT INTO transitions(
                job_id, run_id, from_state, to_state, occurred_at_utc,
                actor, reason, revision
             ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                job_id.to_string(),
                current,
                target,
                now.to_string(),
                "recovery",
                reason,
                revision,
            ],
        )
        .map_err(map_write_error)?;
    Ok(())
}

fn next_revision(revision: Revision) -> Result<i64, StoreError> {
    revision
        .next()
        .map_err(|error| StoreError::Domain(error.to_string()))
        .and_then(revision_i64)
}

fn revision_i64(revision: Revision) -> Result<i64, StoreError> {
    i64::try_from(revision.get())
        .map_err(|_| StoreError::Corrupt("revision exceeds SQLite i64".to_owned()))
}

const fn interruption_reason(fate: CommandFate) -> &'static str {
    match fate {
        CommandFate::Alive => "run monitor disappeared while command remains alive",
        CommandFate::Dead => "run monitor and command are no longer alive",
        CommandFate::Changed => "stored process identity no longer matches",
        CommandFate::Unknown => "run outcome and process identity are unknown",
    }
}

fn recovery_error(error: &StoreError) -> RecoveryStoreError {
    RecoveryStoreError(error.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::Cell;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::super::job_store::tests::sample_job;
    use super::super::{Database, JobStore, RetentionPolicy, StartupStore};
    use crate::application::{
        IdentityInspectionError, IdentityInspector, IdentityStatus, RecoveryStore,
        reconcile_startup,
    };
    use crate::domain::{
        ElapsedInstant, JobState, ProcessIdentitySnapshot, TransitionActor, UtcTimestamp,
    };

    struct UnusedInspector(Cell<usize>);

    impl IdentityInspector for UnusedInspector {
        fn classify(
            &self,
            _identity: &ProcessIdentitySnapshot,
        ) -> Result<IdentityStatus, IdentityInspectionError> {
            self.0.set(self.0.get() + 1);
            Ok(IdentityStatus::Dead)
        }
    }

    #[test]
    fn missed_job_recovery_is_atomic_and_idempotent() {
        let root = tempdir().expect("root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1, 100);
        store.create(&job).expect("create");
        let waiting = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "loaded",
                UtcTimestamp::from_second(2).expect("timestamp"),
            )
            .expect("waiting");

        let plan = {
            let mut startup =
                StartupStore::new(&mut store, RetentionPolicy::new(30, 30).expect("retention"));
            reconcile_startup(
                &mut startup,
                &UnusedInspector(Cell::new(0)),
                UtcTimestamp::from_second(200).expect("timestamp"),
                ElapsedInstant::from_nanos(500),
                "boot",
            )
            .expect("reconcile")
        };
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(
            store
                .load(waiting.id())
                .expect("load")
                .expect("job")
                .state(),
            JobState::Missed
        );
        let startup =
            StartupStore::new(&mut store, RetentionPolicy::new(30, 30).expect("retention"));
        assert!(startup.load_nonterminal().expect("nonterminal").is_empty());
    }
}
