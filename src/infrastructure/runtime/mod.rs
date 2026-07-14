//! Session runtime adapter.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::domain::RunId;
use crate::infrastructure::paths::{PathError, create_new_private_file, ensure_private_dir};

pub(crate) fn start_session_supervisor(
    state_directory: &Path,
    runtime_directory: &Path,
) -> Result<(), io::Error> {
    ensure_private_dir(state_directory).map_err(io::Error::other)?;
    ensure_private_dir(runtime_directory).map_err(io::Error::other)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state_directory.join("supervisor.log"))?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__supervisor")
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // SAFETY: `setsid` has no pointer preconditions and runs before other
    // threads exist in the new child image.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map(|_| ())
}

pub(crate) struct RunArtifacts {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_file: File,
    stderr_file: File,
}

impl RunArtifacts {
    pub(crate) fn stdout_path(&self) -> &Path {
        &self.stdout_path
    }

    pub(crate) fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }

    pub(crate) const fn stdout_file(&self) -> &File {
        &self.stdout_file
    }

    pub(crate) const fn stderr_file(&self) -> &File {
        &self.stderr_file
    }

    pub(crate) fn into_parts(self) -> (PathBuf, PathBuf, File, File) {
        (
            self.stdout_path,
            self.stderr_path,
            self.stdout_file,
            self.stderr_file,
        )
    }
}

pub(crate) fn create_run_artifacts(
    state_directory: &Path,
    run_id: RunId,
) -> Result<RunArtifacts, PathError> {
    ensure_private_dir(state_directory)?;
    let runs_directory = state_directory.join("runs");
    ensure_private_dir(&runs_directory)?;
    let job_directory = runs_directory.join(run_id.to_string());
    ensure_private_dir(&job_directory)?;
    let handle = File::open(&job_directory)?;
    let stdout_file = create_new_private_file(&handle, "stdout.log")?;
    let stderr_file = create_new_private_file(&handle, "stderr.log")?;
    let relative = PathBuf::from("runs").join(run_id.to_string());
    Ok(RunArtifacts {
        stdout_path: relative.join("stdout.log"),
        stderr_path: relative.join("stderr.log"),
        stdout_file,
        stderr_file,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::create_run_artifacts;
    use crate::domain::RunId;

    #[test]
    fn protected_log_paths_are_relative_and_owner_only() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        let artifacts = create_run_artifacts(root.path(), RunId::new()).expect("artifacts");
        assert!(!artifacts.stdout_path().is_absolute());
        assert!(!artifacts.stderr_path().is_absolute());
        assert_eq!(
            artifacts
                .stdout_file()
                .metadata()
                .expect("stdout metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn run_directory_symlink_is_rejected() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        let runs = root.path().join("runs");
        fs::create_dir(&runs).expect("runs");
        fs::set_permissions(&runs, fs::Permissions::from_mode(0o700)).expect("runs mode");
        let elsewhere = tempdir().expect("elsewhere");
        let id = RunId::new();
        symlink(elsewhere.path(), runs.join(id.to_string())).expect("malicious link");
        assert!(create_run_artifacts(root.path(), id).is_err());
    }
}
