//! Supervisor event loop.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use thiserror::Error;

use super::heap::DeadlineHeap;
use crate::domain::{ElapsedInstant, JobId, UtcTimestamp};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum DeadlineReconciliationError {
    #[error("elapsed deadline is outside the supported range")]
    Overflow,
}

pub(crate) enum SupervisorEvent {
    Schedule {
        job_id: JobId,
        deadline: ElapsedInstant,
    },
    Shutdown,
}

/// Wait for deadlines and channel events until shutdown or an empty-session timeout.
pub(crate) fn run_loop<Now, Due>(
    receiver: &Receiver<SupervisorEvent>,
    heap: &mut DeadlineHeap,
    idle_timeout: Option<Duration>,
    now: Now,
    on_due: Due,
) where
    Now: FnMut() -> ElapsedInstant,
    Due: FnMut(Vec<JobId>),
{
    drive_loop(heap, idle_timeout, now, on_due, |wait| match wait {
        Some(wait) => receiver.recv_timeout(wait),
        None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
    });
}

/// Apply the scheduling policy before receiving the next event.
fn drive_loop<Now, Due, Receive>(
    heap: &mut DeadlineHeap,
    idle_timeout: Option<Duration>,
    mut now: Now,
    mut on_due: Due,
    mut receive: Receive,
) where
    Now: FnMut() -> ElapsedInstant,
    Due: FnMut(Vec<JobId>),
    Receive: FnMut(Option<Duration>) -> Result<SupervisorEvent, RecvTimeoutError>,
{
    loop {
        let due = heap.pop_due(now());
        if !due.is_empty() {
            on_due(due);
            continue;
        }
        let empty = heap.is_empty();
        let wait = wait_until(now(), heap.next_deadline(), idle_timeout);
        let event = receive(wait);
        match event {
            Ok(SupervisorEvent::Schedule { job_id, deadline }) => heap.upsert(job_id, deadline),
            Ok(SupervisorEvent::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if empty => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Use the empty-heap idle policy only when there is no queued deadline.
pub(crate) fn wait_until(
    now: ElapsedInstant,
    deadline: Option<ElapsedInstant>,
    idle_timeout: Option<Duration>,
) -> Option<Duration> {
    let Some(deadline) = deadline else {
        return idle_timeout;
    };
    let nanos = deadline.as_nanos().saturating_sub(now.as_nanos());
    let deadline_wait = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
    Some(deadline_wait)
}

/// Maps a persisted wall-clock schedule onto the current elapsed clock.
///
/// Live relative deadlines must keep their original elapsed value instead.
pub(crate) fn reconcile_wall_schedule(
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
    due_utc: UtcTimestamp,
) -> Result<ElapsedInstant, DeadlineReconciliationError> {
    let remaining = due_utc
        .as_jiff()
        .duration_since(wall_now.as_jiff())
        .as_nanos();
    if remaining <= 0 {
        return Ok(elapsed_now);
    }
    let remaining = u128::try_from(remaining).map_err(|_| DeadlineReconciliationError::Overflow)?;
    elapsed_now
        .as_nanos()
        .checked_add(remaining)
        .map(ElapsedInstant::from_nanos)
        .ok_or(DeadlineReconciliationError::Overflow)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{SupervisorEvent, drive_loop, reconcile_wall_schedule, run_loop, wait_until};
    use crate::domain::{ElapsedInstant, JobId, UtcTimestamp};
    use crate::supervisor::heap::DeadlineHeap;

    #[test]
    fn earlier_deadlines_shorten_blocking_wait_without_idle_spin() {
        let now = ElapsedInstant::from_nanos(1_000);
        let idle = Some(Duration::from_secs(60));
        assert_eq!(wait_until(now, None, idle), idle);
        assert_eq!(
            wait_until(now, Some(ElapsedInstant::from_nanos(2_000)), idle),
            Some(Duration::from_micros(1))
        );
        assert_eq!(
            wait_until(now, Some(ElapsedInstant::from_nanos(999)), idle),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn future_deadlines_do_not_use_the_session_idle_timer() {
        let now = ElapsedInstant::from_nanos(0);
        let deadline = ElapsedInstant::from_nanos(86_400_000_000_000);
        for idle in [Some(Duration::from_secs(30)), Some(Duration::ZERO), None] {
            assert_eq!(
                wait_until(now, Some(deadline), idle),
                Some(Duration::from_secs(86_400))
            );
            assert_eq!(
                wait_until(now, Some(ElapsedInstant::from_nanos(1)), idle),
                Some(Duration::from_nanos(1))
            );
            assert_eq!(
                wait_until(now, Some(ElapsedInstant::from_nanos(u128::MAX)), idle),
                Some(Duration::from_nanos(u64::MAX))
            );
        }
    }

    #[test]
    fn populated_loop_observes_only_real_deadlines_and_queued_events() {
        let (sender, receiver) = mpsc::channel();
        let job = JobId::new();
        let earlier = ElapsedInstant::from_nanos(3_600_000_000_000);
        sender
            .send(SupervisorEvent::Schedule {
                job_id: job,
                deadline: earlier,
            })
            .expect("queued schedule");
        sender
            .send(SupervisorEvent::Shutdown)
            .expect("queued shutdown");
        let mut heap = DeadlineHeap::default();
        heap.upsert(job, ElapsedInstant::from_nanos(7_200_000_000_000));
        let clock_reads = AtomicU64::new(0);
        let mut waits = Vec::new();
        drive_loop(
            &mut heap,
            Some(Duration::from_secs(30)),
            || {
                clock_reads.fetch_add(1, Ordering::Relaxed);
                ElapsedInstant::from_nanos(0)
            },
            |_| panic!("future jobs are not due"),
            |wait| {
                let expected = if waits.is_empty() { 7_200 } else { 3_600 };
                assert_eq!(
                    wait,
                    Some(Duration::from_secs(expected)),
                    "an idle tick would wake the populated loop"
                );
                waits.push(wait);
                receiver.recv_timeout(wait.expect("populated heap has a deadline"))
            },
        );
        assert_eq!(waits.len(), 2);
        assert_eq!(clock_reads.load(Ordering::Relaxed), 4);
        assert_eq!(heap.next_deadline(), Some(earlier));
    }

    #[test]
    fn empty_modes_keep_the_session_and_service_wait_policies() {
        for idle in [Some(Duration::from_secs(30)), Some(Duration::ZERO), None] {
            let (sender, receiver) = mpsc::channel();
            if idle.is_none() {
                sender
                    .send(SupervisorEvent::Shutdown)
                    .expect("service shutdown");
            }
            let mut heap = DeadlineHeap::default();
            let clock_reads = AtomicU64::new(0);
            let mut wait_calls = 0;
            drive_loop(
                &mut heap,
                idle,
                || {
                    clock_reads.fetch_add(1, Ordering::Relaxed);
                    ElapsedInstant::from_nanos(0)
                },
                |_| panic!("empty heap cannot produce work"),
                |wait| {
                    wait_calls += 1;
                    assert_eq!(wait, idle);
                    match wait {
                        Some(_) => {
                            assert!(matches!(
                                receiver.try_recv(),
                                Err(mpsc::TryRecvError::Empty)
                            ));
                            Err(mpsc::RecvTimeoutError::Timeout)
                        }
                        None => receiver
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                    }
                },
            );
            assert_eq!(wait_calls, 1);
            assert_eq!(clock_reads.load(Ordering::Relaxed), 2);
            assert!(heap.is_empty());
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(128))]
        #[test]
        fn deadline_waits_ignore_idle_policy_and_keep_saturated_bounds(
            now in proptest::num::u128::ANY,
            deadline in proptest::num::u128::ANY,
            idle in proptest::num::u64::ANY,
        ) {
            let expected = Some(Duration::from_nanos(u64::try_from(deadline.saturating_sub(now)).unwrap_or(u64::MAX)));
            for policy in [Some(Duration::from_nanos(idle)), Some(Duration::ZERO), None] {
                proptest::prop_assert_eq!(wait_until(ElapsedInstant::from_nanos(now), Some(ElapsedInstant::from_nanos(deadline)), policy), expected);
            }
        }
    }

    #[test]
    fn wall_schedules_reconcile_against_both_clocks() {
        let due = UtcTimestamp::from_second(200).expect("valid timestamp");
        assert_eq!(
            reconcile_wall_schedule(
                UtcTimestamp::from_second(150).expect("valid timestamp"),
                ElapsedInstant::from_nanos(10_000),
                due,
            ),
            Ok(ElapsedInstant::from_nanos(50_000_010_000))
        );
        assert_eq!(
            reconcile_wall_schedule(
                UtcTimestamp::from_second(201).expect("valid timestamp"),
                ElapsedInstant::from_nanos(10_000),
                due,
            ),
            Ok(ElapsedInstant::from_nanos(10_000))
        );
    }

    #[test]
    fn queued_deadline_is_delivered_without_idle_waiting() {
        let (sender, receiver) = mpsc::channel();
        let shutdown_sender = sender.clone();
        let clock = Arc::new(AtomicU64::new(0));
        let job_id = JobId::new();
        clock.store(1, Ordering::Release);
        sender
            .send(SupervisorEvent::Schedule {
                job_id,
                deadline: ElapsedInstant::from_nanos(1),
            })
            .expect("event queue is open");

        let mut heap = DeadlineHeap::default();
        let mut due = Vec::new();
        run_loop(
            &receiver,
            &mut heap,
            Some(Duration::from_secs(1)),
            || ElapsedInstant::from_nanos(u128::from(clock.load(Ordering::Acquire))),
            |batch| {
                due.extend(batch);
                shutdown_sender
                    .send(SupervisorEvent::Shutdown)
                    .expect("loop is listening");
            },
        );

        assert_eq!(due, vec![job_id]);
    }

    #[test]
    fn due_jobs_are_delivered_as_one_batch() {
        let (sender, receiver) = mpsc::channel();
        let first = JobId::new();
        let second = JobId::new();
        let mut heap = DeadlineHeap::default();
        heap.upsert(first, ElapsedInstant::from_nanos(1));
        heap.upsert(second, ElapsedInstant::from_nanos(1));
        let mut batches = Vec::new();

        run_loop(
            &receiver,
            &mut heap,
            Some(Duration::from_secs(1)),
            || ElapsedInstant::from_nanos(1),
            |batch| {
                batches.push(batch);
                sender
                    .send(SupervisorEvent::Shutdown)
                    .expect("loop is listening");
            },
        );

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn empty_loop_blocks_once_then_exits() {
        let (_sender, receiver) = mpsc::channel();
        let clock_reads = AtomicU64::new(0);
        let mut heap = DeadlineHeap::default();

        run_loop(
            &receiver,
            &mut heap,
            Some(Duration::from_millis(5)),
            || {
                clock_reads.fetch_add(1, Ordering::Relaxed);
                ElapsedInstant::from_nanos(0)
            },
            |_| panic!("empty heap cannot produce work"),
        );

        assert_eq!(clock_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn service_managed_empty_loop_waits_for_shutdown() {
        let (sender, receiver) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            sender
                .send(SupervisorEvent::Shutdown)
                .expect("loop is listening");
        });
        let mut heap = DeadlineHeap::default();

        run_loop(
            &receiver,
            &mut heap,
            None,
            || ElapsedInstant::from_nanos(0),
            |_| panic!("empty heap cannot produce work"),
        );
        shutdown.join().expect("shutdown thread");
    }
}
