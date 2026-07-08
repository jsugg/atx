//! Bounded command output capture.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureBudget {
    cap: usize,
    written: usize,
    discarded: usize,
}

impl CaptureBudget {
    pub(crate) const fn new(cap: usize) -> Result<Self, CaptureBudgetError> {
        if cap == 0 {
            return Err(CaptureBudgetError);
        }
        Ok(Self {
            cap,
            written: 0,
            discarded: 0,
        })
    }

    pub(crate) fn consume(&mut self, bytes: usize) -> (usize, usize) {
        let remaining = self.cap.saturating_sub(self.written);
        let written = remaining.min(bytes);
        let discarded = bytes - written;
        self.written = self.written.saturating_add(written);
        self.discarded = self.discarded.saturating_add(discarded);
        (written, discarded)
    }

    pub(crate) const fn written(self) -> usize {
        self.written
    }

    pub(crate) const fn discarded(self) -> usize {
        self.discarded
    }

    pub(crate) const fn truncated(self) -> bool {
        self.discarded > 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureBudgetError;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::CaptureBudget;

    #[test]
    fn cap_boundary_tracks_written_and_discarded_bytes() {
        let mut budget = CaptureBudget::new(5).expect("valid cap");
        assert_eq!(budget.consume(3), (3, 0));
        assert_eq!(budget.consume(2), (2, 0));
        assert_eq!(budget.consume(1), (0, 1));
        assert_eq!(budget.written(), 5);
        assert_eq!(budget.discarded(), 1);
        assert!(budget.truncated());
        assert!(CaptureBudget::new(0).is_err());
    }
}
