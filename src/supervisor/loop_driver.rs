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

pub(crate) fn run_loop<Now, Due>(
    receiver: &Receiver<SupervisorEvent>,
    heap: &mut DeadlineHeap,
    idle_timeout: Option<Duration>,
    mut now: Now,
    mut on_due: Due,
) where
    Now: FnMut() -> ElapsedInstant,
    Due: FnMut(Vec<JobId>),
{
    loop {
        let due = heap.pop_due(now());
        if !due.is_empty() {
            on_due(due);
            continue;
        }
        let empty = heap.is_empty();
        let wait = wait_until(now(), heap.next_deadline(), idle_timeout);
        let event = match wait {
            Some(wait) => receiver.recv_timeout(wait),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match event {
            Ok(SupervisorEvent::Schedule { job_id, deadline }) => heap.upsert(job_id, deadline),
            Ok(SupervisorEvent::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if empty => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

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
    Some(idle_timeout.map_or(deadline_wait, |idle| deadline_wait.min(idle)))
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
    use std::time::{Duration, Instant};

    use super::{SupervisorEvent, reconcile_wall_schedule, run_loop, wait_until};
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
    fn new_earlier_deadline_wakes_the_loop() {
        let (sender, receiver) = mpsc::channel();
        let wake_sender = sender.clone();
        let shutdown_sender = sender;
        let clock = Arc::new(AtomicU64::new(0));
        let wake_clock = Arc::clone(&clock);
        let job_id = JobId::new();
        let start = Instant::now();
        let wake = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            wake_clock.store(1, Ordering::Release);
            wake_sender
                .send(SupervisorEvent::Schedule {
                    job_id,
                    deadline: ElapsedInstant::from_nanos(1),
                })
                .expect("loop is listening");
        });

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
        wake.join().expect("wake thread");

        assert_eq!(due, vec![job_id]);
        assert!(start.elapsed() < Duration::from_millis(500));
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
