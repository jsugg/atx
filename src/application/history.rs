//! Run-history application service.

use std::path::Path;

use thiserror::Error;

use crate::domain::{JobId, RunId, RunOutcome, RunState};

/// One captured output stream plus its truncation flag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunStream {
    pub(crate) content: Vec<u8>,
    pub(crate) truncated: bool,
}

impl RunStream {
    pub(crate) const fn empty() -> Self {
        Self {
            content: Vec::new(),
            truncated: false,
        }
    }
}

/// Captured stdout and stderr of one completed run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunOutput {
    pub(crate) run_id: RunId,
    pub(crate) job_id: JobId,
    pub(crate) state: RunState,
    pub(crate) outcome: Option<RunOutcome>,
    pub(crate) stdout: RunStream,
    pub(crate) stderr: RunStream,
}

pub(crate) fn read_run_output<Store: RunOutputStore>(
    store: &Store,
    state_directory: &Path,
    prefix: &str,
) -> Result<RunOutput, RunOutputError> {
    if prefix.is_empty() || prefix.len() > 26 || !prefix.bytes().all(is_identifier_byte) {
        return Err(RunOutputError::InvalidPrefix);
    }
    let matches = store.find_runs_by_prefix(prefix, 2)?;
    let run = match matches.as_slice() {
        [] => {
            let job = resolve_job_for_output(store, prefix)?;
            match store.latest_run(job.id())? {
                Some(run) => run,
                None => return Err(RunOutputError::NoRuns),
            }
        }
        [run] => run.clone(),
        _ => return Err(RunOutputError::Ambiguous),
    };
    let snapshot = run.snapshot();
    if snapshot.state.is_terminal() && snapshot.stdout_path.is_none() {
        // Terminal runs without log paths (spawn failures) have no capture.
        return Ok(RunOutput {
            run_id: snapshot.id,
            job_id: snapshot.job_id,
            state: snapshot.state,
            outcome: snapshot.outcome,
            stdout: RunStream::empty(),
            stderr: RunStream::empty(),
        });
    }
    let (Some(stdout_path), Some(stderr_path)) = (snapshot.stdout_path, snapshot.stderr_path)
    else {
        return Err(RunOutputError::NotCaptured);
    };
    // NOTE: stored log paths are relative to the state directory (they start
    // with `runs/<run-id>/...`), not to the runs directory itself.
    // Canonicalize both sides: on macOS, temp roots live under paths with
    // symlinked ancestors (/var -> /private/var), and containment must
    // compare fully resolved forms.
    let resolved_runs_directory = state_directory
        .join("runs")
        .canonicalize()
        .map_err(|_| RunOutputError::MissingLogs)?;
    let resolve_log = |stored: &str| -> Result<std::path::PathBuf, RunOutputError> {
        state_directory
            .join(stored)
            .canonicalize()
            .map_err(|_| RunOutputError::MissingLogs)
    };
    let stdout = read_stream(
        &resolve_log(&stdout_path)?,
        &resolved_runs_directory,
        store.stdout_truncated(snapshot.id)?,
    )?;
    let stderr = read_stream(
        &resolve_log(&stderr_path)?,
        &resolved_runs_directory,
        store.stderr_truncated(snapshot.id)?,
    )?;
    Ok(RunOutput {
        run_id: snapshot.id,
        job_id: snapshot.job_id,
        state: snapshot.state,
        outcome: snapshot.outcome,
        stdout,
        stderr,
    })
}

fn resolve_job_for_output<Store: RunOutputStore>(
    store: &Store,
    prefix: &str,
) -> Result<crate::domain::Job, RunOutputError> {
    let jobs = store.find_jobs_by_prefix(prefix, 2)?;
    match jobs.as_slice() {
        [] => Err(RunOutputError::NotFound),
        [job] => Ok(job.clone()),
        _ => Err(RunOutputError::Ambiguous),
    }
}

