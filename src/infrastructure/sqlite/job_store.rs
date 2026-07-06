//! Transactional job persistence.

use std::str::FromStr;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{Database, StoreError, map_read_error, map_write_error};
use crate::domain::{
    Description, ExecutionSpec, Job, JobId, JobSnapshot, JobState, MissedPolicy, Name, Revision,
    Schedule, TransitionActor, UtcTimestamp,
};

const JOB_COLUMNS: &str = "
    id, revision, name, description, created_at_utc, updated_at_utc, state,
    runtime_tier, schedule_json, missed_policy, execution_json, next_due_utc,
    timezone_database_version, owner_uid
";
const MAX_PAGE_SIZE: usize = 100;

pub(crate) struct JobStore {
    pub(super) database: Database,
}

impl JobStore {
    pub(crate) const fn new(database: Database) -> Self {
        Self { database }
    }

    pub(crate) const fn database(&self) -> &Database {
        &self.database
    }

    pub(crate) fn create(&mut self, job: &Job) -> Result<(), StoreError> {
        let snapshot = job.snapshot();
        let schedule = serde_json::to_string(&snapshot.schedule)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let execution = snapshot
            .execution
            .to_persistence_json()
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let result = self.database.connection().execute(
            "INSERT INTO jobs(
                id, revision, name, description, created_at_utc, updated_at_utc,
                state, runtime_tier, schedule_json, missed_policy, execution_json,
                next_due_utc, timezone_database_version, owner_uid
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
             )",
            params![
                snapshot.id.to_string(),
                to_sql_u64(snapshot.revision.get())?,
                snapshot.name.as_ref().map(Name::as_str),
                snapshot.description.as_ref().map(Description::as_str),
                snapshot.created_at_utc.to_string(),
                snapshot.updated_at_utc.to_string(),
                encode_enum(snapshot.state)?,
                encode_enum(snapshot.runtime_tier)?,
                schedule,
                encode_enum(snapshot.missed_policy)?,
                execution,
                snapshot.next_due_utc.to_string(),
                snapshot.timezone_database_version,
                i64::from(snapshot.owner_uid),
            ],
        );
        match result {
            Ok(1) => Ok(()),
            Ok(_) => Err(StoreError::Corrupt("job insert affected no row".to_owned())),
            Err(error) if is_constraint(&error) => Err(StoreError::AlreadyExists),
            Err(error) => Err(map_write_error(error)),
        }
    }

    pub(crate) fn load(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        load_job(self.database.connection(), id)
    }

    pub(crate) fn list(&self, after: Option<JobId>, limit: usize) -> Result<Vec<Job>, StoreError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidPageSize);
        }
        let limit = i64::try_from(limit).map_err(|_| StoreError::InvalidPageSize)?;
        let connection = self.database.connection();
        let sql = match after {
            Some(_) => format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id > ?1 ORDER BY id LIMIT ?2"),
            None => format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id LIMIT ?1"),
        };
        let mut statement = connection.prepare(&sql)?;
        let rows = match after {
            Some(id) => statement.query_map(params![id.to_string(), limit], decode_job_row)?,
            None => statement.query_map([limit], decode_job_row)?,
        };
        rows.map(|row| row.map_err(map_read_error)).collect()
    }

    pub(crate) fn delete(
        &mut self,
        id: JobId,
        expected_revision: Revision,
    ) -> Result<(), StoreError> {
        let changed = self
            .database
            .connection()
            .execute(
                "DELETE FROM jobs WHERE id = ?1 AND revision = ?2",
                params![id.to_string(), to_sql_u64(expected_revision.get())?],
            )
            .map_err(map_write_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::Conflict)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn transition_job(
        &mut self,
        id: JobId,
        expected_revision: Revision,
        to: JobState,
        recurring: bool,
        actor: TransitionActor,
        reason: &str,
        now: UtcTimestamp,
    ) -> Result<Job, StoreError> {
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let mut job = load_job(&transaction, id)?.ok_or(StoreError::NotFound)?;
        if job.revision() != expected_revision {
            return Err(StoreError::Conflict);
        }
        let transition = job
            .transition(to, recurring, actor, reason, now)
            .map_err(|error| StoreError::Domain(error.to_string()))?;
        let changed = transaction
            .execute(
                "UPDATE jobs
                 SET revision = ?1, state = ?2, updated_at_utc = ?3
                 WHERE id = ?4 AND revision = ?5",
                params![
                    to_sql_u64(job.revision().get())?,
                    encode_enum(job.state())?,
                    now.to_string(),
                    id.to_string(),
                    to_sql_u64(expected_revision.get())?,
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
                    id.to_string(),
                    encode_enum(transition.from())?,
                    encode_enum(transition.to())?,
                    now.to_string(),
                    encode_enum(transition.actor())?,
                    transition.reason(),
                    to_sql_u64(job.revision().get())?,
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit().map_err(map_write_error)?;
        Ok(job)
    }
}

fn load_job(connection: &Connection, id: JobId) -> Result<Option<Job>, StoreError> {
    let sql = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1");
    connection
        .query_row(&sql, [id.to_string()], decode_job_row)
        .optional()
        .map_err(map_read_error)
}

fn decode_job_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    decode_job_row_inner(row)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}

fn decode_job_row_inner(row: &Row<'_>) -> Result<Job, StoreError> {
    let id = JobId::from_str(&row.get::<_, String>(0)?)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let revision_raw = row.get::<_, i64>(1)?;
    let revision = Revision::new(
        u64::try_from(revision_raw)
            .map_err(|_| StoreError::Corrupt("negative revision".to_owned()))?,
    )
    .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let name = row
        .get::<_, Option<String>>(2)?
        .map(Name::new)
        .transpose()
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let description = row
        .get::<_, Option<String>>(3)?
        .map(Description::new)
        .transpose()
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let created_at_raw = row.get::<_, String>(4)?;
    let created_at_utc = parse_timestamp(&created_at_raw)?;
    let updated_at_raw = row.get::<_, String>(5)?;
    let updated_at_utc = parse_timestamp(&updated_at_raw)?;
    let state = decode_enum(&row.get::<_, String>(6)?)?;
    let runtime_tier = decode_enum(&row.get::<_, String>(7)?)?;
    let schedule: Schedule = serde_json::from_str(&row.get::<_, String>(8)?)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let missed_policy: MissedPolicy = decode_enum(&row.get::<_, String>(9)?)?;
    let execution = ExecutionSpec::from_persistence_json(&row.get::<_, String>(10)?)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let next_due_raw = row.get::<_, String>(11)?;
    let next_due_utc = parse_timestamp(&next_due_raw)?;
    let timezone_database_version = row.get(12)?;
    let owner_uid_raw = row.get::<_, i64>(13)?;
    let owner_uid = u32::try_from(owner_uid_raw)
        .map_err(|_| StoreError::Corrupt("owner UID is outside u32".to_owned()))?;
    Job::rehydrate(JobSnapshot {
        id,
        revision,
        name,
        description,
        created_at_utc,
        updated_at_utc,
        state,
        runtime_tier,
        schedule,
        missed_policy,
        execution,
        next_due_utc,
        timezone_database_version,
        owner_uid,
    })
    .map_err(|error| StoreError::Corrupt(error.to_string()))
}

fn parse_timestamp(value: &str) -> Result<UtcTimestamp, StoreError> {
    value
        .parse::<UtcTimestamp>()
        .map_err(|error| StoreError::Corrupt(error.to_string()))
}

fn encode_enum<T: Serialize>(value: T) -> Result<String, StoreError> {
    let encoded =
        serde_json::to_value(value).map_err(|error| StoreError::Corrupt(error.to_string()))?;
    encoded
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Corrupt("enum did not serialize as text".to_owned()))
}

