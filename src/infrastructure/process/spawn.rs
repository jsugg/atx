//! Isolated command spawning.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};

use thiserror::Error;

use super::{NativeProcessInspector, ProcessError};
use crate::domain::{ExecutionMode, ExecutionSpec, ProcessIdentitySnapshot};

#[derive(Clone, Debug)]
pub(crate) struct NativeProcessRunner {
    inspector: NativeProcessInspector,
}

impl NativeProcessRunner {
    pub(crate) const fn new(inspector: NativeProcessInspector) -> Self {
        Self { inspector }
    }

    pub(crate) fn spawn(&self, execution: &ExecutionSpec) -> Result<SpawnedChild, SpawnError> {
        let mut command = match execution.mode() {
            ExecutionMode::Direct => {
                let mut command = Command::new(&execution.argv()[0]);
                command.args(&execution.argv()[1..]);
                command
            }
            ExecutionMode::Shell => {
                let shell = execution
                    .shell_path()
                    .ok_or(SpawnError::InvalidShellConfiguration)?;
                let mut command = Command::new(shell);
                command.arg("-c").arg(&execution.argv()[0]);
                command
            }
        };
        command
            .current_dir(execution.working_directory())
            .env_clear()
            .envs(
                execution
                    .environment()
                    .iter()
                    .map(|(key, value)| (key, value.expose())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = command.spawn()?;
        let identity = match self.inspector.inspect(child.id()) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                reap_failed_spawn(&mut child);
                return Err(SpawnError::ExitedBeforeInspection);
            }
            Err(error) => {
                reap_failed_spawn(&mut child);
                return Err(SpawnError::Inspection(error));
            }
        };
        if i32::try_from(identity.pid) != Ok(identity.process_group_id) {
            reap_failed_spawn(&mut child);
            return Err(SpawnError::ProcessGroupSetup);
        }
        Ok(SpawnedChild { child, identity })
    }
}

fn reap_failed_spawn(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) struct SpawnedChild {
    child: Child,
    identity: ProcessIdentitySnapshot,
}

impl SpawnedChild {
    pub(crate) const fn identity(&self) -> &ProcessIdentitySnapshot {
        &self.identity
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(crate) fn wait_with_output(self) -> io::Result<Output> {
        self.child.wait_with_output()
    }
}

#[derive(Debug, Error)]
pub(crate) enum SpawnError {
    #[error("invalid shell execution configuration")]
    InvalidShellConfiguration,
    #[error("command spawn failed: {0}")]
    Io(#[from] io::Error),
    #[error("spawned command exited before identity inspection")]
    ExitedBeforeInspection,
    #[error("spawned command identity inspection failed: {0}")]
    Inspection(#[from] ProcessError),
    #[error("spawned command did not enter its dedicated process group")]
    ProcessGroupSetup,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::os::fd::AsRawFd;

    use tempfile::tempfile;

    use super::NativeProcessRunner;
    use crate::application::ElapsedClock;
    use crate::domain::{Environment, ExecutionMode, ExecutionSpec};
    use crate::infrastructure::process::NativeProcessInspector;
    use crate::infrastructure::time::NativeClock;

    fn runner() -> NativeProcessRunner {
        let clock = NativeClock;
        NativeProcessRunner::new(NativeProcessInspector::new(
            clock.boot_identity().expect("boot identity"),
        ))
    }

    fn execution(mode: ExecutionMode, arguments: &[&str], cwd: &str) -> ExecutionSpec {
        ExecutionSpec::new(
            mode,
            arguments.iter().map(ToString::to_string).collect(),
            cwd.to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution")
    }

    #[test]
    fn direct_mode_preserves_argv_without_interpretation() {
        let child = runner()
            .spawn(&execution(
                ExecutionMode::Direct,
                &["/bin/echo", "hello; printf hacked"],
                "/",
            ))
            .expect("spawn");
        assert_eq!(
            child.identity().process_group_id,
            i32::try_from(child.identity().pid).expect("PID fits")
        );
        let output = child.wait_with_output().expect("wait");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8"),
            "hello; printf hacked\n"
        );
    }

    #[test]
    fn shell_mode_uses_one_explicit_command_string() {
        let child = runner()
            .spawn(&execution(
                ExecutionMode::Shell,
                &["printf '%s' shell"],
                "/",
            ))
            .expect("spawn");
        let output = child.wait_with_output().expect("wait");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"shell");
    }

    #[test]
    fn invalid_executable_and_working_directory_are_reported() {
        assert!(
            runner()
                .spawn(&execution(
                    ExecutionMode::Direct,
                    &["/definitely/missing/atx-command"],
                    "/",
                ))
                .is_err()
        );
        assert!(
            runner()
                .spawn(&execution(
                    ExecutionMode::Direct,
                    &["/bin/echo"],
                    "/definitely/missing/atx-cwd",
                ))
                .is_err()
        );
    }

    #[test]
    fn ordinary_parent_descriptors_are_close_on_exec() {
        let file = tempfile().expect("temp file");
        let descriptor = file.as_raw_fd();
        let probe = format!("test ! -e /dev/fd/{descriptor}");
        let child = runner()
            .spawn(&execution(ExecutionMode::Shell, &[&probe], "/"))
            .expect("spawn");
        assert!(child.wait_with_output().expect("wait").status.success());
    }
}
