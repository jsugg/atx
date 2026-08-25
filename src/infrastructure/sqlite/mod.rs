//! `SQLite` job-store adapter.

mod job_store;
mod management;
mod reconcile;
mod retention;
mod run_store;

#[allow(unused_imports)]
pub(crate) use job_store::JobStore;
#[allow(unused_imports)]
pub(crate) use reconcile::StartupStore;
#[allow(unused_imports)]
pub(crate) use retention::RetentionPolicy;

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use rustix::process::geteuid;
use thiserror::Error;

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 3;
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../../../migrations/0001_initial.sql")),
    (2, include_str!("../../../migrations/0002_hidden_jobs.sql")),
    (
        3,
        include_str!("../../../migrations/0003_hidden_id_index.sql"),
    ),
];
const MAX_BUSY_TIMEOUT_MS: u128 = 2_147_483_647;
/// Attempts to observe a schema before declaring an existing empty DB corrupt.
const INIT_SCHEMA_ATTEMPTS: u32 = 10;
/// Delay between schema-observation retries while a peer initializes the DB.
const INIT_SCHEMA_RETRY_DELAY: Duration = Duration::from_millis(20);

pub(crate) struct Database {
    connection: Connection,
}

impl Database {
    pub(crate) fn open(path: &Path, busy_timeout: Duration) -> Result<Self, StoreError> {
        let path = canonical_database_path(path)?;
        let created = prepare_database_file(&path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(path, flags)?;
        configure_connection(&connection, busy_timeout)?;
        // A pre-existing file without a schema is truncation or corruption,
        // never a fresh install: refuse instead of silently rebuilding.
        // Exception: a peer process may be mid-initialization with its
        // migration transaction uncommitted, so retry briefly before
        // declaring the empty file corrupt.
        let mut attempts_left = INIT_SCHEMA_ATTEMPTS;
        while !created
            && connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? == 0
        {
            attempts_left -= 1;
            if attempts_left == 0 {
                return Err(StoreError::Corrupt(
                    "existing database file carries no schema".to_owned(),
                ));
            }
            std::thread::sleep(INIT_SCHEMA_RETRY_DELAY);
        }
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

fn prepare_database_file(path: &Path) -> Result<bool, StoreError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.sync_all()?;
            Ok(true)
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
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
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
    #[error("job already exists")]
    AlreadyExists,
    #[error("job was not found")]
    NotFound,
    #[error("job revision changed")]
    Conflict,
    #[error("page size must be between 1 and 100")]
    InvalidPageSize,
    #[error("database stayed busy past its configured timeout")]
    Busy,
    #[error("stored record is corrupt: {0}")]
    Corrupt(String),
    #[error("domain operation failed: {0}")]
    Domain(String),
    #[error("this job occurrence already has a run claim")]
    DuplicateClaim,
    #[error("run claim token did not match")]
    InvalidClaim,
    #[error("operating-system randomness failed: {0}")]
    Random(String),
}

fn map_write_error(error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked
            ) =>
        {
            StoreError::Busy
        }
        _ => StoreError::Sqlite(error),
    }
}

