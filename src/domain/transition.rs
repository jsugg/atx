//! Legal state transitions.

use serde::Serialize;
use thiserror::Error;

use super::state::{JobState, RunState};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransitionActor {
    Cli,
    Supervisor,
    Monitor,
    Recovery,
    Retention,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Transition<S> {
    from: S,
    to: S,
    actor: TransitionActor,
    reason: String,
}

impl<S: Copy> Transition<S> {
    pub(crate) const fn from(&self) -> S {
        self.from
    }

    pub(crate) const fn to(&self) -> S {
        self.to
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }

    pub(crate) const fn actor(&self) -> TransitionActor {
        self.actor
    }
}

fn transition<S: Copy>(
    from: S,
    to: S,
    actor: TransitionActor,
    reason: &str,
    legal: bool,
) -> Result<Transition<S>, TransitionError> {
    if !legal {
        return Err(TransitionError::Illegal);
    }
    if reason.is_empty() || reason.contains('\0') {
        return Err(TransitionError::InvalidReason);
    }
    Ok(Transition {
        from,
        to,
        actor,
        reason: reason.to_owned(),
    })
}

pub(crate) fn job_transition(
    from: JobState,
    to: JobState,
    recurring: bool,
    actor: TransitionActor,
    reason: &str,
) -> Result<Transition<JobState>, TransitionError> {
    let one_shot_legal = matches!(
        (from, to),
        (
            JobState::Scheduled,
            JobState::Waiting | JobState::CancelRequested | JobState::Missed
        ) | (
            JobState::Waiting,
            JobState::Starting | JobState::CancelRequested | JobState::Missed
        ) | (
            JobState::Starting,
            JobState::Running
                | JobState::Failed
                | JobState::CancelRequested
                | JobState::Interrupted
        ) | (
            JobState::Running,
            JobState::Succeeded
                | JobState::Failed
                | JobState::CancelRequested
                | JobState::Interrupted
        ) | (
            JobState::CancelRequested,
            JobState::Cancelled | JobState::Succeeded | JobState::Failed | JobState::Interrupted
        )
    );
    let recurring_legal = recurring
        && matches!(
            (from, to),
            (
                JobState::Starting
                    | JobState::Running
                    | JobState::Succeeded
                    | JobState::Failed
                    | JobState::Interrupted
                    | JobState::Missed,
                JobState::Waiting
            )
        );

    transition(from, to, actor, reason, one_shot_legal || recurring_legal)
}

pub(crate) fn run_transition(
    from: RunState,
    to: RunState,
    actor: TransitionActor,
    reason: &str,
) -> Result<Transition<RunState>, TransitionError> {
    let legal = matches!(
        (from, to),
        (
            RunState::Starting,
            RunState::Running
                | RunState::Failed
                | RunState::CancelRequested
                | RunState::Interrupted
        ) | (
            RunState::Running,
            RunState::Succeeded
                | RunState::Failed
                | RunState::CancelRequested
                | RunState::Interrupted
        ) | (
            RunState::CancelRequested,
            RunState::Cancelled | RunState::Succeeded | RunState::Failed | RunState::Interrupted
        )
    );
    transition(from, to, actor, reason, legal)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Completion {
    Exit(i32),
    Signal(i32),
    Failure,
    Interrupted,
    TerminatedByCancellation,
}

pub(crate) fn complete_run(
    current: RunState,
    completion: Completion,
) -> Result<RunState, TransitionError> {
    let target = match completion {
        Completion::Exit(0) => RunState::Succeeded,
        Completion::Exit(_) | Completion::Signal(_) | Completion::Failure => RunState::Failed,
        Completion::Interrupted => RunState::Interrupted,
        Completion::TerminatedByCancellation => {
            if current != RunState::CancelRequested {
                return Err(TransitionError::CancellationNotCommitted);
            }
            RunState::Cancelled
        }
    };

    run_transition(
        current,
        target,
        TransitionActor::Monitor,
        "command completion",
    )
    .map(|transition| transition.to())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum TransitionError {
    #[error("illegal state transition")]
    Illegal,
    #[error("transition reason must be non-empty and contain no NUL")]
    InvalidReason,
    #[error("cancellation was not committed before termination")]
    CancellationNotCommitted,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        Completion, TransitionActor, TransitionError, complete_run, job_transition, run_transition,
    };
    use crate::domain::state::{JobState, RunState};

    #[test]
    fn one_shot_matrix_matches_contract() {
        let legal = [
            (JobState::Scheduled, JobState::Waiting),
            (JobState::Scheduled, JobState::CancelRequested),
            (JobState::Scheduled, JobState::Missed),
            (JobState::Waiting, JobState::Starting),
            (JobState::Waiting, JobState::CancelRequested),
            (JobState::Waiting, JobState::Missed),
            (JobState::Starting, JobState::Running),
            (JobState::Starting, JobState::Failed),
            (JobState::Starting, JobState::CancelRequested),
            (JobState::Starting, JobState::Interrupted),
            (JobState::Running, JobState::Succeeded),
            (JobState::Running, JobState::Failed),
            (JobState::Running, JobState::CancelRequested),
            (JobState::Running, JobState::Interrupted),
            (JobState::CancelRequested, JobState::Cancelled),
            (JobState::CancelRequested, JobState::Succeeded),
            (JobState::CancelRequested, JobState::Failed),
            (JobState::CancelRequested, JobState::Interrupted),
        ];

        for from in JobState::ALL {
            for to in JobState::ALL {
                let actual =
                    job_transition(from, to, false, TransitionActor::Supervisor, "matrix").is_ok();
                assert_eq!(actual, legal.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn recurring_run_can_return_job_to_waiting() {
        let transition = job_transition(
            JobState::Running,
            JobState::Waiting,
            true,
            TransitionActor::Monitor,
            "next occurrence",
        );
        assert_eq!(
            transition.map(|value| (value.from(), value.to(), value.reason().to_owned(),)),
            Ok((
                JobState::Running,
                JobState::Waiting,
                "next occurrence".to_owned(),
            ))
        );
    }

    #[test]
    fn transition_reasons_must_be_safe_and_nonempty() {
        for reason in ["", "bad\0reason"] {
            assert!(
                run_transition(
                    RunState::Starting,
                    RunState::Running,
                    TransitionActor::Supervisor,
                    reason,
                )
                .is_err(),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn cancellation_race_uses_committed_precedence() {
        assert_eq!(
            complete_run(
                RunState::CancelRequested,
                Completion::TerminatedByCancellation
            ),
            Ok(RunState::Cancelled)
        );
        assert_eq!(
            complete_run(RunState::CancelRequested, Completion::Exit(0)),
            Ok(RunState::Succeeded)
        );
        assert_eq!(
            complete_run(RunState::CancelRequested, Completion::Exit(7)),
            Ok(RunState::Failed)
        );
        // A cancellation completion without a committed cancel request is a
        // protocol violation, not a cancelled outcome.
        for state in [RunState::Starting, RunState::Running] {
            assert_eq!(
                complete_run(state, Completion::TerminatedByCancellation),
                Err(TransitionError::CancellationNotCommitted)
            );
        }
    }

    #[test]
    fn run_matrix_matches_contract() {
        let legal = [
            (RunState::Starting, RunState::Running),
            (RunState::Starting, RunState::Failed),
            (RunState::Starting, RunState::CancelRequested),
            (RunState::Starting, RunState::Interrupted),
            (RunState::Running, RunState::Succeeded),
            (RunState::Running, RunState::Failed),
            (RunState::Running, RunState::CancelRequested),
            (RunState::Running, RunState::Interrupted),
            (RunState::CancelRequested, RunState::Cancelled),
            (RunState::CancelRequested, RunState::Succeeded),
            (RunState::CancelRequested, RunState::Failed),
            (RunState::CancelRequested, RunState::Interrupted),
        ];

        for from in RunState::ALL {
            for to in RunState::ALL {
                let actual = run_transition(from, to, TransitionActor::Recovery, "matrix").is_ok();
                assert_eq!(actual, legal.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
    }

    proptest! {
        #[test]
        fn generated_transition_sequences_never_leave_terminal_state(
            targets in prop::collection::vec(
                prop::sample::select(RunState::ALL.to_vec()),
                0..64,
            ),
        ) {
            let mut current = RunState::Starting;
            for target in targets {
                let previous = current;
                let was_terminal = current.is_terminal();
                if run_transition(current, target, TransitionActor::Recovery, "generated").is_ok() {
                    current = target;
                }
                if was_terminal {
                    prop_assert_eq!(current, previous);
                    prop_assert!(current.is_terminal());
                }
            }
        }

        #[test]
        fn terminal_run_states_never_transition(
            from in prop::sample::select(vec![
                RunState::Succeeded,
                RunState::Failed,
                RunState::Cancelled,
                RunState::Interrupted,
            ]),
            to in prop::sample::select(RunState::ALL.to_vec()),
        ) {
            prop_assert!(
                run_transition(from, to, TransitionActor::Recovery, "generated").is_err()
            );
        }

        #[test]
        fn generated_job_sequences_never_leave_terminal_state(
            targets in prop::collection::vec(
                prop::sample::select(JobState::ALL.to_vec()),
                0..64,
            ),
        ) {
            let mut current = JobState::Scheduled;
            for target in targets {
                let previous = current;
                let was_terminal = current.is_terminal();
                if job_transition(current, target, false, TransitionActor::Supervisor, "generated")
                    .is_ok()
                {
                    current = target;
                }
                if was_terminal {
                    prop_assert_eq!(current, previous);
                    prop_assert!(current.is_terminal());
                }
            }
        }

        #[test]
        fn generated_recurring_sequences_only_reenter_through_waiting(
            targets in prop::collection::vec(
                prop::sample::select(JobState::ALL.to_vec()),
                0..64,
            ),
        ) {
            let mut current = JobState::Scheduled;
            for target in targets {
                let previous = current;
                if job_transition(current, target, true, TransitionActor::Monitor, "generated")
                    .is_ok()
                {
                    current = target;
                }
                // Recurring jobs may only leave a state via the one-shot edge
                // or the recurring advance back to Waiting; every other move
                // must also be legal without recurrence.
                if current != previous
                    && current != JobState::Waiting
                {
                    prop_assert!(
                        job_transition(
                            previous,
                            current,
                            false,
                            TransitionActor::Monitor,
                            "generated",
                        )
                        .is_ok(),
                        "{previous:?} -> {current:?} only legal because recurring"
                    );
                }
            }
        }
    }
}
