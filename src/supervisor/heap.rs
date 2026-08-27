//! Monotonic deadline heap.

use std::collections::HashMap;

use crate::domain::{ElapsedInstant, JobId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Entry {
    due: ElapsedInstant,
    job_id: JobId,
}

#[derive(Debug, Default)]
pub(crate) struct DeadlineHeap {
    entries: Vec<Entry>,
    positions: HashMap<JobId, usize>,
}

impl DeadlineHeap {
    pub(crate) fn upsert(&mut self, job_id: JobId, due: ElapsedInstant) {
        let Some(&index) = self.positions.get(&job_id) else {
            let index = self.entries.len();
            self.entries.push(Entry { due, job_id });
            self.positions.insert(job_id, index);
            self.sift_up(index);
            return;
        };

        self.entries[index].due = due;
        if index > 0 && Self::comes_before(self.entries[index], self.entries[Self::parent(index)]) {
            self.sift_up(index);
        } else {
            self.sift_down(index);
        }
    }

    pub(crate) fn remove(&mut self, job_id: JobId) -> bool {
        let Some(index) = self.positions.remove(&job_id) else {
            return false;
        };
        let Some(last) = self.entries.pop() else {
            debug_assert!(false, "indexed heap cannot be empty");
            return false;
        };
        if index == self.entries.len() {
            return true;
        }

        self.entries[index] = last;
        self.positions.insert(last.job_id, index);
        if index > 0 && Self::comes_before(self.entries[index], self.entries[Self::parent(index)]) {
            self.sift_up(index);
        } else {
            self.sift_down(index);
        }
        true
    }

    pub(crate) fn next_deadline(&self) -> Option<ElapsedInstant> {
        self.entries.first().map(|entry| entry.due)
    }

    pub(crate) fn pop_due(&mut self, now: ElapsedInstant) -> Vec<JobId> {
        let mut due = Vec::new();
        while self.entries.first().is_some_and(|entry| entry.due <= now) {
            let job_id = self.entries[0].job_id;
            let removed = self.remove(job_id);
            debug_assert!(removed);
            due.push(job_id);
        }
        due
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn parent(index: usize) -> usize {
        (index - 1) / 2
    }

    fn comes_before(left: Entry, right: Entry) -> bool {
        (left.due, left.job_id) < (right.due, right.job_id)
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.entries.swap(left, right);
        self.positions.insert(self.entries[left].job_id, left);
        self.positions.insert(self.entries[right].job_id, right);
    }

    fn sift_up(&mut self, mut index: usize) {
        while index > 0 {
            let parent = Self::parent(index);
            if !Self::comes_before(self.entries[index], self.entries[parent]) {
                break;
            }
            self.swap(index, parent);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let left = index * 2 + 1;
            if left >= self.entries.len() {
                return;
            }
            let right = left + 1;
            let child = if right < self.entries.len()
                && Self::comes_before(self.entries[right], self.entries[left])
            {
                right
            } else {
                left
            };
            if !Self::comes_before(self.entries[child], self.entries[index]) {
                return;
            }
            self.swap(index, child);
            index = child;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::DeadlineHeap;
    use crate::domain::{ElapsedInstant, JobId};

    #[test]
    fn removing_a_deep_entry_can_sift_the_replacement_up() {
        use std::collections::HashMap;

        let mut heap = DeadlineHeap::default();
        // The final due=5 replaces due=200. A remove that only sifts down
        // leaves it behind larger deadlines instead of restoring heap order.
        let dues = [1_u128, 2, 3, 50, 100, 4, 60, 61, 62, 200, 201, 5];
        let jobs: Vec<JobId> = dues.iter().map(|_| JobId::new()).collect();
        let due_of: HashMap<JobId, u128> = std::iter::zip(jobs.iter().copied(), dues).collect();
        for (&job_id, &due) in std::iter::zip(&jobs, &dues) {
            heap.upsert(job_id, ElapsedInstant::from_nanos(due));
        }
        assert!(heap.remove(jobs[9]));

        // Public behavior stays correct: every remaining deadline drains in order.
        let drained = heap.pop_due(ElapsedInstant::from_nanos(u128::MAX));
        let mut expected: Vec<u128> = dues.to_vec();
        expected.retain(|&due| due != 200);
        expected.sort_unstable();
        let drained_dues: Vec<u128> = drained.into_iter().map(|job_id| due_of[&job_id]).collect();
        assert_eq!(drained_dues, expected);
        assert!(heap.is_empty());
    }

    #[test]
    fn ordering_ties_updates_and_removal_are_deterministic() {
        let mut heap = DeadlineHeap::default();
        // Fixed IDs so both arms of the tie-break comparison are exercised.
        let first = JobId::from_u128(2);
        let second = JobId::from_u128(1);
        let (lower, higher) = (second, first);

        heap.upsert(first, ElapsedInstant::from_nanos(20));
        heap.upsert(second, ElapsedInstant::from_nanos(10));
        heap.upsert(first, ElapsedInstant::from_nanos(10));
        assert_eq!(
            heap.pop_due(ElapsedInstant::from_nanos(10)),
            vec![lower, higher]
        );

        heap.upsert(first, ElapsedInstant::from_nanos(30));
        heap.upsert(second, ElapsedInstant::from_nanos(40));
        heap.upsert(second, ElapsedInstant::from_nanos(20));
        assert!(heap.remove(first));
        assert!(!heap.remove(first));
        assert_eq!(heap.next_deadline(), Some(ElapsedInstant::from_nanos(20)));
    }

    #[test]
    fn remove_last_element_and_sift_down_after_removal() {
        let mut heap = DeadlineHeap::default();
        let root = JobId::new();
        let left = JobId::new();
        let right = JobId::new();
        // Removing the root when it is the only entry takes the early-return
        // path; the remaining removals exercise the sift-down replacement.
        heap.upsert(root, ElapsedInstant::from_nanos(10));
        assert!(heap.remove(root));
        heap.upsert(left, ElapsedInstant::from_nanos(10));
        heap.upsert(right, ElapsedInstant::from_nanos(20));
        assert_eq!(
            heap.pop_due(ElapsedInstant::from_nanos(20)),
            vec![left, right]
        );
        assert!(heap.is_empty());
    }

    #[test]
    fn removing_the_root_moves_the_last_entry_into_place() {
        let mut heap = DeadlineHeap::default();
        let root = JobId::new();
        let later = JobId::new();
        heap.upsert(root, ElapsedInstant::from_nanos(10));
        heap.upsert(later, ElapsedInstant::from_nanos(20));

        // The popped last entry lands at index 0, so only sift-down applies.
        assert!(heap.remove(root));
        assert!(heap.pop_due(ElapsedInstant::from_nanos(15)).is_empty());
        assert_eq!(heap.pop_due(ElapsedInstant::from_nanos(20)), vec![later]);
        assert!(heap.is_empty());
    }

    #[test]
    fn ten_thousand_jobs_batch_without_stale_entries() {
        use std::collections::HashSet;

        let mut heap = DeadlineHeap::default();
        let mut jobs = Vec::with_capacity(10_000);
        for due in (1_u128..=10_000).rev() {
            let job_id = JobId::new();
            jobs.push(job_id);
            heap.upsert(job_id, ElapsedInstant::from_nanos(due));
        }
        for &job_id in &jobs {
            heap.upsert(job_id, ElapsedInstant::from_nanos(20_000));
        }

        assert!(heap.pop_due(ElapsedInstant::from_nanos(10_000)).is_empty());
        let drained = heap.pop_due(ElapsedInstant::from_nanos(20_000));
        assert_eq!(drained.len(), jobs.len());
        assert_eq!(
            drained.into_iter().collect::<HashSet<_>>(),
            jobs.into_iter().collect::<HashSet<_>>()
        );
        assert!(heap.is_empty());
    }
}
