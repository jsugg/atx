//! `SQLite` management-query adapter.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::job_store::{JOB_COLUMNS, decode_job_row, load_job};
use super::run_store::{RUN_COLUMNS, decode_run_row};
use super::{JobStore, StoreError, map_read_error, map_write_error};
use crate::application::{
    ManagementStore, ManagementStoreError, RunOutputStore, RunOutputStoreError,
};
use crate::domain::{Job, JobId, JobState, Revision, Run, RunId, UtcTimestamp};

impl ManagementStore for JobStore {
    fn list_jobs(
        &self,
        state: Option<JobState>,
        after: Option<JobId>,
        limit: usize,
    ) -> Result<Vec<Job>, ManagementStoreError> {
        list_jobs(self, state, after, limit).map_err(|error| management_error(&error))
    }

    fn find_jobs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<Job>, ManagementStoreError> {
        find_jobs_by_prefix(self, prefix, limit).map_err(|error| management_error(&error))
    }

    fn list_runs(
        &self,
        job_id: Option<JobId>,
        limit: usize,
    ) -> Result<Vec<Run>, ManagementStoreError> {
        list_runs(self, job_id, limit).map_err(|error| management_error(&error))
    }

    fn active_runs(&self) -> Result<Vec<Run>, ManagementStoreError> {
        active_runs(self).map_err(|error| management_error(&error))
    }

    fn latest_active_run(&self, job_id: JobId) -> Result<Option<Run>, ManagementStoreError> {
        latest_active_run(self, job_id).map_err(|error| management_error(&error))
    }

    fn hide_job(
        &mut self,
        job_id: JobId,
        expected_revision: Revision,
        keep_history: bool,
    ) -> Result<Job, ManagementStoreError> {
        hide_job(self, job_id, expected_revision, keep_history)
            .map_err(|error| management_error(&error))
    }

    fn prepare_rerun(
        &mut self,
        job_id: JobId,
        expected_revision: Revision,
        now: UtcTimestamp,
    ) -> Result<Job, ManagementStoreError> {
        prepare_rerun(self, job_id, expected_revision, now)
            .map_err(|error| management_error(&error))
    }
}

fn list_jobs(
    store: &JobStore,
    state: Option<JobState>,
    after: Option<JobId>,
    limit: usize,
) -> Result<Vec<Job>, StoreError> {
    let after = after.map(|id| id.to_string()).unwrap_or_default();
    let state = state.map(encode_job_state);
    let limit =
        i64::try_from(limit).map_err(|_| StoreError::Domain("result limit overflow".to_owned()))?;
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM jobs
         WHERE hidden = 0
           AND (?1 = '' OR id > ?1)
           AND (?2 IS NULL OR state = ?2)
         ORDER BY id
         LIMIT ?3"
    );
    let mut statement = store.database.connection().prepare(&sql)?;
    let rows = statement.query_map(params![after, state, limit], decode_job_row)?;
    rows.map(|row| row.map_err(map_read_error)).collect()
}

fn find_jobs_by_prefix(
    store: &JobStore,
    prefix: &str,
    limit: usize,
) -> Result<Vec<Job>, StoreError> {
    let prefix = prefix.to_ascii_lowercase();
    let prefix_len = i64::try_from(prefix.len())
        .map_err(|_| StoreError::Domain("prefix length overflow".to_owned()))?;
    let limit =
        i64::try_from(limit).map_err(|_| StoreError::Domain("result limit overflow".to_owned()))?;
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM jobs
         WHERE hidden = 0 AND substr(id, 1, ?1) = ?2
         ORDER BY id
         LIMIT ?3"
    );
    let mut statement = store.database.connection().prepare(&sql)?;
    let rows = statement.query_map(params![prefix_len, prefix, limit], decode_job_row)?;
    rows.map(|row| row.map_err(map_read_error)).collect()
}

fn list_runs(
    store: &JobStore,
    job_id: Option<JobId>,
    limit: usize,
) -> Result<Vec<Run>, StoreError> {
    let job_id = job_id.map(|id| id.to_string());
    let limit =
        i64::try_from(limit).map_err(|_| StoreError::Domain("result limit overflow".to_owned()))?;
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs
         WHERE (?1 IS NULL OR job_id = ?1)
         ORDER BY created_at_utc DESC, id DESC
         LIMIT ?2"
    );
    let mut statement = store.database.connection().prepare(&sql)?;
    let rows = statement.query_map(params![job_id, limit], decode_run_row)?;
    rows.map(|row| row.map_err(map_read_error)).collect()
}

