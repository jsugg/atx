//! `SQLite` job-store adapter.

mod job_cancel;
mod job_store;
mod management;
mod reconcile;
mod retention;
mod run_store;

pub(crate) use job_store::JobStore;
pub(crate) use reconcile::StartupStore;
pub(crate) use retention::RetentionPolicy;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use rustix::fs::{FlockOperation, flock};
use rustix::process::geteuid;
use thiserror::Error;

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 3;
pub(crate) const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../../../migrations/0001_initial.sql")),
    (2, include_str!("../../../migrations/0002_hidden_jobs.sql")),
    (
        3,
        include_str!("../../../migrations/0003_hidden_id_index.sql"),
    ),
];
const MAX_BUSY_TIMEOUT_MS: u128 = 2_147_483_647;

pub(crate) struct Database {
    connection: Connection,
}

impl Database {
    /// Open and initialize under a bounded directory lease without rebuilding existing corrupt data.
    pub(crate) fn open(path: &Path, busy_timeout: Duration) -> Result<Self, StoreError> {
        validate_busy_timeout(busy_timeout)?;
        let path = canonical_database_path(path)?;
        // The directory lease covers publication/configuration/migration, not later SQL writes.
        let _initialization = acquire_initialization(&path, busy_timeout)?;
        let created = prepare_database_file(&path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(path, flags)?;
        connection.busy_timeout(busy_timeout)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !created && version == 0 {
            return Err(StoreError::Corrupt(
                "existing database file carries no schema".to_owned(),
            ));
        }
        if version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                found: version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
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

fn validate_busy_timeout(timeout: Duration) -> Result<(), StoreError> {
    if timeout.is_zero() || timeout.as_millis() > MAX_BUSY_TIMEOUT_MS {
        return Err(StoreError::InvalidBusyTimeout);
    }
    Ok(())
}

fn remaining_initialization_budget(deadline: Instant) -> Result<Duration, StoreError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(StoreError::Busy)
}

/// Exclude peer openers before file creation, releasing the lease when the descriptor drops.
fn acquire_initialization(path: &Path, timeout: Duration) -> Result<File, StoreError> {
    acquire_initialization_with_wait(path, timeout, std::thread::sleep)
}

fn acquire_initialization_with_wait(
    path: &Path,
    timeout: Duration,
    mut wait: impl FnMut(Duration),
) -> Result<File, StoreError> {
    let parent = path.parent().ok_or(StoreError::InvalidDatabasePath)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)?;
    let held = directory.metadata()?;
    if !held.is_dir() || held.uid() != geteuid().as_raw() || held.permissions().mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
                "database initialization directory must be owned by this user and not writable by others",
        )
        .into());
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(StoreError::InvalidBusyTimeout)?;
    let mut waited = false;
    loop {
        if waited {
            remaining_initialization_budget(deadline)?;
        }
        match flock(&directory, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                let current = fs::symlink_metadata(parent)?;
                if !current.is_dir()
                    || current.dev() != held.dev()
                    || current.ino() != held.ino()
                    || current.uid() != held.uid()
                    || current.permissions().mode() & 0o022 != 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "database initialization directory changed while waiting",
                    )
                    .into());
                }
                return Ok(directory);
            }
            Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => {
                waited = true;
                let remaining = remaining_initialization_budget(deadline)?;
                let jitter = u64::try_from(remaining.as_nanos() % 11)
                    .map_err(|_| StoreError::InvalidBusyTimeout)?;
                wait(Duration::from_millis(10 + jitter).min(remaining));
            }
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
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
    validate_busy_timeout(busy_timeout)?;
    connection.busy_timeout(busy_timeout)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
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
    #[cfg(test)]
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

    use super::{CURRENT_SCHEMA_VERSION, Database, StoreError, apply_migrations, map_read_error};
    use crate::infrastructure::paths::private_tempdir as tempdir;

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
    fn peer_bootstrap_waits_for_the_creator_instead_of_declaring_corruption() {
        use rustix::fs::{FlockOperation, flock};
        use std::sync::mpsc;

        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        let lease = std::fs::File::open(root.path()).expect("initializer directory");
        flock(&lease, FlockOperation::NonBlockingLockExclusive).expect("creator lease");
        assert!(super::prepare_database_file(&path).expect("create database"));
        let (started, ready) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let peer_path = path.clone();
        let peer = std::thread::spawn(move || {
            started.send(()).expect("notify peer start");
            let result = Database::open(&peer_path, Duration::from_secs(5))
                .and_then(|database| database.schema_version());
            finished.send(result).expect("peer result");
        });
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("peer started");
        let early = completion.recv_timeout(Duration::from_millis(350));
        let mut creator = Connection::open(&path).expect("creator connection");
        super::configure_connection(&creator, Duration::from_secs(5)).expect("creator pragmas");
        apply_migrations(&mut creator, super::MIGRATIONS, CURRENT_SCHEMA_VERSION)
            .expect("creator migration");
        drop(creator);
        drop(lease);
        let result = match early {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => completion
                .recv_timeout(Duration::from_secs(5))
                .expect("peer completed"),
            Err(error) => {
                assert!(
                    matches!(error, mpsc::RecvTimeoutError::Timeout),
                    "peer disconnected: {error}"
                );
                completion
                    .recv_timeout(Duration::from_secs(5))
                    .expect("peer completed")
            }
        };
        peer.join().expect("peer joined");
        assert_eq!(
            result.expect("peer must not mistake a slow creator for corruption"),
            CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn held_initialization_lease_expires_without_changing_database_bytes() {
        use rustix::fs::{FlockOperation, flock};
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        drop(Database::open(&path, Duration::from_secs(5)).expect("initial database"));
        let before = std::fs::read(&path).expect("database bytes");
        let lease = std::fs::File::open(root.path()).expect("directory");
        flock(&lease, FlockOperation::NonBlockingLockExclusive).expect("held lease");
        let started = std::time::Instant::now();
        let error = Database::open(&path, Duration::from_millis(50))
            .err()
            .expect("lease must exclude a peer open");
        assert!(matches!(error, StoreError::Busy), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(std::fs::read(&path).expect("unchanged database"), before);
        drop(lease);
        Database::open(&path, Duration::from_secs(5)).expect("lease released");
    }

    #[test]
    fn invalid_initialization_budget_does_not_publish_an_empty_database() {
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        assert!(matches!(
            Database::open(&path, Duration::ZERO),
            Err(StoreError::InvalidBusyTimeout)
        ));
        assert!(
            !path.exists(),
            "an invalid budget poisoned a future first open"
        );
    }

    #[test]
    fn shared_default_timeout_matches_the_five_second_adr() {
        let root = tempdir().expect("root");
        assert_eq!(super::DATABASE_BUSY_TIMEOUT, Duration::from_secs(5));
        let database = Database::open(&root.path().join("atx.db"), super::DATABASE_BUSY_TIMEOUT)
            .expect("database");
        assert_eq!(database.pragmas().expect("pragmas").busy_timeout_ms, 5000);
    }

    #[test]
    fn uncontended_initialization_does_not_consume_the_sqlite_busy_wait_budget() {
        let root = tempdir().expect("root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_nanos(1))
            .expect("no lock contention requires no waiting budget");
        assert_eq!(
            database.schema_version().expect("schema"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(database.pragmas().expect("pragmas").busy_timeout_ms, 0);
    }

    #[test]
    fn concurrent_first_openers_observe_one_complete_schema() {
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let peers: Vec<_> = (0..8).map(|_| scope.spawn(|| {
                barrier.wait();
                let database = Database::open(&path, super::DATABASE_BUSY_TIMEOUT).expect("peer open");
                assert_eq!(database.schema_version().expect("schema"), CURRENT_SCHEMA_VERSION);
                assert_eq!(database.pragmas().expect("timeout").busy_timeout_ms, 5000);
                let migrations: u32 = database.connection().query_row("SELECT count(*) FROM migrations", [], |row| row.get(0)).expect("migration count");
                assert_eq!(migrations, 1);
                let latest_index: u32 = database.connection().query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='jobs_hidden_id_idx'",
                    [], |row| row.get(0),
                ).expect("latest migration index");
                assert_eq!(latest_index, 1);
            })).collect();
            for peer in peers {
                peer.join().expect("peer joined");
            }
        });
    }

    #[test]
    fn failed_initializer_releases_the_lease_but_is_not_rebuilt_as_fresh() {
        use rustix::fs::{FlockOperation, flock};
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        let lease = std::fs::File::open(root.path()).expect("directory");
        flock(&lease, FlockOperation::NonBlockingLockExclusive).expect("creator lease");
        assert!(super::prepare_database_file(&path).expect("created"));
        let mut connection = Connection::open(&path).expect("connection");
        super::configure_connection(&connection, Duration::from_secs(5)).expect("pragmas");
        assert!(
            apply_migrations(
                &mut connection,
                &[
                    (1, "CREATE TABLE first(value);"),
                    (2, "CREATE TABLE broken(")
                ],
                2
            )
            .is_err()
        );
        drop(connection);
        drop(lease);
        let before = std::fs::read(&path).expect("failed initializer bytes");
        assert!(matches!(
            Database::open(&path, Duration::from_secs(5)),
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(std::fs::read(&path).expect("preserved failure"), before);
    }

    #[test]
    fn schema_less_corruption_is_rejected_without_writing_a_sqlite_header() {
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        std::fs::File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("empty existing file");
        assert!(matches!(
            Database::open(&path, Duration::from_secs(5)),
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(std::fs::metadata(&path).expect("untouched file").len(), 0);
    }

    #[test]
    fn initialization_refuses_a_directory_writable_by_other_users() {
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777))
            .expect("loose directory");
        let result = Database::open(&path, Duration::from_secs(5));
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore private fixture");
        assert!(
            matches!(result, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert!(!path.exists());
    }

    #[test]
    fn initialization_refuses_a_group_writable_directory() {
        let root = tempdir().expect("root");
        let path = root.path().join("atx.db");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o775))
            .expect("group-writable directory");
        let result = Database::open(&path, Duration::from_secs(5));
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore private fixture");
        assert!(
            matches!(result, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert!(!path.exists());
    }

    #[test]
    fn initialization_rejects_directory_replacement_while_waiting() {
        use rustix::fs::{FlockOperation, flock};
        let parent = tempdir().expect("parent");
        let directory = parent.path().join("state");
        std::fs::create_dir(&directory).expect("state");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("private state");
        let moved = parent.path().join("old-state");
        let mut owner = Some(std::fs::File::open(&directory).expect("directory"));
        flock(
            owner.as_ref().expect("owner"),
            FlockOperation::NonBlockingLockExclusive,
        )
        .expect("owner lease");
        let mut changed = false;
        let result = super::acquire_initialization_with_wait(
            &directory.join("atx.db"),
            Duration::from_secs(5),
            |_| {
                assert!(!changed);
                std::fs::rename(&directory, &moved).expect("move locked directory");
                std::fs::create_dir(&directory).expect("replacement");
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .expect("private replacement");
                drop(owner.take());
                changed = true;
            },
        );
        assert!(
            changed,
            "replacement must occur after the blocked acquisition"
        );
        assert!(
            matches!(result, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
        );
        assert!(!directory.join("atx.db").exists());
        assert!(!moved.join("atx.db").exists());
    }

    #[test]
    fn a_late_lease_release_cannot_extend_an_expired_wait_budget() {
        use rustix::fs::{FlockOperation, flock};
        let root = tempdir().expect("root");
        let mut owner = Some(std::fs::File::open(root.path()).expect("directory"));
        flock(
            owner.as_ref().expect("owner"),
            FlockOperation::NonBlockingLockExclusive,
        )
        .expect("owner lease");
        let result = super::acquire_initialization_with_wait(
            &root.path().join("atx.db"),
            Duration::from_millis(30),
            |_| {
                std::thread::sleep(Duration::from_millis(60));
                drop(owner.take());
            },
        );
        assert!(
            matches!(result, Err(StoreError::Busy)),
            "late release was accepted after the wait deadline"
        );
        assert!(!root.path().join("atx.db").exists());
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
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: root bypasses directory write permissions");
            return;
        }
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
        assert!(matches!(error, StoreError::Sqlite(_)));
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
        let before = std::fs::read(&path).expect("newer database bytes");
        assert!(matches!(
            Database::open(&path, Duration::from_secs(5)),
            Err(super::StoreError::NewerSchema { found, supported })
                if found == CURRENT_SCHEMA_VERSION + 1
                    && supported == CURRENT_SCHEMA_VERSION
        ));
        assert_eq!(
            std::fs::read(&path).expect("unchanged newer database"),
            before
        );
        let connection = Connection::open(path).expect("reopen");
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }
}
