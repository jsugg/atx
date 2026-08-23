//! Supervisor recovery wiring.

use super::heap::DeadlineHeap;
use crate::application::RecoveryPlan;

pub(crate) fn rebuild_deadline_heap(plan: &RecoveryPlan) -> DeadlineHeap {
    let mut heap = DeadlineHeap::default();
    for deadline in &plan.deadlines {
        heap.upsert(deadline.job_id, deadline.elapsed_due);
    }
    heap
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::rebuild_deadline_heap;
    use crate::application::{RecoveredDeadline, RecoveryPlan};
    use crate::domain::{ElapsedInstant, JobId, UtcTimestamp};

    #[test]
    fn rebuild_uses_only_recovered_waiting_jobs() {
        let first = JobId::new();
        let second = JobId::new();
        let plan = RecoveryPlan {
            deadlines: vec![
                RecoveredDeadline {
                    job_id: first,
                    scheduled_for_utc: UtcTimestamp::from_second(10).expect("constant timestamp"),
                    elapsed_due: ElapsedInstant::from_nanos(20),
                },
                RecoveredDeadline {
                    job_id: second,
                    scheduled_for_utc: UtcTimestamp::from_second(11).expect("constant timestamp"),
                    elapsed_due: ElapsedInstant::from_nanos(10),
                },
            ],
            preserved_runs: Vec::new(),
            actions: Vec::new(),
        };

        let mut heap = rebuild_deadline_heap(&plan);
        assert_eq!(heap.pop_due(ElapsedInstant::from_nanos(10)), vec![second]);
        assert_eq!(heap.pop_due(ElapsedInstant::from_nanos(20)), vec![first]);
    }
}
