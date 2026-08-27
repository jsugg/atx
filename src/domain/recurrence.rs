//! Relative and fixed-rate deadline arithmetic.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::duration::DurationSeconds;
use super::primitives::UtcTimestamp;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ElapsedInstant(u128);

impl ElapsedInstant {
    pub(crate) const fn from_nanos(nanoseconds: u128) -> Self {
        Self(nanoseconds)
    }

    pub(crate) const fn as_nanos(self) -> u128 {
        self.0
    }

    pub(crate) fn checked_add_seconds(
        self,
        duration: DurationSeconds,
    ) -> Result<Self, DeadlineError> {
        let nanoseconds = u128::from(duration.get())
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(DeadlineError::Overflow)?;
        self.0
            .checked_add(nanoseconds)
            .map(Self)
            .ok_or(DeadlineError::Overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RelativeDeadline {
    elapsed_due: ElapsedInstant,
    persisted_due_utc: UtcTimestamp,
}

impl RelativeDeadline {
    #[cfg(test)]
    pub(crate) const fn elapsed_due(self) -> ElapsedInstant {
        self.elapsed_due
    }

    pub(crate) const fn persisted_due_utc(self) -> UtcTimestamp {
        self.persisted_due_utc
    }

    #[cfg(test)]
    pub(crate) const fn is_due(self, now: ElapsedInstant) -> bool {
        now.0 >= self.elapsed_due.0
    }
}

pub(crate) fn relative_deadline(
    wall_now: UtcTimestamp,
    elapsed_now: ElapsedInstant,
    duration: DurationSeconds,
) -> Result<RelativeDeadline, DeadlineError> {
    let seconds = i64::try_from(duration.get()).map_err(|_| DeadlineError::Overflow)?;
    let persisted_due_utc = wall_now
        .as_jiff()
        .checked_add(SignedDuration::from_secs(seconds))
        .map(UtcTimestamp::from_jiff)
        .map_err(|_| DeadlineError::Overflow)?;
    let elapsed_due = elapsed_now.checked_add_seconds(duration)?;
    Ok(RelativeDeadline {
        elapsed_due,
        persisted_due_utc,
    })
}

#[cfg(test)]
pub(crate) fn next_fixed_rate(
    anchor: ElapsedInstant,
    now: ElapsedInstant,
    interval: DurationSeconds,
) -> Result<ElapsedInstant, DeadlineError> {
    if now < anchor {
        return Ok(anchor);
    }

    let interval_nanos = u128::from(interval.get())
        .checked_mul(NANOS_PER_SECOND)
        .ok_or(DeadlineError::Overflow)?;
    let elapsed = now
        .as_nanos()
        .checked_sub(anchor.as_nanos())
        .ok_or(DeadlineError::Overflow)?;
    let periods = elapsed
        .checked_div(interval_nanos)
        .and_then(|value| value.checked_add(1))
        .ok_or(DeadlineError::Overflow)?;
    let offset = periods
        .checked_mul(interval_nanos)
        .ok_or(DeadlineError::Overflow)?;
    anchor
        .as_nanos()
        .checked_add(offset)
        .map(ElapsedInstant::from_nanos)
        .ok_or(DeadlineError::Overflow)
}

pub(crate) fn next_fixed_rate_utc(
    anchor: UtcTimestamp,
    now: UtcTimestamp,
    interval: DurationSeconds,
) -> Result<UtcTimestamp, DeadlineError> {
    if now < anchor {
        return Ok(anchor);
    }

    let interval_nanos = i128::from(interval.get())
        .checked_mul(1_000_000_000)
        .ok_or(DeadlineError::Overflow)?;
    let anchor_nanos = anchor.as_jiff().as_nanosecond();
    let elapsed = now
        .as_jiff()
        .as_nanosecond()
        .checked_sub(anchor_nanos)
        .ok_or(DeadlineError::Overflow)?;
    let periods = elapsed
        .checked_div(interval_nanos)
        .and_then(|value| value.checked_add(1))
        .ok_or(DeadlineError::Overflow)?;
    let next = anchor_nanos
        .checked_add(
            periods
                .checked_mul(interval_nanos)
                .ok_or(DeadlineError::Overflow)?,
        )
        .ok_or(DeadlineError::Overflow)?;
    Timestamp::from_nanosecond(next)
        .map(UtcTimestamp::from_jiff)
        .map_err(|_| DeadlineError::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum DeadlineError {
    #[error("deadline arithmetic overflowed")]
    Overflow,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use proptest::prelude::*;

    use super::{ElapsedInstant, next_fixed_rate, next_fixed_rate_utc, relative_deadline};
    use crate::domain::duration::{DurationSeconds, MAX_DURATION_SECONDS};
    use crate::domain::primitives::UtcTimestamp;

    #[test]
    fn wall_jump_does_not_change_live_relative_deadline() {
        let wall = UtcTimestamp::from_second(1_000).expect("valid timestamp");
        let elapsed = ElapsedInstant::from_nanos(50_000_000_000);
        let deadline = relative_deadline(
            wall,
            elapsed,
            DurationSeconds::new(30).expect("valid duration"),
        )
        .expect("valid deadline");

        let _jumped_wall = UtcTimestamp::from_second(100_000).expect("valid timestamp");
        assert_eq!(
            deadline.elapsed_due(),
            ElapsedInstant::from_nanos(80_000_000_000)
        );
        assert_eq!(
            deadline.persisted_due_utc().to_string(),
            "1970-01-01T00:17:10Z"
        );
        assert!(!deadline.is_due(ElapsedInstant::from_nanos(79_999_999_999)));
        assert!(deadline.is_due(ElapsedInstant::from_nanos(80_000_000_000)));
    }

    #[test]
    fn recurrence_stays_anchored_and_skips_missed_intervals() {
        let anchor = ElapsedInstant::from_nanos(10_000_000_000);
        let interval = DurationSeconds::new(5).expect("valid duration");
        assert_eq!(
            next_fixed_rate(anchor, ElapsedInstant::from_nanos(21_000_000_000), interval),
            Ok(ElapsedInstant::from_nanos(25_000_000_000))
        );
    }

    #[test]
    fn recurrence_boundary_is_strictly_after_now() {
        let anchor = ElapsedInstant::from_nanos(10_000_000_000);
        let interval = DurationSeconds::new(5).expect("valid duration");

        assert_eq!(
            next_fixed_rate(anchor, ElapsedInstant::from_nanos(9_999_999_999), interval,),
            Ok(anchor)
        );
        assert_eq!(
            next_fixed_rate(anchor, anchor, interval),
            Ok(ElapsedInstant::from_nanos(15_000_000_000))
        );
    }

    #[test]
    fn suspend_advance_makes_relative_deadline_due() {
        let wall = UtcTimestamp::from_second(1_000).expect("valid timestamp");
        let before_suspend = ElapsedInstant::from_nanos(10_000_000_000);
        let deadline = relative_deadline(
            wall,
            before_suspend,
            DurationSeconds::new(30).expect("valid duration"),
        )
        .expect("valid deadline");

        let after_suspend = ElapsedInstant::from_nanos(3_610_000_000_000);
        assert!(deadline.is_due(after_suspend));
    }

    #[test]
    fn large_missed_count_uses_constant_time_arithmetic() {
        let anchor = ElapsedInstant::from_nanos(0);
        let now = ElapsedInstant::from_nanos(1_000_000_000_000_000_000_000);
        let interval = DurationSeconds::new(1).expect("valid duration");
        assert!(
            next_fixed_rate(anchor, now, interval).expect("large count remains in range") > now
        );
    }

    #[test]
    fn reboot_recovery_uses_persisted_utc_anchor() {
        let anchor = UtcTimestamp::from_second(1_000).expect("valid timestamp");
        let now = UtcTimestamp::from_second(1_021).expect("valid timestamp");
        let interval = DurationSeconds::new(5).expect("valid duration");
        assert_eq!(
            next_fixed_rate_utc(anchor, now, interval)
                .expect("next deadline")
                .to_string(),
            "1970-01-01T00:17:05Z"
        );
    }

    #[test]
    fn persisted_recurrence_boundary_is_strictly_after_now() {
        let before_anchor = UtcTimestamp::from_second(999).expect("valid timestamp");
        let anchor = UtcTimestamp::from_second(1_000).expect("valid timestamp");
        let interval = DurationSeconds::new(5).expect("valid duration");

        assert_eq!(
            next_fixed_rate_utc(anchor, before_anchor, interval),
            Ok(anchor)
        );
        assert_eq!(
            next_fixed_rate_utc(anchor, anchor, interval)
                .expect("next deadline")
                .to_string(),
            "1970-01-01T00:16:45Z"
        );
    }

    proptest! {
        #[test]
        fn next_occurrence_is_strictly_future(
            interval in 1_u64..=MAX_DURATION_SECONDS,
            elapsed in 0_u64..=u64::MAX,
        ) {
            let anchor = ElapsedInstant::from_nanos(0);
            let now = ElapsedInstant::from_nanos(u128::from(elapsed));
            let interval = DurationSeconds::new(interval).expect("generated in range");
            let next = next_fixed_rate(anchor, now, interval).expect("bounded arithmetic");
            prop_assert!(next > now);
        }
    }
}
