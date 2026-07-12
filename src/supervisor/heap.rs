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
    fn ordering_ties_updates_and_removal_are_deterministic() {
        let mut heap = DeadlineHeap::default();
        let first = JobId::new();
        let second = JobId::new();
        let (lower, higher) = if first < second {
            (first, second)
        } else {
            (second, first)
        };

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
    fn ten_thousand_jobs_batch_without_stale_entries() {
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

        assert_eq!(heap.entries.len(), 10_000);
        assert!(heap.pop_due(ElapsedInstant::from_nanos(10_000)).is_empty());
        assert_eq!(
            heap.pop_due(ElapsedInstant::from_nanos(20_000)).len(),
            10_000
        );
        assert!(heap.is_empty());
    }
}