fn active_runs(store: &JobStore) -> Result<Vec<Run>, StoreError> {
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs
         WHERE state IN ('starting', 'running', 'cancel_requested')
         ORDER BY created_at_utc, id
         LIMIT 1001"
    );
    let mut statement = store.database.connection().prepare(&sql)?;
    let runs = statement
        .query_map([], decode_run_row)?
        .map(|row| row.map_err(map_read_error))
        .collect::<Result<Vec<_>, _>>()?;
    if runs.len() > 1_000 {
        return Err(StoreError::Corrupt(
            "more than 1000 active runs need inspection".to_owned(),
        ));
    }
    Ok(runs)
}

fn latest_active_run(store: &JobStore, job_id: JobId) -> Result<Option<Run>, StoreError> {
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs
         WHERE job_id = ?1 AND state IN ('starting', 'running', 'cancel_requested')
         ORDER BY sequence DESC
         LIMIT 1"
    );
    store
        .database
        .connection()
        .query_row(&sql, [job_id.to_string()], decode_run_row)
        .optional()
        .map_err(map_read_error)
}

fn hide_job(
    store: &mut JobStore,
    job_id: JobId,
    expected_revision: Revision,
    keep_history: bool,
) -> Result<Job, StoreError> {
    let job = store.load(job_id)?.ok_or(StoreError::NotFound)?;
    let connection = store.database.connection_mut();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_write_error)?;
    // Default removal drops the completed-run history with the job;
    // --keep-history hides only. Transitions stay as the audit trail.
    if !keep_history {
        transaction
            .execute("DELETE FROM runs WHERE job_id = ?1", [job_id.to_string()])
            .map_err(map_write_error)?;
    }
    let changed = transaction
        .execute(
            "UPDATE jobs SET hidden = 1
             WHERE id = ?1 AND revision = ?2 AND hidden = 0
               AND state IN ('succeeded', 'failed', 'cancelled', 'interrupted', 'missed')",
            params![job_id.to_string(), revision_i64(expected_revision)?],
        )
        .map_err(map_write_error)?;
    if changed == 1 {
        transaction.commit().map_err(map_write_error)?;
        Ok(job)
    } else {
        // Transaction drops without commit: the history delete rolls back.
        Err(StoreError::Conflict)
    }
}

fn prepare_rerun(
    store: &mut JobStore,
    job_id: JobId,
    expected_revision: Revision,
    now: UtcTimestamp,
) -> Result<Job, StoreError> {
    let transaction = store
        .database
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_write_error)?;
    let current = transaction
        .query_row(
            "SELECT state FROM jobs
             WHERE id = ?1 AND revision = ?2 AND hidden = 0",
            params![job_id.to_string(), revision_i64(expected_revision)?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_read_error)?
        .ok_or(StoreError::Conflict)?;
    if !matches!(
        current.as_str(),
        "succeeded" | "failed" | "cancelled" | "interrupted" | "missed"
    ) {
        return Err(StoreError::Conflict);
    }
    let next_revision = expected_revision
        .next()
        .map_err(|error| StoreError::Domain(error.to_string()))?;
    transaction
        .execute(
            "UPDATE jobs SET
                revision = ?1, state = 'waiting',
                updated_at_utc = ?2, next_due_utc = ?2
             WHERE id = ?3 AND revision = ?4 AND state = ?5",
            params![
                revision_i64(next_revision)?,
                now.to_string(),
                job_id.to_string(),
                revision_i64(expected_revision)?,
                current,
            ],
        )
        .map_err(map_write_error)?;
    transaction
        .execute(
            "INSERT INTO transitions(
                job_id, run_id, from_state, to_state, occurred_at_utc,
                actor, reason, revision
             ) VALUES (?1, NULL, ?2, 'waiting', ?3, 'cli', ?4, ?5)",
            params![
                job_id.to_string(),
                current,
                now.to_string(),
                "explicit rerun requested",
                revision_i64(next_revision)?,
            ],
        )
        .map_err(map_write_error)?;
    transaction.commit().map_err(map_write_error)?;
    load_job(store.database.connection(), job_id)?.ok_or(StoreError::NotFound)
}

fn revision_i64(revision: Revision) -> Result<i64, StoreError> {
    i64::try_from(revision.get())
        .map_err(|_| StoreError::Corrupt("revision exceeds SQLite i64".to_owned()))
}

const fn encode_job_state(state: JobState) -> &'static str {
    match state {
        JobState::Scheduled => "scheduled",
        JobState::Waiting => "waiting",
        JobState::Starting => "starting",
        JobState::Running => "running",
        JobState::CancelRequested => "cancel_requested",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Interrupted => "interrupted",
        JobState::Missed => "missed",
    }
}

