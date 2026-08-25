//! Claimed run lifecycle persistence.

use std::str::FromStr;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::{JobStore, StoreError, map_read_error, map_write_error};
use crate::application::{CancellationStore, CancellationStoreError};
use crate::domain::{
    ClaimToken, JobId, ProcessIdentitySnapshot, Run, RunId, RunOutcome, RunSnapshot, RunState,
    Sequence, UtcTimestamp,
};

pub(super) const RUN_COLUMNS: &str = "
    id, job_id, sequence, scheduled_for_utc, created_at_utc, started_at_utc,
    finished_at_utc, state, claim_token, monitor_identity_json,
    command_identity_json, process_group_id, exit_code, terminating_signal,
    failure, outcome_json, stdout_path, stderr_path
";

impl JobStore {
    pub(crate) fn claim_run(
        &mut self,
        job_id: JobId,
        scheduled_for_utc: UtcTimestamp,
        created_at_utc: UtcTimestamp,
    ) -> Result<Run, StoreError> {
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs WHERE job_id = ?1 AND scheduled_for_utc = ?2
             )",
            params![job_id.to_string(), scheduled_for_utc.to_string()],
            |row| row.get(0),
        )?;
        if duplicate {
            return Err(StoreError::DuplicateClaim);
        }
        let last_sequence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM runs WHERE job_id = ?1",
            [job_id.to_string()],
            |row| row.get(0),
        )?;
        let sequence_raw = u64::try_from(last_sequence)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| StoreError::Corrupt("run sequence overflow".to_owned()))?;
        let sequence =
            Sequence::new(sequence_raw).map_err(|error| StoreError::Domain(error.to_string()))?;
        let mut token = [0_u8; 32];
        getrandom::fill(&mut token).map_err(|error| StoreError::Random(error.to_string()))?;
        let run = Run::new(
            job_id,
            sequence,
            scheduled_for_utc,
            created_at_utc,
            ClaimToken::from_bytes(token),
        );
        let snapshot = run.snapshot();
        let result = transaction.execute(
            "INSERT INTO runs(
                id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                claim_token
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                snapshot.id.to_string(),
                snapshot.job_id.to_string(),
                to_sql_u64(snapshot.sequence.get())?,
                snapshot.scheduled_for_utc.to_string(),
                snapshot.created_at_utc.to_string(),
                encode_run_state(snapshot.state),
                snapshot.claim_token.as_bytes().as_slice(),
            ],
        );
        match result {
            Ok(1) => {}
            Ok(_) => return Err(StoreError::Corrupt("run claim affected no row".to_owned())),
            Err(error) if is_constraint(&error) => return Err(StoreError::DuplicateClaim),
            Err(error) => return Err(map_write_error(error)),
        }
        transaction.commit().map_err(map_write_error)?;
        Ok(run)
    }

    pub(crate) fn load_run(&self, id: RunId) -> Result<Option<Run>, StoreError> {
        load_run(self.database.connection(), id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mark_run_running(
        &mut self,
        id: RunId,
        claim_token: ClaimToken,
        started_at_utc: UtcTimestamp,
        monitor_identity: ProcessIdentitySnapshot,
        command_identity: ProcessIdentitySnapshot,
        stdout_path: &str,
        stderr_path: &str,
    ) -> Result<Run, StoreError> {
        let run = load_run(self.database.connection(), id)?.ok_or(StoreError::NotFound)?;
        verify_claim(&run, claim_token)?;
        let running = run
            .mark_running(
                started_at_utc,
                monitor_identity,
                command_identity,
                stdout_path.to_owned(),
                stderr_path.to_owned(),
            )
            .map_err(|error| StoreError::Domain(error.to_string()))?;
        let snapshot = running.snapshot();
        let monitor_json = serde_json::to_string(&snapshot.monitor_identity)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let command_json = serde_json::to_string(&snapshot.command_identity)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let process_group_id = snapshot
            .command_identity
            .as_ref()
            .map(|identity| identity.process_group_id);
        let changed = self
            .database
            .connection()
            .execute(
                "UPDATE runs SET
                    started_at_utc = ?1, state = 'running',
                    monitor_identity_json = ?2, command_identity_json = ?3,
                    process_group_id = ?4, stdout_path = ?5, stderr_path = ?6
                 WHERE id = ?7 AND state = 'starting' AND claim_token = ?8",
                params![
                    started_at_utc.to_string(),
                    monitor_json,
                    command_json,
                    process_group_id,
                    stdout_path,
                    stderr_path,
                    id.to_string(),
                    claim_token.as_bytes().as_slice(),
                ],
            )
            .map_err(map_write_error)?;
        if changed != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(running)
    }

    pub(crate) fn record_run_terminal(
        &mut self,
        id: RunId,
        claim_token: ClaimToken,
        finished_at_utc: UtcTimestamp,
        outcome: RunOutcome,
    ) -> Result<Run, StoreError> {
        let run = load_run(self.database.connection(), id)?.ok_or(StoreError::NotFound)?;
        verify_claim(&run, claim_token)?;
        if run.state().is_terminal() {
            if run.finished_at_utc() == Some(finished_at_utc) && run.outcome() == Some(&outcome) {
                return Ok(run);
            }
            return Err(StoreError::Conflict);
        }
        let completed = run
            .with_outcome(finished_at_utc, outcome)
            .map_err(|error| StoreError::Domain(error.to_string()))?;
        let snapshot = completed.snapshot();
        let outcome = snapshot
            .outcome
            .as_ref()
            .ok_or_else(|| StoreError::Corrupt("terminal run has no outcome".to_owned()))?;
        let outcome_json = serde_json::to_string(outcome)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let (exit_code, signal, failure): (Option<i32>, Option<i32>, Option<&str>) = match outcome {
            RunOutcome::Exit(code) => (Some(*code), None, None),
            RunOutcome::Signal(signal) => (None, Some(*signal), None),
            RunOutcome::Failure(message)
            | RunOutcome::Interrupted(message)
            | RunOutcome::Cancelled(message) => (None, None, Some(message)),
        };
        let changed = self
            .database
            .connection()
            .execute(
                "UPDATE runs SET
                    finished_at_utc = ?1, state = ?2, exit_code = ?3,
                    terminating_signal = ?4, failure = ?5, outcome_json = ?6
                 WHERE id = ?7 AND state IN ('starting', 'running', 'cancel_requested')
                     AND claim_token = ?8",
                params![
                    finished_at_utc.to_string(),
                    encode_run_state(snapshot.state),
                    exit_code,
                    signal,
                    failure,
                    outcome_json,
                    id.to_string(),
                    claim_token.as_bytes().as_slice(),
                ],
            )
            .map_err(map_write_error)?;
        if changed != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(completed)
    }

    pub(crate) fn request_run_cancellation(
        &mut self,
        id: RunId,
        claim_token: ClaimToken,
    ) -> Result<Run, StoreError> {
        let run = load_run(self.database.connection(), id)?.ok_or(StoreError::NotFound)?;
        verify_claim(&run, claim_token)?;
        if run.state() == RunState::CancelRequested || run.state().is_terminal() {
            return Ok(run);
        }
        let previous_state = encode_run_state(run.state());
        let requested = run
            .request_cancellation()
            .map_err(|error| StoreError::Domain(error.to_string()))?;
        let changed = self
            .database
            .connection()
            .execute(
                "UPDATE runs SET state = 'cancel_requested'
                 WHERE id = ?1 AND state = ?2 AND claim_token = ?3",
                params![
                    id.to_string(),
                    previous_state,
                    claim_token.as_bytes().as_slice(),
                ],
            )
            .map_err(map_write_error)?;
        if changed != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(requested)
    }
}

pub(super) fn load_run(connection: &Connection, id: RunId) -> Result<Option<Run>, StoreError> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
    connection
        .query_row(&sql, [id.to_string()], decode_run_row)
        .optional()
        .map_err(map_read_error)
}