fn map_read_error(error: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::FromSqlConversionFailure(_, _, source) = &error {
        if let Some(StoreError::Corrupt(message)) = source.downcast_ref::<StoreError>() {
            return StoreError::Corrupt(message.clone());
        }
    }
    StoreError::Sqlite(error)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use std::os::unix::fs::PermissionsExt as _;

    use rusqlite::Connection;
    use rustix::fs::OpenOptionsExt as _;
    use tempfile::tempdir;

    use super::{CURRENT_SCHEMA_VERSION, Database, apply_migrations, map_read_error};

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
        assert_eq!(
            database.schema_version().expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
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
    fn truncated_preexisting_database_is_rejected_not_rebuilt() {
        let root = tempdir().expect("temp root");

        // A zero-byte pre-existing file passes the ownership/mode checks but
        // carries no schema: open must refuse instead of reinitializing.
        let path = root.path().join("atx.db");
        std::fs::File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("create empty owner-only file");
        let first = Database::open(&path, Duration::from_secs(5))
            .err()
            .expect("some error");
        assert!(
            matches!(first, super::StoreError::Corrupt(_)),
            "expected Corrupt, got {first:?}"
        );

        // A database that was properly initialized stays openable.
        Database::open(&root.path().join("fresh.db"), Duration::from_secs(5))
            .expect("initialize database");
        Database::open(&root.path().join("fresh.db"), Duration::from_secs(5))
            .expect("reopen initialized database");

        // A file wiped after initialization is corruption, not a fresh start.
        std::fs::write(&path, []).expect("truncate");
        assert!(matches!(
            Database::open(&path, Duration::from_secs(5)),
            Err(super::StoreError::Corrupt(_))
        ));
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
    fn insecure_preexisting_database_files_are_rejected() {
        let root = tempdir().expect("temp root");

        let link = root.path().join("link.db");
        std::os::unix::fs::symlink("/tmp", &link).expect("symlink");
        assert!(matches!(
            Database::open(&link, Duration::from_secs(5)),
            Err(super::StoreError::InsecureDatabaseFile)
        ));

        let loose = root.path().join("loose.db");
        std::fs::write(&loose, []).expect("seed file");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644))
            .expect("relax mode");
        assert!(matches!(
            Database::open(&loose, Duration::from_secs(5)),
            Err(super::StoreError::InsecureDatabaseFile)
        ));

        let directory = root.path().join("dir.db");
        std::fs::create_dir(&directory).expect("directory");
        assert!(matches!(
            Database::open(&directory, Duration::from_secs(5)),
            Err(super::StoreError::InsecureDatabaseFile)
        ));
    }

    #[test]
    fn zero_busy_timeout_is_rejected() {
        let root = tempdir().expect("temp root");
        assert!(matches!(
            Database::open(&root.path().join("atx.db"), Duration::ZERO),
            Err(super::StoreError::InvalidBusyTimeout)
        ));
    }

    #[test]
    fn oversized_busy_timeout_is_rejected() {
        let root = tempdir().expect("temp root");
        // One millisecond past SQLite's 32-bit busy-handler ceiling.
        assert!(matches!(
            Database::open(
                &root.path().join("atx.db"),
                Duration::from_millis(2_147_483_648)
            ),
            Err(super::StoreError::InvalidBusyTimeout)
        ));
    }

    #[test]
    fn pragma_drift_is_detected_by_verification() {
        let configured = || {
            let connection = Connection::open_in_memory().expect("memory database");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("busy timeout");
            connection
                .pragma_update(None, "foreign_keys", true)
                .expect("foreign keys");
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .expect("journal mode");
            connection
                .pragma_update(None, "synchronous", "FULL")
                .expect("synchronous");
            connection
        };

        for tamper in [
            "PRAGMA foreign_keys = OFF",
            "PRAGMA journal_mode = DELETE",
            "PRAGMA synchronous = NORMAL",
        ] {
            let connection = configured();
            connection.execute_batch(tamper).expect("tamper");
            assert!(
                matches!(
                    Database { connection }.verify_pragmas(Duration::from_secs(5)),
                    Err(super::StoreError::PragmaMismatch)
                ),
                "{tamper}"
            );
        }

        let connection = configured();
        connection
            .busy_timeout(Duration::from_millis(1_234))
            .expect("busy timeout");
        assert!(matches!(
            Database { connection }.verify_pragmas(Duration::from_secs(5)),
            Err(super::StoreError::PragmaMismatch)
        ));
    }

    #[test]
    fn unwritable_parent_directory_surfaces_as_io_error() {
        let root = tempdir().expect("temp root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555))
            .expect("lock directory");
        let result = Database::open(&root.path().join("atx.db"), Duration::from_secs(5));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .expect("unlock directory");
        assert!(matches!(result, Err(super::StoreError::Io(_))));
    }

    #[test]
    fn migrations_above_target_version_are_skipped() {
        let mut connection = Connection::open_in_memory().expect("memory database");
        apply_migrations(
            &mut connection,
            &[
                (1, "CREATE TABLE kept(value INTEGER);"),
                (2, "CREATE TABLE dropped(value INTEGER);"),
            ],
            1,
        )
        .expect("migrate to target");
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 1);
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='kept'",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("kept query"),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='dropped'",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("dropped query"),
            0
        );
    }

    #[test]
    fn migrations_resume_from_a_partially_applied_schema() {
        let mut connection = Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch("CREATE TABLE first(value INTEGER); PRAGMA user_version=1;")
            .expect("seed schema");

        apply_migrations(
            &mut connection,
            &[
                (1, "SELECT 1;"),
                (2, "CREATE TABLE second(value INTEGER);"),
                (3, "CREATE TABLE third(value INTEGER);"),
            ],
            3,
        )
        .expect("resume migrations");

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 3);
        for table in ["first", "second", "third"] {
            assert!(
                connection
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?1",
                        [table],
                        |row| row.get::<_, u32>(0),
                    )
                    .expect("table query")
                    == 1,
                "{table}"
            );
        }
    }

    #[test]
    fn non_store_conversion_failures_stay_sqlite_errors() {
        let error = map_read_error(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("not a store error")),
        ));
        assert!(
            error.to_string().starts_with("database operation failed"),
            "{error}"
        );
    }

    #[test]
    fn newer_schema_is_rejected_without_changes() {
        let root = tempdir().expect("temp root");
        let path = root.path().join("atx.db");
        // Seed with owner-only mode so the file passes the security checks and
        // open() actually reaches the migration version comparison.
        {
            let connection = Connection::open(&path).expect("seed database");
            connection
                .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
                .expect("newer version");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict mode");
        assert!(matches!(
            Database::open(&path, Duration::from_secs(5)),
            Err(super::StoreError::NewerSchema { found, supported })
                if found == CURRENT_SCHEMA_VERSION + 1
                    && supported == CURRENT_SCHEMA_VERSION
        ));
        let connection = Connection::open(path).expect("reopen");
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }
}