fn management_error(error: &StoreError) -> ManagementStoreError {
    ManagementStoreError(error.to_string())
}

impl RunOutputStore for JobStore {
    fn find_runs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<Run>, RunOutputStoreError> {
        let prefix = prefix.to_ascii_lowercase();
        let prefix_len = i64::try_from(prefix.len())
            .map_err(|_| output_store_error("prefix length overflow"))?;
        let limit =
            i64::try_from(limit).map_err(|_| output_store_error("result limit overflow"))?;
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs
             WHERE substr(id, 1, ?1) = ?2
             ORDER BY id
             LIMIT ?3"
        );
        let mut statement = self
            .database
            .connection()
            .prepare(&sql)
            .map_err(|error| output_store_error(&map_read_error(error).to_string()))?;
        let rows = statement
            .query_map(params![prefix_len, prefix, limit], decode_run_row)
            .map_err(|error| output_store_error(&map_read_error(error).to_string()))?;
        rows.map(|row| row.map_err(|error| output_store_error(&map_read_error(error).to_string())))
            .collect()
    }

    fn latest_run(&self, job_id: JobId) -> Result<Option<Run>, RunOutputStoreError> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs
             WHERE job_id = ?1
             ORDER BY sequence DESC
             LIMIT 1"
        );
        self.database
            .connection()
            .query_row(&sql, [job_id.to_string()], decode_run_row)
            .optional()
            .map_err(|error| output_store_error(&map_read_error(error).to_string()))
    }

    fn find_jobs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<Job>, RunOutputStoreError> {
        find_jobs_by_prefix(self, prefix, limit)
            .map_err(|error| output_store_error(&error.to_string()))
    }

    fn stdout_truncated(&self, run_id: RunId) -> Result<bool, RunOutputStoreError> {
        read_truncation_flag(self, run_id, "stdout_truncated")
    }

    fn stderr_truncated(&self, run_id: RunId) -> Result<bool, RunOutputStoreError> {
        read_truncation_flag(self, run_id, "stderr_truncated")
    }
}

fn read_truncation_flag(
    store: &JobStore,
    run_id: RunId,
    column: &str,
) -> Result<bool, RunOutputStoreError> {
    // NOTE: column comes only from the two literal call sites above.
    let sql = format!("SELECT {column} FROM runs WHERE id = ?1");
    store
        .database
        .connection()
        .query_row(&sql, [run_id.to_string()], |row| row.get(0))
        .optional()
        .map_err(|error| output_store_error(&map_read_error(error).to_string()))
        .and_then(|flag| flag.ok_or_else(|| output_store_error("run row vanished")))
}

fn output_store_error(message: &str) -> RunOutputStoreError {
    RunOutputStoreError(message.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use tempfile::tempdir;

    use super::super::job_store::tests::sample_job;
    use super::super::{Database, JobStore};
    use super::{Job, hide_job};

    fn run_row_count(store: &JobStore, job_id: String) -> i64 {
        store
            .database()
            .connection()
            .query_row(
                "SELECT count(*) FROM runs WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .expect("run count")
    }

    /// Fixture: persisted job with one claimed run, forced into a terminal
    /// state so `hide_job`'s state guard accepts it.
    fn terminal_job_with_run(store: &mut JobStore, seed: i64) -> Job {
        let job = sample_job(seed, seed + 30);
        store.create(&job).expect("create job");
        store
            .claim_run(
                job.id(),
                crate::domain::UtcTimestamp::from_second(seed + 30).expect("scheduled"),
                crate::domain::UtcTimestamp::from_second(seed + 1).expect("created"),
            )
            .expect("claim run");
        store
            .database()
            .connection()
            .execute(
                "UPDATE jobs SET state = 'succeeded' WHERE id = ?1",
                [job.id().to_string()],
            )
            .expect("force terminal");
        job
    }

    #[test]
    fn removal_drops_history_unless_kept() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);

        // Default removal hides the job and deletes its completed-run rows.
        let job = terminal_job_with_run(&mut store, 1_000);
        let hidden = hide_job(&mut store, job.id(), job.revision(), false).expect("hide");
        assert_eq!(run_row_count(&store, hidden.id().to_string()), 0);

        // --keep-history hides the job but keeps the run rows.
        let job = terminal_job_with_run(&mut store, 2_000);
        let hidden = hide_job(&mut store, job.id(), job.revision(), true).expect("hide");
        assert_eq!(run_row_count(&store, hidden.id().to_string()), 1);
    }
}