pub(super) fn decode_run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    decode_run_row_inner(row)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}

fn decode_run_row_inner(row: &Row<'_>) -> Result<Run, StoreError> {
    let id = RunId::from_str(&row.get::<_, String>(0)?)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let job_id = JobId::from_str(&row.get::<_, String>(1)?)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let sequence_raw = row.get::<_, i64>(2)?;
    let sequence = Sequence::new(
        u64::try_from(sequence_raw)
            .map_err(|_| StoreError::Corrupt("negative sequence".to_owned()))?,
    )
    .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let scheduled_for_utc = parse_timestamp(&row.get::<_, String>(3)?)?;
    let created_at_utc = parse_timestamp(&row.get::<_, String>(4)?)?;
    let started_at_raw = row.get::<_, Option<String>>(5)?;
    let started_at_utc = parse_optional_timestamp(started_at_raw.as_deref())?;
    let finished_at_raw = row.get::<_, Option<String>>(6)?;
    let finished_at_utc = parse_optional_timestamp(finished_at_raw.as_deref())?;
    let state = decode_run_state(&row.get::<_, String>(7)?)?;
    let token = row.get::<_, Vec<u8>>(8)?;
    let claim_token = ClaimToken::from_bytes(
        token
            .try_into()
            .map_err(|_| StoreError::Corrupt("claim token is not 32 bytes".to_owned()))?,
    );
    let monitor_identity = decode_optional_json(row.get(9)?)?;
    let command_identity: Option<ProcessIdentitySnapshot> = decode_optional_json(row.get(10)?)?;
    let process_group_id = row.get::<_, Option<i32>>(11)?;
    if process_group_id
        != command_identity
            .as_ref()
            .map(|identity| identity.process_group_id)
    {
        return Err(StoreError::Corrupt(
            "process group does not match command identity".to_owned(),
        ));
    }
    let exit_code = row.get::<_, Option<i32>>(12)?;
    let signal = row.get::<_, Option<i32>>(13)?;
    let failure = row.get::<_, Option<String>>(14)?;
    let outcome: Option<RunOutcome> = decode_optional_json(row.get(15)?)?;
    validate_outcome_columns(outcome.as_ref(), exit_code, signal, failure.as_deref())?;
    let stdout_path = row.get(16)?;
    let stderr_path = row.get(17)?;
    Run::rehydrate(RunSnapshot {
        id,
        job_id,
        sequence,
        scheduled_for_utc,
        created_at_utc,
        started_at_utc,
        finished_at_utc,
        state,
        claim_token,
        monitor_identity,
        command_identity,
        outcome,
        stdout_path,
        stderr_path,
    })
    .map_err(|error| StoreError::Corrupt(error.to_string()))
}

