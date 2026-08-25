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

    use crate::application::{ElapsedClock, ProcessGroupCanceller};
    use crate::domain::{Environment, ExecutionMode, ExecutionSpec, ProcessIdentitySnapshot};
    use crate::infrastructure::process::{
        CancellationResult, NativeGroupCanceller, NativeProcessInspector, NativeProcessRunner,
        cancel_validated_group,
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

    #[test]
    fn already_dead_identity_short_circuits_before_signalling() {
        let inspector = inspector();
        let dead = ProcessIdentitySnapshot {
            boot_identity: "boot-a".to_owned(),
            pid: 4242,
            start_token: 999,
            process_group_id: 4242,
        };
        assert_eq!(
            cancel_validated_group(&inspector, &dead, Duration::from_millis(1))
                .expect("classification"),
            CancellationResult::AlreadyDead
        );
    }

    #[test]
    fn process_group_canceller_maps_results_and_rejects_missing_identity() {
        use crate::domain::Run;

        let inspector = inspector();
        let runner = NativeProcessRunner::new(inspector.clone());
        let mut child = runner
            .spawn(&shell("trap '' TERM; printf 'ready\\n'; sleep 30 & wait"))
            .expect("spawn");
        let (stdout, stderr) = child.take_output().expect("pipes");
        let mut reader = BufReader::new(stdout);
        let mut ready = String::new();
        reader.read_line(&mut ready).expect("ready line");
        assert_eq!(ready, "ready\n");
        let canceller = NativeGroupCanceller::new(&inspector, Duration::from_millis(30));
        let mut run = child_to_run(&child);
        let outcome = canceller.cancel_group(&run).expect("cancel");
        assert_eq!(
            outcome,
            crate::application::GroupCancellation::KilledAfterGrace
        );
        drop((reader, stderr));
        assert!(!child.wait().expect("wait").success());

        // A run with no recorded command identity is not cancellable.
        run = Run::new(
            crate::domain::JobId::new(),
            crate::domain::Sequence::new(1).expect("sequence"),
            crate::domain::UtcTimestamp::from_second(1_784_204_100).expect("ts"),
            crate::domain::UtcTimestamp::from_second(1_784_204_100).expect("ts"),
            crate::domain::ClaimToken::from_bytes([3; 32]),
        );
        assert!(canceller.cancel_group(&run).is_err());
    }

    /// Package a live child's identity into a minimal run, deterministically.
    fn child_to_run(child: &crate::infrastructure::process::SpawnedChild) -> crate::domain::Run {
        use crate::domain::{ClaimToken, Run};
        Run::new(
            crate::domain::JobId::new(),
            crate::domain::Sequence::new(1).expect("sequence"),
            crate::domain::UtcTimestamp::from_second(1_784_204_100).expect("ts"),
            crate::domain::UtcTimestamp::from_second(1_784_204_100).expect("ts"),
            ClaimToken::from_bytes([3; 32]),
        )
        .mark_running(
            crate::domain::UtcTimestamp::from_second(1_784_204_100).expect("ts"),
            child.identity().clone(),
            child.identity().clone(),
            "stdout.log".to_owned(),
            "stderr.log".to_owned(),
        )
        .expect("running")
    }
}
