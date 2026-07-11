//! Validated process-group cancellation.

use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use thiserror::Error;

use super::{IdentityStatus, NativeProcessInspector, ProcessError};
use crate::application::{GroupCancellation, ProcessCancellationError, ProcessGroupCanceller};
use crate::domain::{ProcessIdentitySnapshot, Run};

const POLL_INTERVAL: Duration = Duration::from_millis(5);

pub(crate) fn cancel_validated_group(
    inspector: &NativeProcessInspector,
    expected: &ProcessIdentitySnapshot,
    grace: Duration,
) -> Result<CancellationResult, CancellationError> {
    match inspector.classify(expected)? {
        IdentityStatus::Dead => return Ok(CancellationResult::AlreadyDead),
        IdentityStatus::Reused => return Ok(CancellationResult::IdentityChanged),
        IdentityStatus::Alive => {}
    }
    let group =
        Pid::from_raw(expected.process_group_id).ok_or(CancellationError::InvalidProcessGroup)?;
    if let Err(error) = kill_process_group(group, Signal::TERM) {
        if error == rustix::io::Errno::SRCH {
            return Ok(CancellationResult::AlreadyDead);
        }
        return Err(CancellationError::Signal(error));
    }

    let deadline = Instant::now()
        .checked_add(grace)
        .ok_or(CancellationError::InvalidGrace)?;
    loop {
        match inspector.classify(expected)? {
            IdentityStatus::Dead => return Ok(CancellationResult::TerminatedDuringGrace),
            IdentityStatus::Reused => return Ok(CancellationResult::IdentityChanged),
            IdentityStatus::Alive if Instant::now() >= deadline => break,
            IdentityStatus::Alive => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(POLL_INTERVAL.min(remaining));
            }
        }
    }

    match inspector.classify(expected)? {
        IdentityStatus::Dead => return Ok(CancellationResult::TerminatedDuringGrace),
        IdentityStatus::Reused => return Ok(CancellationResult::IdentityChanged),
        IdentityStatus::Alive => {}
    }
    match kill_process_group(group, Signal::KILL) {
        Ok(()) => Ok(CancellationResult::KilledAfterGrace),
        Err(error) if error == rustix::io::Errno::SRCH => {
            Ok(CancellationResult::TerminatedDuringGrace)
        }
        Err(error) => Err(CancellationError::Signal(error)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationResult {
    AlreadyDead,
    IdentityChanged,
    TerminatedDuringGrace,
    KilledAfterGrace,
}

pub(crate) struct NativeGroupCanceller<'a> {
    inspector: &'a NativeProcessInspector,
    grace: Duration,
}

impl<'a> NativeGroupCanceller<'a> {
    pub(crate) const fn new(inspector: &'a NativeProcessInspector, grace: Duration) -> Self {
        Self { inspector, grace }
    }
}

impl ProcessGroupCanceller for NativeGroupCanceller<'_> {
    fn cancel_group(&self, run: &Run) -> Result<GroupCancellation, ProcessCancellationError> {
        let identity = run
            .command_identity()
            .ok_or_else(|| ProcessCancellationError("command identity is missing".to_owned()))?;
        cancel_validated_group(self.inspector, identity, self.grace)
            .map(|result| match result {
                CancellationResult::AlreadyDead => GroupCancellation::AlreadyDead,
                CancellationResult::IdentityChanged => GroupCancellation::IdentityChanged,
                CancellationResult::TerminatedDuringGrace => {
                    GroupCancellation::TerminatedDuringGrace
                }
                CancellationResult::KilledAfterGrace => GroupCancellation::KilledAfterGrace,
            })
            .map_err(|error| ProcessCancellationError(error.to_string()))
    }
}

#[derive(Debug, Error)]
pub(crate) enum CancellationError {
    #[error(transparent)]
    Inspection(#[from] ProcessError),
    #[error("process group ID is invalid")]
    InvalidProcessGroup,
    #[error("cancellation grace interval is invalid")]
    InvalidGrace,
    #[error("sending a process-group signal failed: {0}")]
    Signal(rustix::io::Errno),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    use crate::application::ElapsedClock;
    use crate::domain::{Environment, ExecutionMode, ExecutionSpec};
    use crate::infrastructure::process::{
        CancellationResult, NativeProcessInspector, NativeProcessRunner, cancel_validated_group,
    };
    use crate::infrastructure::time::NativeClock;

    fn inspector() -> NativeProcessInspector {
        let clock = NativeClock;
        NativeProcessInspector::new(clock.boot_identity().expect("boot identity"))
    }

    fn shell(command: &str) -> ExecutionSpec {
        ExecutionSpec::new(
            ExecutionMode::Shell,
            vec![command.to_owned()],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution")
    }

    #[test]
    fn mismatched_and_dead_identities_are_never_signalled() {
        let inspector = inspector();
        let current = inspector
            .inspect(std::process::id())
            .expect("inspect")
            .expect("current process");
        let mut reused = current.clone();
        reused.start_token += 1;
        assert_eq!(
            cancel_validated_group(&inspector, &reused, Duration::from_millis(1))
                .expect("classification"),
            CancellationResult::IdentityChanged
        );

        let runner = NativeProcessRunner::new(inspector.clone());
        let child = runner.spawn(&shell("exit 0")).expect("spawn");
        let identity = child.identity().clone();
        child.wait_with_output().expect("wait");
        assert_eq!(
            cancel_validated_group(&inspector, &identity, Duration::from_millis(1))
                .expect("classification"),
            CancellationResult::AlreadyDead
        );
    }

    #[test]
    fn term_then_kill_reaches_the_whole_command_group() {
        let inspector = inspector();
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut child = runner
            .spawn(&shell("trap '' TERM; printf 'ready\\n'; sleep 30 & wait"))
            .expect("spawn");
        let (stdout, stderr) = child.take_output().expect("pipes");
        let mut stdout = BufReader::new(stdout);
        let mut ready = String::new();
        stdout.read_line(&mut ready).expect("ready line");
        assert_eq!(ready, "ready\n");
        let result =
            cancel_validated_group(&inspector, child.identity(), Duration::from_millis(30))
                .expect("cancel");
        assert_eq!(result, CancellationResult::KilledAfterGrace);
        drop((stdout, stderr));
        assert!(!child.wait().expect("wait").success());
    }
}