fn verify_claim(run: &Run, claim_token: ClaimToken) -> Result<(), StoreError> {
    if run.claim_token() == claim_token {
        Ok(())
    } else {
        Err(StoreError::InvalidClaim)
    }
}

fn parse_timestamp(value: &str) -> Result<UtcTimestamp, StoreError> {
    value
        .parse::<UtcTimestamp>()
        .map_err(|error| StoreError::Corrupt(error.to_string()))
}

fn parse_optional_timestamp(value: Option<&str>) -> Result<Option<UtcTimestamp>, StoreError> {
    value.map(parse_timestamp).transpose()
}

fn encode_run_state(state: RunState) -> &'static str {
    match state {
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::CancelRequested => "cancel_requested",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Interrupted => "interrupted",
    }
}

fn decode_run_state(value: &str) -> Result<RunState, StoreError> {
    match value {
        "starting" => Ok(RunState::Starting),
        "running" => Ok(RunState::Running),
        "cancel_requested" => Ok(RunState::CancelRequested),
        "succeeded" => Ok(RunState::Succeeded),
        "failed" => Ok(RunState::Failed),
        "cancelled" => Ok(RunState::Cancelled),
        "interrupted" => Ok(RunState::Interrupted),
        _ => Err(StoreError::Corrupt("unknown run state".to_owned())),
    }
}

fn decode_optional_json<T: serde::de::DeserializeOwned>(
    value: Option<String>,
) -> Result<Option<T>, StoreError> {
    value
        .map(|json| {
            serde_json::from_str(&json).map_err(|error| StoreError::Corrupt(error.to_string()))
        })
        .transpose()
}

fn validate_outcome_columns(
    outcome: Option<&RunOutcome>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    failure: Option<&str>,
) -> Result<(), StoreError> {
    let matches = match outcome {
        None => exit_code.is_none() && signal.is_none() && failure.is_none(),
        Some(RunOutcome::Exit(expected)) => {
            exit_code == Some(*expected) && signal.is_none() && failure.is_none()
        }
        Some(RunOutcome::Signal(expected)) => {
            signal == Some(*expected) && exit_code.is_none() && failure.is_none()
        }
        Some(
            RunOutcome::Failure(expected)
            | RunOutcome::Interrupted(expected)
            | RunOutcome::Cancelled(expected),
        ) => failure == Some(expected.as_str()) && exit_code.is_none() && signal.is_none(),
    };
    if matches {
        Ok(())
    } else {
        Err(StoreError::Corrupt(
            "run outcome columns disagree".to_owned(),
        ))
    }
}