/// Read one captured stream, rejecting anything outside the runs directory.
fn read_stream(
    path: &Path,
    runs_directory: &Path,
    truncated: bool,
) -> Result<RunStream, RunOutputError> {
    if !path.starts_with(runs_directory) {
        return Err(RunOutputError::MissingLogs);
    }
    let metadata = std::fs::metadata(path).map_err(|_| RunOutputError::MissingLogs)?;
    if !metadata.is_file() {
        return Err(RunOutputError::MissingLogs);
    }
    let content = std::fs::read(path).map_err(|error| RunOutputError::Read(error.to_string()))?;
    Ok(RunStream { content, truncated })
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

pub(crate) trait RunOutputStore {
    /// Find terminal and active runs whose ID starts with `prefix`.
    ///
    /// The limit bounds the scan so ambiguity is detected with at most two
    /// rows.
    fn find_runs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<crate::domain::Run>, RunOutputStoreError>;

    /// Latest run of a job by sequence.
    fn latest_run(&self, job_id: JobId) -> Result<Option<crate::domain::Run>, RunOutputStoreError>;

    fn find_jobs_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<crate::domain::Job>, RunOutputStoreError>;

    fn stdout_truncated(&self, run_id: RunId) -> Result<bool, RunOutputStoreError>;
    fn stderr_truncated(&self, run_id: RunId) -> Result<bool, RunOutputStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("run output storage failed: {0}")]
pub(crate) struct RunOutputStoreError(pub(crate) String);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum RunOutputError {
    #[error("run or job was not found")]
    NotFound,
    #[error("run or job prefix is ambiguous")]
    Ambiguous,
    #[error("prefix is not valid")]
    InvalidPrefix,
    #[error("job has no recorded runs")]
    NoRuns,
    #[error("output has not been captured yet")]
    NotCaptured,
    #[error("captured logs are missing from disk")]
    MissingLogs,
    #[error("reading captured logs failed: {0}")]
    Read(String),
    #[error(transparent)]
    Store(#[from] RunOutputStoreError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{RunOutputError, RunOutputStore, RunOutputStoreError, read_run_output};
    use crate::domain::{
        ClaimToken, Job, JobId, ProcessIdentitySnapshot, Run, RunOutcome, Sequence, UtcTimestamp,
    };

    struct Store {
        runs: Vec<Run>,
        jobs: Vec<Job>,
    }

    impl Store {
        fn with_run(run: Run) -> Self {
            Self {
                runs: vec![run],
                jobs: Vec::new(),
            }
        }
    }

    fn identity() -> ProcessIdentitySnapshot {
        ProcessIdentitySnapshot {
            boot_identity: "boot".to_owned(),
            pid: 10,
            start_token: 10,
            process_group_id: 10,
        }
    }

    fn running_run(stdout_path: &str, stderr_path: &str) -> Run {
        let timestamp = UtcTimestamp::from_second(1_000).expect("timestamp");
        let mut run = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            timestamp,
            timestamp,
            ClaimToken::from_bytes([7; 32]),
        );
        run = run
            .mark_running(
                UtcTimestamp::from_second(1_001).expect("started"),
                identity(),
                identity(),
                stdout_path.to_owned(),
                stderr_path.to_owned(),
            )
            .expect("running");
        run.with_outcome(
            UtcTimestamp::from_second(1_002).expect("finished"),
            RunOutcome::Exit(0),
        )
        .expect("terminal")
    }

    impl RunOutputStore for Store {
        fn find_runs_by_prefix(
            &self,
            prefix: &str,
            limit: usize,
        ) -> Result<Vec<Run>, RunOutputStoreError> {
            let text = prefix.to_owned();
            Ok(self
                .runs
                .iter()
                .filter(|run| run.id().to_string().starts_with(&text))
                .take(limit)
                .cloned()
                .collect())
        }

        fn latest_run(&self, _job_id: JobId) -> Result<Option<Run>, RunOutputStoreError> {
            Ok(self.runs.first().cloned())
        }

        fn find_jobs_by_prefix(
            &self,
            _prefix: &str,
            _limit: usize,
        ) -> Result<Vec<Job>, RunOutputStoreError> {
            Ok(self.jobs.clone())
        }

        fn stdout_truncated(
            &self,
            _run_id: crate::domain::RunId,
        ) -> Result<bool, RunOutputStoreError> {
            Ok(false)
        }

        fn stderr_truncated(
            &self,
            _run_id: crate::domain::RunId,
        ) -> Result<bool, RunOutputStoreError> {
            Ok(false)
        }
    }

    #[test]
    fn output_reads_captured_streams_from_disk() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let run = running_run("runs/x/stdout.log", "runs/x/stderr.log");
        let stdout_path = root.path().join("runs/x/stdout.log");
        fs::create_dir_all(stdout_path.parent().expect("parent")).expect("dir");
        fs::write(&stdout_path, b"hello").expect("write stdout");
        let stderr_path = root.path().join("runs/x/stderr.log");
        fs::write(&stderr_path, b"oops").expect("write stderr");

        let store = Store::with_run(run.clone());
        let output = read_run_output(&store, root.path(), &run.id().to_string()).expect("output");
        assert_eq!(output.stdout.content, b"hello");
        assert_eq!(output.stderr.content, b"oops");
        assert_eq!(output.outcome, Some(RunOutcome::Exit(0)));
    }

    #[test]
    fn unknown_and_ambiguous_prefixes_fail() {
        let store = Store {
            runs: Vec::new(),
            jobs: Vec::new(),
        };
        assert!(matches!(
            read_run_output(&store, std::path::Path::new("/nonexistent"), "zzzzzz"),
            Err(RunOutputError::NotFound)
        ));
        assert!(matches!(
            read_run_output(&store, std::path::Path::new("/nonexistent"), "!!"),
            Err(RunOutputError::InvalidPrefix)
        ));
    }

    #[test]
    fn missing_log_files_surface_as_missing_logs() {
        let root = tempdir().expect("root");
        let run = running_run("runs/gone/stdout.log", "runs/gone/stderr.log");
        let store = Store::with_run(run);
        assert!(matches!(
            read_run_output(&store, root.path(), "0"),
            Err(RunOutputError::MissingLogs)
        ));
    }

    #[test]
    fn output_prefixes_reject_empty_and_oversized_inputs() {
        let store = Store {
            runs: Vec::new(),
            jobs: Vec::new(),
        };
        let nowhere = std::path::Path::new("/nonexistent");
        assert!(matches!(
            read_run_output(&store, nowhere, ""),
            Err(RunOutputError::InvalidPrefix)
        ));
        let oversized = "a".repeat(27);
        assert!(matches!(
            read_run_output(&store, nowhere, oversized.as_str()),
            Err(RunOutputError::InvalidPrefix)
        ));
    }

    #[test]
    fn ambiguous_run_prefixes_are_rejected() {
        let first = running_run("runs/a/stdout.log", "runs/a/stderr.log");
        let second = running_run("runs/b/stdout.log", "runs/b/stderr.log");
        let store = Store {
            runs: vec![first, second],
            jobs: Vec::new(),
        };
        assert!(matches!(
            read_run_output(&store, std::path::Path::new("/nonexistent"), "0"),
            Err(RunOutputError::Ambiguous)
        ));
    }

    #[test]
    fn uncaptured_runs_and_directory_logs_are_rejected() {
        // A starting run has no log paths yet.
        let timestamp = UtcTimestamp::from_second(4_000).expect("timestamp");
        let fresh = Run::new(
            JobId::new(),
            Sequence::new(1).expect("sequence"),
            timestamp,
            timestamp,
            ClaimToken::from_bytes([7; 32]),
        );
        let store = Store::with_run(fresh);
        assert!(matches!(
            read_run_output(&store, std::path::Path::new("/nonexistent"), "0"),
            Err(RunOutputError::NotCaptured)
        ));

        // Terminal spawn failures record an outcome without any capture.
        let failed = Run::new(
            JobId::new(),
            Sequence::new(2).expect("sequence"),
            timestamp,
            timestamp,
            ClaimToken::from_bytes([7; 32]),
        )
        .with_outcome(
            UtcTimestamp::from_second(4_001).expect("finished"),
            RunOutcome::Failure("spawn failed".to_owned()),
        )
        .expect("terminal");
        let empty = read_run_output(
            &Store::with_run(failed),
            std::path::Path::new("/nowhere"),
            "0",
        )
        .expect("empty capture");
        assert_eq!(empty.stdout.content, b"");
        assert_eq!(empty.stderr.content, b"");

        // Log paths that resolve to directories are not readable files.
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("root mode");
        let run = running_run("runs/x/stdout.log", "runs/x/stderr.log");
        fs::create_dir_all(root.path().join("runs/x/stdout.log")).expect("stdout as dir");
        fs::create_dir_all(root.path().join("runs/x/stderr.log")).expect("stderr as dir");
        let dirs = Store::with_run(run);
        assert!(matches!(
            read_run_output(&dirs, root.path(), "0"),
            Err(RunOutputError::MissingLogs)
        ));
    }
}
