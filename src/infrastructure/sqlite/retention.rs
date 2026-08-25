//! Retention queries and orphan discovery.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use jiff::SignedDuration;
use rusqlite::{TransactionBehavior, params};
use thiserror::Error;

use super::{JobStore, StoreError, map_write_error};
use crate::domain::{ClaimToken, RunId, UtcTimestamp};

const MAX_RETENTION_DAYS: u16 = 3_650;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionPolicy {
    history_days: u16,
    terminal_job_days: u16,
}

impl RetentionPolicy {
    pub(crate) const fn new(
        history_days: u16,
        terminal_job_days: u16,
    ) -> Result<Self, RetentionError> {
        if history_days == 0
            || history_days > MAX_RETENTION_DAYS
            || terminal_job_days == 0
            || terminal_job_days > MAX_RETENTION_DAYS
        {
            return Err(RetentionError::InvalidDays);
        }
        Ok(Self {
            history_days,
            terminal_job_days,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RetentionReport {
    pub(crate) runs: usize,
    pub(crate) jobs: usize,
    pub(crate) transitions: usize,
}

impl JobStore {
    pub(crate) fn record_log_truncation(
        &mut self,
        run_id: RunId,
        claim_token: ClaimToken,
        stdout_truncated: bool,
        stderr_truncated: bool,
    ) -> Result<(), StoreError> {
        let changed = self
            .database
            .connection()
            .execute(
                "UPDATE runs
                 SET stdout_truncated = ?1, stderr_truncated = ?2
                 WHERE id = ?3 AND claim_token = ?4",
                params![
                    stdout_truncated,
                    stderr_truncated,
                    run_id.to_string(),
                    claim_token.as_bytes().as_slice(),
                ],
            )
            .map_err(map_write_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::InvalidClaim)
        }
    }

    pub(crate) fn cleanup_retention(
        &mut self,
        now: UtcTimestamp,
        policy: RetentionPolicy,
    ) -> Result<RetentionReport, StoreError> {
        let history_cutoff = cutoff(now, policy.history_days)?;
        let job_cutoff = cutoff(now, policy.terminal_job_days)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let runs = transaction
            .execute(
                "DELETE FROM runs
                 WHERE state IN ('succeeded', 'failed', 'cancelled', 'interrupted')
                   AND finished_at_utc < ?1",
                [history_cutoff.as_str()],
            )
            .map_err(map_write_error)?;
        let transitions = transaction
            .execute(
                "DELETE FROM transitions WHERE occurred_at_utc < ?1",
                [history_cutoff.as_str()],
            )
            .map_err(map_write_error)?;
        let jobs = transaction
            .execute(
                "DELETE FROM jobs
                 WHERE state IN ('succeeded', 'failed', 'cancelled', 'interrupted', 'missed')
                   AND updated_at_utc < ?1",
                [job_cutoff.as_str()],
            )
            .map_err(map_write_error)?;
        transaction.commit().map_err(map_write_error)?;
        Ok(RetentionReport {
            runs,
            jobs,
            transitions,
        })
    }
}

fn cutoff(now: UtcTimestamp, days: u16) -> Result<String, StoreError> {
    let seconds = i64::from(days)
        .checked_mul(24 * 60 * 60)
        .ok_or_else(|| StoreError::Domain("retention interval overflow".to_owned()))?;
    now.as_jiff()
        .checked_sub(SignedDuration::from_secs(seconds))
        .map(|timestamp| timestamp.to_string())
        .map_err(|_| StoreError::Domain("retention cutoff overflow".to_owned()))
}

pub(crate) fn discover_orphan_artifacts(
    runs_directory: &Path,
    known_runs: &[RunId],
) -> Result<Vec<PathBuf>, RetentionError> {
    let known: HashSet<String> = known_runs.iter().map(ToString::to_string).collect();
    let mut orphans = Vec::new();
    for entry in fs::read_dir(runs_directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(RetentionError::NonUtf8Artifact)?;
        if !known.contains(name) {
            orphans.push(entry.path());
        }
    }
    orphans.sort_unstable();
    Ok(orphans)
}

#[derive(Debug, Error)]
pub(crate) enum RetentionError {
    #[error("retention days must be between 1 and 3650")]
    InvalidDays,
    #[error("artifact directory contains a non-UTF-8 name")]
    NonUtf8Artifact,
    #[error("artifact scan failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::time::Duration;

    use tempfile::tempdir;

    use rusqlite::params;

    use super::super::job_store::tests::sample_job;
    use super::super::{Database, JobStore};
    use super::{RetentionPolicy, discover_orphan_artifacts};
    use crate::domain::{JobState, RunId, UtcTimestamp};

    #[test]
    fn cleanup_covers_every_terminal_job_state_and_is_idempotent() {
        let root = tempdir().expect("temp root");
        let database = Database::open(&root.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        let mut store = JobStore::new(database);
        for (offset, state) in [
            (0_i64, JobState::Succeeded),
            (1, JobState::Failed),
            (2, JobState::Cancelled),
            (3, JobState::Interrupted),
            (4, JobState::Missed),
        ] {
            let job = sample_job(1_000 + offset, 2_000 + offset);
            store.create(&job).expect("job");
            let id = job.id().to_string();
            store
                .database()
                .connection()
                .execute(
                    "UPDATE jobs SET state = ?1, updated_at_utc = '1970-01-01T00:00:01Z'
                     WHERE id = ?2",
                    params![state_name(state), id],
                )
                .expect("terminal fixture");
        }
        let policy = RetentionPolicy::new(30, 30).expect("policy");
        let now = UtcTimestamp::from_second(4_000_000).expect("now");
        assert_eq!(
            store.cleanup_retention(now, policy).expect("cleanup").jobs,
            5
        );
        assert_eq!(
            store.cleanup_retention(now, policy).expect("repeat").jobs,
            0
        );
    }

    #[test]
    fn orphan_discovery_ignores_known_runs_and_flags_unknown_entries() {
        let root = tempdir().expect("temp root");
        let known = RunId::new();
        let orphan = RunId::new();
        fs::create_dir(root.path().join(known.to_string())).expect("known");
        fs::create_dir(root.path().join(orphan.to_string())).expect("orphan");
        fs::write(root.path().join("unexpected"), b"x").expect("unexpected");

        let found = discover_orphan_artifacts(root.path(), &[known]).expect("discover");
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|path| path.ends_with(orphan.to_string())));
        assert!(found.iter().any(|path| path.ends_with("unexpected")));
    }

    #[test]
    fn truncation_flags_are_idempotent_and_bad_log_paths_are_corrupt() {
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
        for _ in 0..2 {
            store
                .record_log_truncation(run.id(), run.claim_token(), true, false)
                .expect("record flags");
        }
        let stdout_truncated: bool = store
            .database()
            .connection()
            .query_row(
                "SELECT stdout_truncated FROM runs WHERE id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .expect("flag");
        assert!(stdout_truncated);

        store
            .database()
            .connection()
            .execute(
                "UPDATE runs SET stdout_path = '../victim' WHERE id = ?1",
                [run.id().to_string()],
            )
            .expect("corrupt path");
        assert!(matches!(
            store.load_run(run.id()),
            Err(super::super::StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn retention_policy_rejects_out_of_range_days() {
        for (history, terminal) in [(0, 30), (3_651, 30), (30, 0), (30, 3_651)] {
            assert!(RetentionPolicy::new(history, terminal).is_err());
        }
        assert!(RetentionPolicy::new(3_650, 3_650).is_ok());
    }

    #[test]
    fn truncation_flags_require_the_live_claim() {
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

        store
            .database()
            .connection()
            .execute(
                "UPDATE runs SET claim_token = ?1 WHERE id = ?2",
                params![vec![9_u8; 32], run.id().to_string()],
            )
            .expect("substitute token");
        assert!(matches!(
            store.record_log_truncation(run.id(), run.claim_token(), true, false),
            Err(super::super::StoreError::InvalidClaim)
        ));
    }

    fn state_name(state: JobState) -> &'static str {
        match state {
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Interrupted => "interrupted",
            JobState::Missed => "missed",
            _ => "invalid",
        }
    }
}