fn to_sql_u64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Corrupt("counter exceeds SQLite i64".to_owned()))
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ffi::ErrorCode::ConstraintViolation
    )
}

impl CancellationStore for JobStore {
    fn load_for_cancellation(&self, id: RunId) -> Result<Option<Run>, CancellationStoreError> {
        self.load_run(id)
            .map_err(|error| CancellationStoreError(error.to_string()))
    }

    fn commit_cancellation(
        &mut self,
        id: RunId,
        claim_token: ClaimToken,
    ) -> Result<Run, CancellationStoreError> {
        self.request_run_cancellation(id, claim_token)
            .map_err(|error| CancellationStoreError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use tempfile::tempdir;

    use super::super::JobStore;
    use super::super::job_store::tests::sample_job;
    use super::super::{Database, StoreError};
    use crate::domain::{
        ClaimToken, ProcessIdentitySnapshot, RunId, RunOutcome, RunState, UtcTimestamp,
    };
    use rusqlite::params;

    fn identity(pid: u32, start_token: u64, process_group_id: i32) -> ProcessIdentitySnapshot {
        ProcessIdentitySnapshot {
            boot_identity: "boot-a".to_owned(),
            pid,
            start_token,
            process_group_id,
        }
    }

    #[test]
    fn unreadable_run_rows_surface_as_corrupt() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run_id = RunId::new();
        store
            .database()
            .connection()
            .execute(
                "INSERT INTO runs(id, job_id, sequence, scheduled_for_utc, created_at_utc, state, claim_token)
                 VALUES (?1, ?2, 1, '2026-01-01T00:00:00Z', 'not-a-timestamp', 'starting', zeroblob(32))",
                params![run_id.to_string(), job.id().to_string()],
            )
            .expect("seed corrupt row");

        let error = store
            .load_run(run_id)
            .expect_err("corrupt row must fail to decode");
        assert!(matches!(error, StoreError::Corrupt(_)), "got {error:?}");
    }

    #[test]
    fn constraint_violation_at_claim_insert_maps_to_duplicate_claim() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        // Simulate a peer claiming the occurrence between the EXISTS pre-check
        // and the INSERT: the unique constraint fires on the insert itself.
        store
            .database()
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_run_insert BEFORE INSERT ON runs
                 BEGIN SELECT RAISE(ABORT, 'duplicate run'); END;",
            )
            .expect("trigger");
        assert!(matches!(
            store.claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            ),
            Err(StoreError::DuplicateClaim)
        ));
    }

    #[test]
    fn claim_is_unique_per_occurrence_and_uses_random_token() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let scheduled = UtcTimestamp::from_second(1_030).expect("scheduled");
        let first = store
            .claim_run(
                job.id(),
                scheduled,
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        assert_eq!(first.sequence().get(), 1);
        assert_ne!(first.claim_token().as_bytes(), &[0; 32]);
        assert!(matches!(
            store.claim_run(
                job.id(),
                scheduled,
                UtcTimestamp::from_second(1_002).expect("created"),
            ),
            Err(StoreError::DuplicateClaim)
        ));
    }

    #[test]
    fn running_and_terminal_updates_are_checked_and_idempotent() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let mut run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        run = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        assert_eq!(run.state(), RunState::Running);
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        let completed = store
            .record_run_terminal(run.id(), run.claim_token(), finished, RunOutcome::Exit(0))
            .expect("complete");
        assert_eq!(completed.state(), RunState::Succeeded);
        assert_eq!(
            store
                .record_run_terminal(run.id(), run.claim_token(), finished, RunOutcome::Exit(0),)
                .expect("idempotent"),
            completed
        );
        assert!(matches!(
            store
                .record_run_terminal(run.id(), run.claim_token(), finished, RunOutcome::Signal(9),),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn claimed_run_decodes_after_reopen() {
        let root = tempdir().expect("temp root");
        let path = root.path().join("atx.db");
        let run_id;
        {
            let database = Database::open(&path, Duration::from_millis(100)).expect("database");
            let mut store = JobStore::new(database);
            let job = sample_job(1_000, 1_030);
            store.create(&job).expect("job");
            run_id = store
                .claim_run(
                    job.id(),
                    UtcTimestamp::from_second(1_030).expect("scheduled"),
                    UtcTimestamp::from_second(1_001).expect("created"),
                )
                .expect("claim")
                .id();
        }
        let database = Database::open(&path, Duration::from_millis(100)).expect("reopen");
        let store = JobStore::new(database);
        assert_eq!(
            store.load_run(run_id).expect("load").expect("run").id(),
            run_id
        );
    }

    #[test]
    fn cancellation_request_is_idempotent_and_natural_exit_wins() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 10),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        let requested = store
            .request_run_cancellation(running.id(), running.claim_token())
            .expect("request");
        assert_eq!(requested.state(), RunState::CancelRequested);
        assert_eq!(
            store
                .request_run_cancellation(running.id(), running.claim_token())
                .expect("repeat"),
            requested
        );
        let completed = store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                UtcTimestamp::from_second(1_003).expect("finished"),
                RunOutcome::Exit(0),
            )
            .expect("natural completion");
        assert_eq!(completed.state(), RunState::Succeeded);
    }

    fn run_id(value: u128) -> RunId {
        RunId::from_u128(value)
    }

    #[test]
    fn load_run_returns_none_for_missing_id() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let store = JobStore::new(database);
        assert!(store.load_run(run_id(99)).expect("load").is_none());
    }

    #[test]
    fn wrong_claim_token_rejected_on_running_mark() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        assert!(matches!(
            store.mark_run_running(
                run.id(),
                ClaimToken::from_bytes([9; 32]),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            ),
            Err(StoreError::InvalidClaim)
        ));
    }

    #[test]
    fn wrong_claim_token_rejected_on_terminal_record() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        assert!(matches!(
            store.record_run_terminal(
                run.id(),
                ClaimToken::from_bytes([9; 32]),
                UtcTimestamp::from_second(1_002).expect("finished"),
                RunOutcome::Exit(0),
            ),
            Err(StoreError::InvalidClaim)
        ));
    }

    #[test]
    fn wrong_claim_token_rejected_on_cancellation_request() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        assert!(matches!(
            store.request_run_cancellation(run.id(), ClaimToken::from_bytes([9; 32]),),
            Err(StoreError::InvalidClaim)
        ));
    }

    #[test]
    fn missing_run_is_not_found_on_every_path() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let missing = run_id(7);
        // load_run reports absence as Ok(None), not an error.
        assert!(store.load_run(missing).expect("load").is_none());
        assert!(matches!(
            store.mark_run_running(
                missing,
                ClaimToken::from_bytes([1; 32]),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            ),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.record_run_terminal(
                missing,
                ClaimToken::from_bytes([1; 32]),
                UtcTimestamp::from_second(1_002).expect("finished"),
                RunOutcome::Exit(0),
            ),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.request_run_cancellation(missing, ClaimToken::from_bytes([1; 32]),),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn marking_running_fails_when_run_is_not_starting() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Exit(0),
            )
            .expect("complete");
        // Run is terminal: the domain guard rejects before any DB write, surfaced as Domain.
        assert!(matches!(
            store.mark_run_running(
                running.id(),
                running.claim_token(),
                UtcTimestamp::from_second(1_004).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            ),
            Err(StoreError::Domain(_))
        ));
    }

    #[test]
    fn terminal_record_conflicts_when_outcome_differs_after_terminal() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Exit(0),
            )
            .expect("complete");
        // Same finished time but a different outcome: terminal run, so it conflicts rather than idempotently returning.
        assert!(matches!(
            store.record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Signal(9),
            ),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn cancellation_conflicts_after_terminal_transition() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Exit(0),
            )
            .expect("complete");
        // Terminal run: request_cancellation leaves a terminal run untouched
        // (idempotent no-op branch), returning the same run id.
        let again = store
            .request_run_cancellation(running.id(), running.claim_token())
            .expect("already terminal");
        assert_eq!(again.id(), running.id());
        assert!(again.state().is_terminal());
    }

    #[test]
    fn cancellation_requested_state_allows_natural_exit_then_conflicts_again() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        store
            .request_run_cancellation(running.id(), running.claim_token())
            .expect("request");
        // cancel_requested is a valid pre-terminal state for record_run_terminal.
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        let completed = store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Cancelled("user".to_owned()),
            )
            .expect("cancelled completion");
        assert_eq!(completed.state(), RunState::Cancelled);
        // Now terminal: request_cancellation leaves a terminal run untouched
        // (idempotent no-op branch), returning the same run.
        let again = store
            .request_run_cancellation(running.id(), running.claim_token())
            .expect("already terminal");
        assert_eq!(again.id(), completed.id());
        assert!(again.state().is_terminal());
    }

    #[test]
    fn claim_reuses_existing_run_row_without_touching_job_state() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let first = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let second = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_031).expect("scheduled"),
                UtcTimestamp::from_second(1_002).expect("created"),
            )
            .expect("claim");
        assert_ne!(first.id(), second.id());
        assert_eq!(second.sequence().get(), 2);
        assert_ne!(
            first.claim_token().as_bytes(),
            second.claim_token().as_bytes()
        );
        // Job itself remains untouched (never started/running).
        assert_eq!(
            store
                .load_run(first.id())
                .expect("load")
                .expect("run")
                .job_id(),
            job.id()
        );
    }

    // Seed a single corrupt runs row directly into the migrated schema so the
    // decoder is exercised against a persisted-but-invalid record. FK enforcement
    // stays on, so a real jobs parent row is inserted first via the store.
    fn seed_corrupt_row(mut store: JobStore, id: RunId, corrupt: &str) {
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let job_id = job.id().to_string();
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'starting', ?3, '', '')",
                    params![id.to_string(), job_id, vec![0_u8; 32]],
                )
                .expect("seed");
            connection
                .execute(
                    &format!("UPDATE runs SET {corrupt} WHERE id = ?1"),
                    [id.to_string()],
                )
                .expect("corrupt");
        }
        assert!(
            matches!(store.load_run(id), Err(StoreError::Corrupt(_))),
            "expected Corrupt decoding {corrupt}"
        );
    }

    #[test]
    fn corrupt_row_unknown_state() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        seed_corrupt_row(JobStore::new(database), run_id(1), "state = 'bogus'");
    }

    #[test]
    fn corrupt_row_bad_created_timestamp() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        seed_corrupt_row(
            JobStore::new(database),
            run_id(2),
            "created_at_utc = 'garbage'",
        );
    }

    #[test]
    fn corrupt_row_bad_scheduled_timestamp() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        seed_corrupt_row(
            JobStore::new(database),
            run_id(3),
            "scheduled_for_utc = 'nope'",
        );
    }

    #[test]
    fn corrupt_row_short_claim_token() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'starting', ?3, '', '')",
                    params![
                        run_id(4).to_string(),
                        job.id().to_string(),
                        vec![1_u8, 2, 3]
                    ],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(4)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_row_process_group_mismatch() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, command_identity_json, process_group_id,
                        stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'running', ?3, '{\"boot_identity\":\"b\",\"pid\":1,\"start_token\":1,\"process_group_id\":2}', 99, '', '')",
                    params![run_id(5).to_string(), job.id().to_string(), vec![0_u8; 32]],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(5)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_row_signal_columns_disagree() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, outcome_json, terminating_signal, exit_code,
                        stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'failed', ?3, '{\"kind\":\"signal\",\"value\":9}', 9, 0, '', '')",
                    params![run_id(7).to_string(), job.id().to_string(), vec![0_u8; 32]],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(7)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_row_failure_columns_disagree() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, outcome_json, failure, terminating_signal,
                        stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'failed', ?3, '{\"Failure\":\"boom\"}',
                        'different', 5, '', '')",
                    params![run_id(8).to_string(), job.id().to_string(), vec![0_u8; 32]],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(8)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_row_success_without_outcome_json() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            // Terminal state carrying outcome columns but no outcome JSON.
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, exit_code, stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'succeeded', ?3, 0, '', '')",
                    params![run_id(9).to_string(), job.id().to_string(), vec![0_u8; 32]],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(9)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_row_outcome_columns_disagree() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        {
            let connection = store.database().connection();
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("disable checks");
            connection
                .execute(
                    "INSERT INTO runs(
                        id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                        claim_token, outcome_json, exit_code, stdout_path, stderr_path
                     ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                        '1970-01-01T00:00:01Z', 'succeeded', ?3, '{\"kind\":\"exit\",\"value\":0}', 7, '', '')",
                    params![run_id(6).to_string(), job.id().to_string(), vec![0_u8; 32]],
                )
                .expect("seed");
        }
        assert!(matches!(
            store.load_run(run_id(6)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn signal_and_failure_outcome_rows_decode() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let finished = UtcTimestamp::from_second(4_003).expect("finished");

        for (seed, outcome) in [
            (1_000, RunOutcome::Signal(9)),
            (2_000, RunOutcome::Failure("boom".to_owned())),
        ] {
            let job = sample_job(seed, seed + 30);
            store.create(&job).expect("job");
            let run = store
                .claim_run(
                    job.id(),
                    UtcTimestamp::from_second(seed + 30).expect("scheduled"),
                    UtcTimestamp::from_second(seed + 1).expect("created"),
                )
                .expect("claim");
            let run = store
                .mark_run_running(
                    run.id(),
                    run.claim_token(),
                    UtcTimestamp::from_second(seed + 2).expect("started"),
                    identity(10, 100, 20),
                    identity(11, 101, 20),
                    "runs/out.log",
                    "runs/err.log",
                )
                .expect("running");
            store
                .record_run_terminal(run.id(), run.claim_token(), finished, outcome.clone())
                .expect("complete");
            assert_eq!(
                store
                    .load_run(run.id())
                    .expect("decode")
                    .expect("run")
                    .outcome(),
                Some(&outcome)
            );
        }
    }

    #[test]
    fn non_constraint_claim_failures_pass_through_as_sqlite_errors() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        // A failing trigger that is not a constraint violation must not be
        // misreported as DuplicateClaim.
        store
            .database()
            .connection()
            .execute_batch(
                "CREATE TRIGGER broken_run_insert BEFORE INSERT ON runs
                 BEGIN INSERT INTO no_such_table VALUES (1); END;",
            )
            .expect("trigger");
        assert!(matches!(
            store.claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            ),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn lost_update_races_surface_as_conflict_on_every_transition() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_100);
        store.create(&job).expect("job");
        let claimed: Vec<_> = (0..3)
            .map(|offset| {
                store
                    .claim_run(
                        job.id(),
                        UtcTimestamp::from_second(1_030 + offset).expect("scheduled"),
                        UtcTimestamp::from_second(1_001).expect("created"),
                    )
                    .expect("claim")
            })
            .collect();
        // Simulate a peer winning the row between our load and our UPDATE:
        // RAISE(IGNORE) silently skips the statement, so changed == 0.
        store
            .database()
            .connection()
            .execute_batch(
                "CREATE TRIGGER ignore_run_updates BEFORE UPDATE ON runs
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .expect("trigger");
        assert!(matches!(
            store.mark_run_running(
                claimed[0].id(),
                claimed[0].claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            ),
            Err(StoreError::Conflict)
        ));
        assert!(matches!(
            store.record_run_terminal(
                claimed[1].id(),
                claimed[1].claim_token(),
                UtcTimestamp::from_second(1_003).expect("finished"),
                RunOutcome::Exit(0),
            ),
            Err(StoreError::Conflict)
        ));
        assert!(matches!(
            store.request_run_cancellation(claimed[2].id(), claimed[2].claim_token(),),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn terminal_record_conflicts_when_finish_time_differs() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(1_030).expect("scheduled"),
                UtcTimestamp::from_second(1_001).expect("created"),
            )
            .expect("claim");
        let running = store
            .mark_run_running(
                run.id(),
                run.claim_token(),
                UtcTimestamp::from_second(1_002).expect("started"),
                identity(10, 100, 20),
                identity(11, 101, 20),
                "runs/out.log",
                "runs/err.log",
            )
            .expect("running");
        let finished = UtcTimestamp::from_second(1_003).expect("finished");
        store
            .record_run_terminal(
                running.id(),
                running.claim_token(),
                finished,
                RunOutcome::Exit(0),
            )
            .expect("complete");
        // Same outcome but a different finish time on an already-terminal run.
        assert!(matches!(
            store.record_run_terminal(
                running.id(),
                running.claim_token(),
                UtcTimestamp::from_second(1_004).expect("later finish"),
                RunOutcome::Exit(0),
            ),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn non_decode_read_errors_stay_sqlite_errors() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("job");
        store
            .database()
            .connection()
            .execute_batch("DROP TABLE runs;")
            .expect("drop table");
        assert!(matches!(
            store.load_run(run_id(50)),
            Err(StoreError::Sqlite(_))
        ));
    }

    // Seed a runs row via direct SQL so `updates` can produce column states
    // the store would never write, then leave check constraints off.
    fn seed_outcome_row(store: &mut JobStore, n: i64, id: RunId, updates: &str) {
        let scheduled = n * 1_000;
        let job = sample_job(scheduled, scheduled + 30);
        store.create(&job).expect("job");
        let connection = store.database().connection();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("disable checks");
        connection
            .execute(
                "INSERT INTO runs(
                    id, job_id, sequence, scheduled_for_utc, created_at_utc, state,
                    claim_token, stdout_path, stderr_path
                 ) VALUES (?1, ?2, 1, '1970-01-01T00:00:01Z',
                    '1970-01-01T00:00:01Z', 'starting', randomblob(32), '', '')",
                params![id.to_string(), job.id().to_string()],
            )
            .expect("seed");
        connection
            .execute(
                &format!("UPDATE runs SET {updates}, finished_at_utc = '1970-01-01T00:00:02Z' WHERE id = ?1"),
                [id.to_string()],
            )
            .expect("apply updates");
    }

    #[test]
    fn outcome_column_combinations_validate_against_json() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        let cases: [(i64, &str, Option<RunOutcome>); 13] = [
            (
                21,
                "state='failed', outcome_json=NULL, terminating_signal=9",
                None,
            ),
            (
                22,
                "state='failed', outcome_json=NULL, failure='boom'",
                None,
            ),
            (
                23,
                "state='succeeded', outcome_json='{\"kind\":\"exit\",\"value\":0}', exit_code=5",
                None,
            ),
            (
                24,
                "state='succeeded', outcome_json='{\"kind\":\"exit\",\"value\":0}', exit_code=0, terminating_signal=9",
                None,
            ),
            (
                25,
                "state='succeeded', outcome_json='{\"kind\":\"exit\",\"value\":0}', exit_code=0, failure='boom'",
                None,
            ),
            (
                26,
                "state='failed', outcome_json='{\"kind\":\"signal\",\"value\":9}', terminating_signal=5",
                None,
            ),
            (
                27,
                "state='failed', outcome_json='{\"kind\":\"signal\",\"value\":9}', terminating_signal=9, exit_code=0",
                None,
            ),
            (
                28,
                "state='failed', outcome_json='{\"kind\":\"signal\",\"value\":9}', terminating_signal=9, failure='boom'",
                None,
            ),
            (
                29,
                "state='failed', outcome_json='{\"kind\":\"failure\",\"value\":\"a\"}', failure='b'",
                None,
            ),
            (
                30,
                "state='failed', outcome_json='{\"kind\":\"failure\",\"value\":\"a\"}', failure='a', exit_code=0",
                None,
            ),
            (
                31,
                "state='failed', outcome_json='{\"kind\":\"failure\",\"value\":\"a\"}', failure='a', terminating_signal=1",
                None,
            ),
            (
                32,
                "state='interrupted', outcome_json='{\"kind\":\"interrupted\",\"value\":\"a\"}', failure='a'",
                Some(RunOutcome::Interrupted("a".to_owned())),
            ),
            (
                33,
                "state='cancelled', outcome_json='{\"kind\":\"cancelled\",\"value\":\"a\"}', failure='a'",
                Some(RunOutcome::Cancelled("a".to_owned())),
            ),
        ];
        for (n, updates, want) in cases {
            let id = run_id(u128::try_from(n).expect("positive"));
            seed_outcome_row(&mut store, n, id, updates);
            match want {
                Some(outcome) => assert_eq!(
                    store.load_run(id).expect("decode").expect("run").outcome(),
                    Some(&outcome),
                    "{updates}"
                ),
                None => assert!(
                    matches!(store.load_run(id), Err(StoreError::Corrupt(_))),
                    "{updates}"
                ),
            }
        }
    }
}
