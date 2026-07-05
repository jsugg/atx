//! `SQLite` job-store adapter.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use rustix::process::geteuid;
use thiserror::Error;

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;
const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("../../../migrations/0001_initial.sql"))];
const MAX_BUSY_TIMEOUT_MS: u128 = 2_147_483_647;

pub(crate) struct Database {
    connection: Connection,
}

impl Database {
    pub(crate) fn open(path: &Path, busy_timeout: Duration) -> Result<Self, StoreError> {
        let path = canonical_database_path(path)?;
        prepare_database_file(&path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(path, flags)?;
        configure_connection(&connection, busy_timeout)?;
        apply_migrations(&mut connection, MIGRATIONS, CURRENT_SCHEMA_VERSION)?;
        let database = Self { connection };
        database.verify_pragmas(busy_timeout)?;
        Ok(database)
    }

    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub(crate) fn schema_version(&self) -> Result<u32, StoreError> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StoreError::from)
    }

    pub(crate) fn pragmas(&self) -> Result<ConnectionPragmas, StoreError> {
        let foreign_keys = self
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?;
        let journal_mode = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
        let synchronous = self
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get::<_, u8>(0))?;
        let busy_timeout_ms_raw =
            self.connection
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))?;
        let busy_timeout_ms =
            u64::try_from(busy_timeout_ms_raw).map_err(|_| StoreError::PragmaMismatch)?;
        Ok(ConnectionPragmas {
            foreign_keys,
            journal_mode,
            synchronous,
            busy_timeout_ms,
        })
    }

    fn verify_pragmas(&self, busy_timeout: Duration) -> Result<(), StoreError> {
        let expected_timeout =
            u64::try_from(busy_timeout.as_millis()).map_err(|_| StoreError::InvalidBusyTimeout)?;
        let actual = self.pragmas()?;
        if !actual.foreign_keys
            || actual.journal_mode != "wal"
            || actual.synchronous != 2
            || actual.busy_timeout_ms != expected_timeout
        {
            return Err(StoreError::PragmaMismatch);
        }
        Ok(())
    }
}

fn canonical_database_path(path: &Path) -> Result<std::path::PathBuf, StoreError> {
    let parent = path.parent().ok_or(StoreError::InvalidDatabasePath)?;
    let name = path.file_name().ok_or(StoreError::InvalidDatabasePath)?;
    Ok(fs::canonicalize(parent)?.join(name))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionPragmas {
    pub(crate) foreign_keys: bool,
    pub(crate) journal_mode: String,
    pub(crate) synchronous: u8,
    pub(crate) busy_timeout_ms: u64,
}

fn prepare_database_file(path: &Path) -> Result<(), StoreError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != geteuid().as_raw()
                || metadata.permissions().mode() & 0o777 != 0o600
            {
                return Err(StoreError::InsecureDatabaseFile);
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn configure_connection(connection: &Connection, busy_timeout: Duration) -> Result<(), StoreError> {
    if busy_timeout.is_zero() || busy_timeout.as_millis() > MAX_BUSY_TIMEOUT_MS {
        return Err(StoreError::InvalidBusyTimeout);
    }
    connection.busy_timeout(busy_timeout)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn apply_migrations(
    connection: &mut Connection,
    migrations: &[(u32, &str)],
    target_version: u32,
) -> Result<(), StoreError> {
    let current: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > target_version {
        return Err(StoreError::NewerSchema {
            found: current,
            supported: target_version,
        });
    }
    if current == target_version {
        return Ok(());
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (version, sql) in migrations {
        if *version > current && *version <= target_version {
            transaction.execute_batch(sql)?;
        }
    }
    transaction.pragma_update(None, "user_version", target_version)?;
    transaction.commit()?;
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("database I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database file must be regular, owner-only, and owned by this user")]
    InsecureDatabaseFile,
    #[error("database path must have a parent and file name")]
    InvalidDatabasePath,
    #[error("database schema {found} is newer than supported schema {supported}")]
    NewerSchema { found: u32, supported: u32 },
    #[error("busy timeout is outside SQLite's supported range")]
    InvalidBusyTimeout,
    #[error("SQLite connection pragmas did not stick")]
    PragmaMismatch,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{CURRENT_SCHEMA_VERSION, Database, apply_migrations};

    #[test]
    fn schema_has_expected_constraints_and_indexes() {
        let root = tempdir().expect("temp root");
        let database =
            Database::open(&root.path().join("atx.db"), Duration::from_secs(5)).expect("database");
        let connection = database.connection();

        for table in ["metadata", "migrations", "jobs", "runs", "transitions"] {
            let count: u32 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("schema query");
            assert_eq!(count, 1, "{table}");
        }
        for index in [
            "jobs_state_due_idx",
            "runs_job_sequence_idx",
            "runs_state_idx",
            "transitions_job_revision_idx",
        ] {
            let count: u32 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name=?1",
                    [index],
                    |row| row.get(0),
                )
                .expect("index query");
            assert_eq!(count, 1, "{index}");
        }
        assert_eq!(database.schema_version().expect("schema version"), 1);
    }

    #[test]
    fn foreign_keys_and_connection_pragmas_are_verified() {
        let root = tempdir().expect("temp root");
        let database =
            Database::open(&root.path().join("atx.db"), Duration::from_secs(5)).expect("database");
        let pragmas = database.pragmas().expect("pragmas");
        assert!(pragmas.foreign_keys);
        assert_eq!(pragmas.journal_mode, "wal");
        assert_eq!(pragmas.synchronous, 2);
        assert_eq!(pragmas.busy_timeout_ms, 5_000);

        assert!(
            database
                .connection()
                .execute(
                    "INSERT INTO runs(id, job_id, sequence, scheduled_for_utc, created_at_utc, state, claim_token)
                     VALUES ('run', 'missing', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'starting', zeroblob(32))",
                    [],
                )
                .is_err()
        );
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_version() {
        let mut connection = Connection::open_in_memory().expect("memory database");
        let result = apply_migrations(
            &mut connection,
            &[
                (1, "CREATE TABLE first(value INTEGER);"),
                (2, "CREATE TABLE broken("),
            ],
            2,
        );
        assert!(result.is_err());
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 0);
        assert!(
            connection
                .query_row("SELECT count(*) FROM first", [], |row| row.get::<_, u32>(0))
                .is_err()
        );
    }

    #[test]
    fn newer_schema_is_rejected_without_changes() {
        let root = tempdir().expect("temp root");
        let path = root.path().join("atx.db");
        {
            let connection = Connection::open(&path).expect("seed database");
            connection
                .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
                .expect("newer version");
        }
        assert!(Database::open(&path, Duration::from_secs(5)).is_err());
        let connection = Connection::open(path).expect("reopen");
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }
}
