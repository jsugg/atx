//! Claimed run lifecycle persistence.

use std::str::FromStr;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::{JobStore, StoreError, map_read_error, map_write_error};
use crate::domain::{
    ClaimToken, JobId, ProcessIdentitySnapshot, Run, RunId, RunOutcome, RunSnapshot, RunState,
    Sequence, UtcTimestamp,
};

const RUN_COLUMNS: &str = "
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
}

fn load_run(connection: &Connection, id: RunId) -> Result<Option<Run>, StoreError> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
    connection
        .query_row(&sql, [id.to_string()], decode_run_row)
        .optional()
        .map_err(map_read_error)
}

fn decode_run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use tempfile::tempdir;

    use super::super::JobStore;
    use super::super::job_store::tests::sample_job;
    use super::super::{Database, StoreError};
    use crate::domain::{ProcessIdentitySnapshot, RunOutcome, RunState, UtcTimestamp};

    fn identity(pid: u32, start_token: u64, process_group_id: i32) -> ProcessIdentitySnapshot {
        ProcessIdentitySnapshot {
            boot_identity: "boot-a".to_owned(),
            pid,
            start_token,
            process_group_id,
        }
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
}
