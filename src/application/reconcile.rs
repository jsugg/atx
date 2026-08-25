//! Startup reconciliation service.

use thiserror::Error;

use crate::domain::{
    ClaimToken, ElapsedInstant, Job, JobId, JobState, ProcessIdentitySnapshot, Revision, Run,
    RunId, RunState, Schedule, UtcTimestamp, next_fixed_rate_utc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityStatus {
    Alive,
    Dead,
    Changed,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("process identity inspection failed: {0}")]
pub(crate) struct IdentityInspectionError(pub(crate) String);

pub(crate) trait IdentityInspector {
    fn classify(
        &self,
        identity: &ProcessIdentitySnapshot,
    ) -> Result<IdentityStatus, IdentityInspectionError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandFate {
    Alive,
    Dead,
    Changed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryRecord {
    pub(crate) job: Job,
    pub(crate) active_run: Option<Run>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredDeadline {
    pub(crate) job_id: JobId,
    pub(crate) scheduled_for_utc: UtcTimestamp,
    pub(crate) elapsed_due: ElapsedInstant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryAction {
    Interrupt {
        job_id: JobId,
        expected_revision: Revision,
        run: Option<(RunId, ClaimToken)>,
        command_fate: CommandFate,
    },
    MarkMissed {
        job_id: JobId,
        expected_revision: Revision,
    },
    AdvanceRecurring {
        job_id: JobId,
        expected_revision: Revision,
        next_due_utc: UtcTimestamp,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecoveryPlan {
    pub(crate) deadlines: Vec<RecoveredDeadline>,
    pub(crate) preserved_runs: Vec<RunId>,
    pub(crate) actions: Vec<RecoveryAction>,
}

pub(crate) trait RecoveryStore {
    fn load_nonterminal(&self) -> Result<Vec<RecoveryRecord>, RecoveryStoreError>;
    fn apply_recovery(
        &mut self,
        actions: &[RecoveryAction],
        now: UtcTimestamp,
    ) -> Result<(), RecoveryStoreError>;
    fn cleanup_recovery(&mut self, now: UtcTimestamp) -> Result<(), RecoveryStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("recovery storage failed: {0}")]
pub(crate) struct RecoveryStoreError(pub(crate) String);

pub(crate) fn reconcile_startup<Store: RecoveryStore, Inspector: IdentityInspector>(
    store: &mut Store,
    inspector: &Inspector,
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
    boot_identity: &str,
) -> Result<RecoveryPlan, StartupReconciliationError> {
    let records = store.load_nonterminal()?;
    let plan = plan_recovery(&records, inspector, wall_now, elapsed_now, boot_identity)?;
    store.apply_recovery(&plan.actions, wall_now)?;
    store.cleanup_recovery(wall_now)?;
    Ok(plan)
}

pub(crate) fn plan_recovery<Inspector: IdentityInspector>(
    records: &[RecoveryRecord],
    inspector: &Inspector,
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
    boot_identity: &str,
) -> Result<RecoveryPlan, ReconciliationError> {
    if boot_identity.is_empty() || boot_identity.contains('\0') {
        return Err(ReconciliationError::InvalidBootIdentity);
    }
    let mut plan = RecoveryPlan::default();
    for record in records {
        if record.job.state().is_terminal() {
            return Err(ReconciliationError::TerminalJobLoaded(record.job.id()));
        }
        if matches!(
            record.job.state(),
            JobState::Starting | JobState::Running | JobState::CancelRequested
        ) {
            reconcile_active(record, inspector, boot_identity, &mut plan);
        } else {
            if record.active_run.is_some() {
                return Err(ReconciliationError::UnexpectedActiveRun(record.job.id()));
            }
            reconcile_waiting(record, wall_now, elapsed_now, &mut plan)?;
        }
    }
    plan.deadlines
        .sort_unstable_by_key(|deadline| (deadline.elapsed_due, deadline.job_id));
    plan.preserved_runs.sort_unstable();
    Ok(plan)
}

fn reconcile_active<Inspector: IdentityInspector>(
    record: &RecoveryRecord,
    inspector: &Inspector,
    boot_identity: &str,
    plan: &mut RecoveryPlan,
) {
    let Some(run) = record.active_run.as_ref() else {
        plan.actions
            .push(interrupt_action(record, None, CommandFate::Unknown));
        return;
    };
    if run.state().is_terminal() {
        plan.actions
            .push(interrupt_action(record, None, CommandFate::Unknown));
        return;
    }

    let command_fate = inspect_fate(run.command_identity(), inspector, boot_identity);
    let monitor_alive = run.monitor_identity().is_some_and(|identity| {
        identity.boot_identity == boot_identity
            && inspector.classify(identity) == Ok(IdentityStatus::Alive)
    });
    if monitor_alive && matches!(run.state(), RunState::Running | RunState::CancelRequested) {
        plan.preserved_runs.push(run.id());
        return;
    }
    plan.actions.push(interrupt_action(
        record,
        Some((run.id(), run.claim_token())),
        command_fate,
    ));
}

fn interrupt_action(
    record: &RecoveryRecord,
    run: Option<(RunId, ClaimToken)>,
    command_fate: CommandFate,
) -> RecoveryAction {
    RecoveryAction::Interrupt {
        job_id: record.job.id(),
        expected_revision: record.job.revision(),
        run,
        command_fate,
    }
}

fn inspect_fate<Inspector: IdentityInspector>(
    identity: Option<&ProcessIdentitySnapshot>,
    inspector: &Inspector,
    boot_identity: &str,
) -> CommandFate {
    let Some(identity) = identity else {
        return CommandFate::Unknown;
    };
    if identity.boot_identity != boot_identity {
        return CommandFate::Changed;
    }
    match inspector.classify(identity) {
        Ok(IdentityStatus::Alive) => CommandFate::Alive,
        Ok(IdentityStatus::Dead) => CommandFate::Dead,
        Ok(IdentityStatus::Changed) => CommandFate::Changed,
        Err(_) => CommandFate::Unknown,
    }
}

fn reconcile_waiting(
    record: &RecoveryRecord,
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
    plan: &mut RecoveryPlan,
) -> Result<(), ReconciliationError> {
    let due = record.job.next_due_utc();
    if due > wall_now {
        plan.deadlines
            .push(deadline(record.job.id(), due, wall_now, elapsed_now)?);
        return Ok(());
    }

    match (record.job.schedule(), record.job.missed_policy()) {
        (_, crate::domain::MissedPolicy::RunLatest) => {
            plan.deadlines.push(RecoveredDeadline {
                job_id: record.job.id(),
                scheduled_for_utc: due,
                elapsed_due: elapsed_now,
            });
            if let Schedule::RecurringInterval {
                interval,
                persisted_anchor_utc,
            } = record.job.schedule()
            {
                let next_due = next_fixed_rate_utc(*persisted_anchor_utc, wall_now, *interval)
                    .map_err(|_| ReconciliationError::DeadlineOverflow)?;
                plan.actions.push(RecoveryAction::AdvanceRecurring {
                    job_id: record.job.id(),
                    expected_revision: record.job.revision(),
                    next_due_utc: next_due,
                });
            }
        }
        (
            Schedule::RecurringInterval {
                interval,
                persisted_anchor_utc,
            },
            crate::domain::MissedPolicy::Skip,
        ) => {
            let next_due = next_fixed_rate_utc(*persisted_anchor_utc, wall_now, *interval)
                .map_err(|_| ReconciliationError::DeadlineOverflow)?;
            plan.actions.push(RecoveryAction::AdvanceRecurring {
                job_id: record.job.id(),
                expected_revision: record.job.revision(),
                next_due_utc: next_due,
            });
            plan.deadlines
                .push(deadline(record.job.id(), next_due, wall_now, elapsed_now)?);
        }
        _ => plan.actions.push(RecoveryAction::MarkMissed {
            job_id: record.job.id(),
            expected_revision: record.job.revision(),
        }),
    }
    Ok(())
}

fn deadline(
    job_id: JobId,
    scheduled_for_utc: UtcTimestamp,
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
) -> Result<RecoveredDeadline, ReconciliationError> {
    let remaining = scheduled_for_utc
        .as_jiff()
        .duration_since(wall_now.as_jiff())
        .as_nanos();
    let remaining =
        u128::try_from(remaining.max(0)).map_err(|_| ReconciliationError::DeadlineOverflow)?;
    let elapsed_due = elapsed_now
        .as_nanos()
        .checked_add(remaining)
        .map(ElapsedInstant::from_nanos)
        .ok_or(ReconciliationError::DeadlineOverflow)?;
    Ok(RecoveredDeadline {
        job_id,
        scheduled_for_utc,
        elapsed_due,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ReconciliationError {
    #[error("current boot identity is invalid")]
    InvalidBootIdentity,
    #[error("recovery loaded terminal job {0}")]
    TerminalJobLoaded(JobId),
    #[error("waiting job {0} has an active run")]
    UnexpectedActiveRun(JobId),
    #[error("deadline arithmetic overflowed")]
    DeadlineOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum StartupReconciliationError {
    #[error(transparent)]
    Plan(#[from] ReconciliationError),
    #[error(transparent)]
    Store(#[from] RecoveryStoreError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::Cell;

    use super::{
        CommandFate, IdentityInspectionError, IdentityInspector, IdentityStatus, RecoveryAction,
        RecoveryRecord, plan_recovery,
    };
    use crate::domain::{
        ClaimToken, DurationSeconds, ElapsedInstant, Environment, ExecutionMode, ExecutionSpec,
        Job, JobState, MissedPolicy, ProcessIdentitySnapshot, Run, RunOutcome, RuntimeTier,
        Schedule, Sequence, TransitionActor, UtcTimestamp,
    };

    struct Inspector {
        status: IdentityStatus,
        fail: bool,
        calls: Cell<usize>,
    }

    impl IdentityInspector for Inspector {
        fn classify(
            &self,
            _identity: &ProcessIdentitySnapshot,
        ) -> Result<IdentityStatus, IdentityInspectionError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                Err(IdentityInspectionError("unknown".to_owned()))
            } else {
                Ok(self.status)
            }
        }
    }

    #[test]
    fn live_monitor_survives_supervisor_restart() {
        let job = job_in(JobState::Running, MissedPolicy::Hold, false);
        let run = running_run(&job, "boot");
        let inspector = Inspector {
            status: IdentityStatus::Alive,
            fail: false,
            calls: Cell::new(0),
        };

        let plan = plan_recovery(
            &[RecoveryRecord {
                job,
                active_run: Some(run.clone()),
            }],
            &inspector,
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        )
        .expect("plan");

        assert_eq!(plan.preserved_runs, vec![run.id()]);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn active_states_without_a_valid_monitor_are_interrupted() {
        for state in [
            JobState::Starting,
            JobState::Running,
            JobState::CancelRequested,
        ] {
            let job = job_in(state, MissedPolicy::Hold, false);
            let active_run = match state {
                JobState::Starting => Some(starting_run(&job)),
                JobState::Running => Some(running_run(&job, "old-boot")),
                JobState::CancelRequested => Some(
                    running_run(&job, "old-boot")
                        .request_cancellation()
                        .expect("cancellation"),
                ),
                _ => None,
            };
            let plan = plan_recovery(
                &[RecoveryRecord { job, active_run }],
                &Inspector {
                    status: IdentityStatus::Alive,
                    fail: false,
                    calls: Cell::new(0),
                },
                timestamp(200),
                ElapsedInstant::from_nanos(500),
                "boot",
            )
            .expect("plan");
            assert!(matches!(
                plan.actions.as_slice(),
                [RecoveryAction::Interrupt { .. }]
            ));
        }
    }

    #[test]
    fn unknown_monitor_and_live_command_are_reported() {
        let job = job_in(JobState::Running, MissedPolicy::Hold, false);
        let run = running_run(&job, "boot");
        let plan = plan_recovery(
            &[RecoveryRecord {
                job,
                active_run: Some(run),
            }],
            &Inspector {
                status: IdentityStatus::Alive,
                fail: true,
                calls: Cell::new(0),
            },
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        )
        .expect("plan");

        assert!(matches!(
            plan.actions.as_slice(),
            [RecoveryAction::Interrupt {
                command_fate: CommandFate::Unknown,
                ..
            }]
        ));
    }

    #[test]
    fn waiting_jobs_apply_each_missed_policy_and_rebuild_deadlines() {
        let future = job_in(JobState::Scheduled, MissedPolicy::Hold, false);
        let held = overdue_job(MissedPolicy::Hold, false);
        let latest = overdue_job(MissedPolicy::RunLatest, false);
        let recurring = overdue_job(MissedPolicy::Skip, true);
        let records = vec![
            RecoveryRecord {
                job: future,
                active_run: None,
            },
            RecoveryRecord {
                job: held,
                active_run: None,
            },
            RecoveryRecord {
                job: latest,
                active_run: None,
            },
            RecoveryRecord {
                job: recurring,
                active_run: None,
            },
        ];
        let inspector = Inspector {
            status: IdentityStatus::Dead,
            fail: false,
            calls: Cell::new(0),
        };

        let first = plan_recovery(
            &records,
            &inspector,
            timestamp(200),
            ElapsedInstant::from_nanos(1_000),
            "boot",
        )
        .expect("plan");
        let second = plan_recovery(
            &records,
            &inspector,
            timestamp(200),
            ElapsedInstant::from_nanos(1_000),
            "boot",
        )
        .expect("plan");

        assert_eq!(first, second);
        assert_eq!(first.deadlines.len(), 3);
        assert_eq!(first.actions.len(), 2);
        assert!(
            first
                .actions
                .iter()
                .any(|action| matches!(action, RecoveryAction::MarkMissed { .. }))
        );
        assert!(
            first
                .actions
                .iter()
                .any(|action| matches!(action, RecoveryAction::AdvanceRecurring { .. }))
        );
    }

    #[test]
    fn invalid_boot_identity_is_rejected() {
        let job = job_in(JobState::Running, MissedPolicy::Hold, false);
        for boot_identity in ["", "b\0ot"] {
            let result = plan_recovery(
                &[RecoveryRecord {
                    job: job.clone(),
                    active_run: None,
                }],
                &inspector(IdentityStatus::Dead, false),
                timestamp(200),
                ElapsedInstant::from_nanos(500),
                boot_identity,
            );
            assert!(
                matches!(result, Err(super::ReconciliationError::InvalidBootIdentity)),
                "{result:?}"
            );
        }
    }

    #[test]
    fn terminal_job_and_stray_active_run_are_rejected() {
        let mut finished = job_in(JobState::Running, MissedPolicy::Hold, false);
        finished
            .transition(
                JobState::Succeeded,
                false,
                TransitionActor::Monitor,
                "finished",
                timestamp(20),
            )
            .expect("terminal");
        let result = plan_recovery(
            &[RecoveryRecord {
                job: finished.clone(),
                active_run: None,
            }],
            &inspector(IdentityStatus::Dead, false),
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        );
        assert!(
            matches!(
                &result,
                Err(super::ReconciliationError::TerminalJobLoaded(job_id))
                    if *job_id == finished.id()
            ),
            "{result:?}"
        );

        let waiting = overdue_job(MissedPolicy::Hold, false);
        let result = plan_recovery(
            &[RecoveryRecord {
                job: waiting.clone(),
                active_run: Some(starting_run(&waiting)),
            }],
            &inspector(IdentityStatus::Dead, false),
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        );
        assert!(
            matches!(
                &result,
                Err(super::ReconciliationError::UnexpectedActiveRun(job_id))
                    if *job_id == waiting.id()
            ),
            "{result:?}"
        );
    }

    #[test]
    fn active_jobs_without_a_live_run_are_interrupted() {
        // No run row at all: the command fate is unknowable.
        let plan = plan_recovery(
            &[RecoveryRecord {
                job: job_in(JobState::Running, MissedPolicy::Hold, false),
                active_run: None,
            }],
            &inspector(IdentityStatus::Alive, false),
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        )
        .expect("plan");
        assert!(matches!(
            plan.actions.as_slice(),
            [RecoveryAction::Interrupt {
                command_fate: CommandFate::Unknown,
                ..
            }]
        ));

        // A run that already recorded a terminal outcome needs no inspection.
        let finished = job_in(JobState::Running, MissedPolicy::Hold, false);
        let done = running_run(&finished, "old-boot")
            .with_outcome(timestamp(12), RunOutcome::Exit(0))
            .expect("finish");
        let plan = plan_recovery(
            &[RecoveryRecord {
                job: finished,
                active_run: Some(done),
            }],
            &inspector(IdentityStatus::Alive, false),
            timestamp(200),
            ElapsedInstant::from_nanos(500),
            "boot",
        )
        .expect("plan");
        assert!(matches!(
            plan.actions.as_slice(),
            [RecoveryAction::Interrupt {
                command_fate: CommandFate::Unknown,
                ..
            }]
        ));
        assert!(plan.preserved_runs.is_empty());
    }

    #[test]
    fn run_latest_recurring_job_reschedules_from_its_anchor() {
        let latest = overdue_job(MissedPolicy::RunLatest, true);
        let plan = plan_recovery(
            &[RecoveryRecord {
                job: latest,
                active_run: None,
            }],
            &inspector(IdentityStatus::Dead, false),
            timestamp(200),
            ElapsedInstant::from_nanos(1_000),
            "boot",
        )
        .expect("plan");
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(
            plan.actions.as_slice(),
            [RecoveryAction::AdvanceRecurring { .. }]
        ));
    }

    fn inspector(status: IdentityStatus, fail: bool) -> Inspector {
        Inspector {
            status,
            fail,
            calls: Cell::new(0),
        }
    }

    fn overdue_job(policy: MissedPolicy, recurring: bool) -> Job {
        let mut job = base_job(policy, recurring, 100);
        job.transition(
            JobState::Waiting,
            recurring,
            TransitionActor::Supervisor,
            "test setup",
            timestamp(2),
        )
        .expect("waiting");
        job
    }

    fn job_in(state: JobState, policy: MissedPolicy, recurring: bool) -> Job {
        let mut job = base_job(policy, recurring, 300);
        for next in [
            JobState::Waiting,
            JobState::Starting,
            JobState::Running,
            JobState::CancelRequested,
        ] {
            if job.state() == state {
                break;
            }
            job.transition(
                next,
                recurring,
                TransitionActor::Supervisor,
                "test setup",
                timestamp(i64::try_from(job.revision().get()).expect("small revision") + 1),
            )
            .expect("transition");
        }
        job
    }

    fn base_job(policy: MissedPolicy, recurring: bool, due: i64) -> Job {
        let schedule = if recurring {
            Schedule::RecurringInterval {
                interval: DurationSeconds::new(30).expect("duration"),
                persisted_anchor_utc: timestamp(due),
            }
        } else {
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("duration"), timestamp(due))
        };
        Job::new(
            timestamp(1),
            schedule,
            policy,
            RuntimeTier::Session,
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["true".to_owned()],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .expect("execution"),
            501,
        )
        .expect("job")
    }

    fn running_run(job: &Job, boot_identity: &str) -> Run {
        starting_run(job)
            .mark_running(
                timestamp(11),
                identity(10, boot_identity),
                identity(11, boot_identity),
                "runs/out.log".to_owned(),
                "runs/err.log".to_owned(),
            )
            .expect("running")
    }

    fn starting_run(job: &Job) -> Run {
        Run::new(
            job.id(),
            Sequence::new(1).expect("sequence"),
            job.next_due_utc(),
            timestamp(10),
            ClaimToken::from_bytes([7; 32]),
        )
    }

    fn identity(pid: u32, boot_identity: &str) -> ProcessIdentitySnapshot {
        ProcessIdentitySnapshot {
            boot_identity: boot_identity.to_owned(),
            pid,
            start_token: u64::from(pid),
            process_group_id: i32::try_from(pid).expect("pid"),
        }
    }

    fn timestamp(second: i64) -> UtcTimestamp {
        UtcTimestamp::from_second(second).expect("timestamp")
    }
}
