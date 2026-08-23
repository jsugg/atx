//! Scheduling value types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) use super::duration::DurationSeconds;
use super::primitives::UtcTimestamp;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeTier {
    Session,
    Durable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MissedPolicy {
    Hold,
    RunLatest,
    Skip,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DstResolution {
    Reject,
    Earlier,
    Later,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Schedule {
    OneShotRelative {
        duration: DurationSeconds,
        persisted_due_utc: UtcTimestamp,
    },
    OneShotAbsolute {
        original_input: String,
        timezone: String,
        timezone_database_version: String,
        resolved_due_utc: UtcTimestamp,
        dst_resolution: DstResolution,
    },
    RecurringInterval {
        interval: DurationSeconds,
        persisted_anchor_utc: UtcTimestamp,
    },
}

impl Schedule {
    pub(crate) fn one_shot_relative(
        duration: DurationSeconds,
        persisted_due_utc: UtcTimestamp,
    ) -> Self {
        Self::OneShotRelative {
            duration,
            persisted_due_utc,
        }
    }

    pub(crate) fn one_shot_absolute(
        original_input: String,
        timezone: String,
        timezone_database_version: String,
        resolved_due_utc: UtcTimestamp,
        dst_resolution: DstResolution,
    ) -> Self {
        Self::OneShotAbsolute {
            original_input,
            timezone,
            timezone_database_version,
            resolved_due_utc,
            dst_resolution,
        }
    }

    pub(crate) const fn next_due_utc(&self) -> UtcTimestamp {
        match self {
            Self::OneShotRelative {
                persisted_due_utc, ..
            } => *persisted_due_utc,
            Self::OneShotAbsolute {
                resolved_due_utc, ..
            } => *resolved_due_utc,
            Self::RecurringInterval {
                persisted_anchor_utc,
                ..
            } => *persisted_anchor_utc,
        }
    }

    pub(crate) fn timezone_database_version(&self) -> &str {
        match self {
            Self::OneShotAbsolute {
                timezone_database_version,
                ..
            } => timezone_database_version,
            Self::OneShotRelative { .. } | Self::RecurringInterval { .. } => "not-applicable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum ScheduleError {
    #[error("the resolved deadline must be in the future")]
    DeadlineNotFuture,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{DstResolution, DurationSeconds, Schedule};
    use crate::domain::primitives::UtcTimestamp;

    #[test]
    fn every_schedule_reports_its_next_due_and_tzdb() {
        let due = UtcTimestamp::from_second(2_000).expect("valid timestamp");
        let relative =
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("duration"), due);
        assert_eq!(relative.next_due_utc(), due);
        assert_eq!(relative.timezone_database_version(), "not-applicable");

        let absolute = Schedule::one_shot_absolute(
            "2030-01-01".to_owned(),
            "UTC".to_owned(),
            "test".to_owned(),
            due,
            DstResolution::Reject,
        );
        assert_eq!(absolute.next_due_utc(), due);
        assert_eq!(absolute.timezone_database_version(), "test");

        let recurring = Schedule::RecurringInterval {
            interval: DurationSeconds::new(60).expect("duration"),
            persisted_anchor_utc: due,
        };
        assert_eq!(recurring.next_due_utc(), due);
        assert_eq!(recurring.timezone_database_version(), "not-applicable");
    }
}