fn decode_enum<T: DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    let encoded =
        serde_json::to_string(value).map_err(|error| StoreError::Corrupt(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(|error| StoreError::Corrupt(error.to_string()))
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
pub(super) mod tests {
    #![allow(clippy::expect_used)]

    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::super::{Database, StoreError};
    use super::JobStore;
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobState, MissedPolicy,
        RuntimeTier, Schedule, TransitionActor, UtcTimestamp,
    };

    pub(crate) fn sample_job(now: i64, due: i64) -> Job {
        let now = UtcTimestamp::from_second(now).expect("valid timestamp");
        let due = UtcTimestamp::from_second(due).expect("valid timestamp");
        let schedule =
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("duration"), due);
        let execution = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["printf".to_owned(), "%s".to_owned(), "hello".to_owned()],
            "/tmp".to_owned(),
            Environment::from_pairs([("TOKEN", "stored-secret")]).expect("environment"),
        )
        .expect("execution");
        Job::new(
            now,
            schedule,
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("job")
    }

    fn store() -> (tempfile::TempDir, JobStore) {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        (root, JobStore::new(database))
    }

    #[test]
    fn crud_and_cursor_pagination_round_trip_validated_jobs() {
        let (_root, mut store) = store();
        let jobs = [
            sample_job(1_000, 1_030),
            sample_job(2_000, 2_030),
            sample_job(3_000, 3_030),
        ];
        for job in &jobs {
            store.create(job).expect("create");
        }

        let first = store.list(None, 2).expect("first page");
        assert_eq!(first.len(), 2);
        let second = store.list(Some(first[1].id()), 2).expect("second page");
        assert_eq!(second.len(), 1);
        for job in &jobs {
            let loaded = store.load(job.id()).expect("load").expect("present");
            assert_eq!(&loaded, job);
        }

        store
            .delete(jobs[0].id(), jobs[0].revision())
            .expect("delete");
        assert!(store.load(jobs[0].id()).expect("load").is_none());
        assert!(store.list(None, 0).is_err());
        assert!(store.list(None, 101).is_err());
    }

    #[test]
    fn transition_and_history_commit_with_revision_cas() {
        let (_root, mut store) = store();
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("create");
        let changed = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "supervisor accepted job",
                UtcTimestamp::from_second(1_001).expect("timestamp"),
            )
            .expect("transition");
        assert_eq!(changed.revision().get(), 2);
        assert_eq!(changed.state(), JobState::Waiting);
        assert!(matches!(
            store.transition_job(
                job.id(),
                job.revision(),
                JobState::CancelRequested,
                false,
                TransitionActor::Cli,
                "stale request",
                UtcTimestamp::from_second(1_002).expect("timestamp"),
            ),
            Err(StoreError::Conflict)
        ));

        let history: u32 = store
            .database()
            .connection()
            .query_row("SELECT count(*) FROM transitions", [], |row| row.get(0))
            .expect("history count");
        assert_eq!(history, 1);
    }

    #[test]
    fn corrupt_records_return_typed_errors() {
        let (_root, mut store) = store();
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("create");
        store
            .database()
            .connection()
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable checks");
        store
            .database()
            .connection()
            .execute("UPDATE jobs SET state = 'teleported'", [])
            .expect("corrupt record");
        assert!(matches!(store.load(job.id()), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn history_failure_rolls_back_job_update() {
        let (_root, mut store) = store();
        let job = sample_job(1_000, 1_030);
        store.create(&job).expect("create");
        store
            .database()
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_history BEFORE INSERT ON transitions
                 BEGIN SELECT RAISE(ABORT, 'no history'); END;",
            )
            .expect("trigger");
        assert!(
            store
                .transition_job(
                    job.id(),
                    job.revision(),
                    JobState::Waiting,
                    false,
                    TransitionActor::Supervisor,
                    "must roll back",
                    UtcTimestamp::from_second(1_001).expect("timestamp"),
                )
                .is_err()
        );
        assert_eq!(
            store.load(job.id()).expect("load").expect("job").revision(),
            job.revision()
        );
    }

    #[test]
    fn busy_wait_is_bounded_by_connection_timeout() {
        let root = tempdir().expect("temp root");
        let path = root.path().join("atx.db");
        let mut blocker = Database::open(&path, Duration::from_millis(25)).expect("blocker");
        let mut contender =
            JobStore::new(Database::open(&path, Duration::from_millis(25)).expect("contender"));
        blocker
            .connection_mut()
            .execute_batch("BEGIN IMMEDIATE")
            .expect("write lock");
        let start = Instant::now();
        let result = contender.create(&sample_job(1_000, 1_030));
        assert!(matches!(result, Err(StoreError::Busy)));
        assert!(start.elapsed() < Duration::from_secs(1));
        blocker
            .connection_mut()
            .execute_batch("ROLLBACK")
            .expect("release lock");
    }
}
