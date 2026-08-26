//! Calendar timezone resolution.

use jiff::civil::{DateTime, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use serde::Serialize;
use thiserror::Error;

use super::calendar_syntax::{CalendarSyntax, CalendarSyntaxError, parse_calendar};
use super::primitives::UtcTimestamp;
use super::schedule::DstResolution;

const MAX_TIMEZONE_BYTES: usize = 255;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TimeZoneSelection {
    Local,
    Utc,
    Named(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CalendarResolution {
    original_input: String,
    timezone: String,
    timezone_database_version: String,
    resolved_utc: UtcTimestamp,
    dst_resolution: DstResolution,
}

impl CalendarResolution {
    pub(crate) fn original_input(&self) -> &str {
        &self.original_input
    }

    pub(crate) fn timezone(&self) -> &str {
        &self.timezone
    }

    pub(crate) fn timezone_database_version(&self) -> &str {
        &self.timezone_database_version
    }

    pub(crate) const fn resolved_utc(&self) -> UtcTimestamp {
        self.resolved_utc
    }

    pub(crate) const fn dst_resolution(&self) -> DstResolution {
        self.dst_resolution
    }
}

pub(crate) fn bundled_tzdb_version() -> &'static str {
    jiff_tzdb::VERSION.unwrap_or("unknown")
}

pub(crate) fn resolve_calendar(
    input: &str,
    selection: &TimeZoneSelection,
    dst_resolution: DstResolution,
    no_rollover: bool,
    now: UtcTimestamp,
) -> Result<CalendarResolution, CalendarError> {
    let parsed = parse_calendar(input).map_err(CalendarError::Syntax)?;
    let (time_zone, zone_name) = resolve_time_zone(selection)?;
    let resolved_utc = match parsed {
        CalendarSyntax::Time(time) => {
            resolve_time_only(time, &time_zone, dst_resolution, no_rollover, now)?
        }
        CalendarSyntax::Date(date) => {
            let candidate = date.to_datetime(Time::midnight());
            require_future(
                resolve_datetime(candidate, &time_zone, dst_resolution)?,
                now,
            )?
        }
        CalendarSyntax::DateTime(candidate) => require_future(
            resolve_datetime(candidate, &time_zone, dst_resolution)?,
            now,
        )?,
    };

    Ok(CalendarResolution {
        original_input: input.to_owned(),
        timezone: zone_name,
        timezone_database_version: bundled_tzdb_version().to_owned(),
        resolved_utc,
        dst_resolution,
    })
}

fn resolve_time_zone(selection: &TimeZoneSelection) -> Result<(TimeZone, String), CalendarError> {
    let requested = match selection {
        TimeZoneSelection::Utc => "UTC".to_owned(),
        TimeZoneSelection::Named(name) => {
            if name.len() > MAX_TIMEZONE_BYTES
                || !name.is_ascii()
                || name.contains('\0')
                || name.is_empty()
            {
                return Err(CalendarError::InvalidTimeZone);
            }
            name.clone()
        }
        TimeZoneSelection::Local => {
            let system = TimeZone::try_system().map_err(|_| CalendarError::LocalTimeZoneUnknown)?;
            system
                .iana_name()
                .map(str::to_owned)
                .ok_or(CalendarError::LocalTimeZoneUnknown)?
        }
    };

    let (canonical, data) = jiff_tzdb::get(&requested).ok_or(CalendarError::TimeZoneNotFound)?;
    let time_zone = TimeZone::tzif(canonical, data).map_err(|_| CalendarError::TimeZoneNotFound)?;
    Ok((time_zone, canonical.to_owned()))
}

fn resolve_time_only(
    time: Time,
    time_zone: &TimeZone,
    dst_resolution: DstResolution,
    no_rollover: bool,
    now: UtcTimestamp,
) -> Result<UtcTimestamp, CalendarError> {
    let today = now.as_jiff().to_zoned(time_zone.clone()).date();
    let candidate = resolve_datetime(today.to_datetime(time), time_zone, dst_resolution)?;
    if candidate > now {
        return Ok(candidate);
    }
    if no_rollover {
        return Err(CalendarError::RolloverDisabled);
    }

    let tomorrow = today
        .tomorrow()
        .map_err(|_| CalendarError::ResolutionOutOfRange)?;
    resolve_datetime(tomorrow.to_datetime(time), time_zone, dst_resolution)
}

fn resolve_datetime(
    date_time: DateTime,
    time_zone: &TimeZone,
    dst_resolution: DstResolution,
) -> Result<UtcTimestamp, CalendarError> {
    let ambiguous = time_zone.to_ambiguous_zoned(date_time);
    let zoned = match ambiguous.offset() {
        AmbiguousOffset::Gap { .. } => return Err(CalendarError::NonexistentLocalTime),
        AmbiguousOffset::Fold { .. } => match dst_resolution {
            DstResolution::Reject => return Err(CalendarError::AmbiguousLocalTime),
            DstResolution::Earlier => ambiguous.earlier(),
            DstResolution::Later => ambiguous.later(),
        },
        AmbiguousOffset::Unambiguous { .. } => ambiguous.unambiguous(),
    }
    .map_err(|_| CalendarError::ResolutionOutOfRange)?;

    Ok(UtcTimestamp::from_jiff(zoned.timestamp()))
}

fn require_future(
    candidate: UtcTimestamp,
    now: UtcTimestamp,
) -> Result<UtcTimestamp, CalendarError> {
    if candidate <= now {
        return Err(CalendarError::NotFuture);
    }
    Ok(candidate)
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum CalendarError {
    #[error(transparent)]
    Syntax(CalendarSyntaxError),
    #[error("timezone identifier must be 1..=255 ASCII bytes")]
    InvalidTimeZone,
    #[error("timezone identifier was not found in the bundled database")]
    TimeZoneNotFound,
    #[error("the system local timezone cannot be mapped to an IANA identifier")]
    LocalTimeZoneUnknown,
    #[error("local time does not exist because of a timezone transition")]
    NonexistentLocalTime,
    #[error("local time occurs twice; choose earlier or later")]
    AmbiguousLocalTime,
    #[error("resolved calendar time is outside the supported range")]
    ResolutionOutOfRange,
    #[error("resolved calendar time must be in the future")]
    NotFuture,
    #[error("time has passed today and rollover is disabled")]
    RolloverDisabled,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::str::FromStr;

    use super::{TimeZoneSelection, bundled_tzdb_version, resolve_calendar};
    use crate::domain::primitives::UtcTimestamp;
    use crate::domain::schedule::DstResolution;

    #[test]
    fn time_only_rolls_over_at_exact_now() {
        let now = UtcTimestamp::from_str("2026-07-30T15:00:00Z").expect("valid timestamp");
        let resolved = resolve_calendar(
            "15:00",
            &TimeZoneSelection::Utc,
            DstResolution::Reject,
            false,
            now,
        )
        .expect("rollover should resolve");
        assert_eq!(resolved.resolved_utc().to_string(), "2026-07-31T15:00:00Z");

        assert!(
            resolve_calendar(
                "15:00",
                &TimeZoneSelection::Utc,
                DstResolution::Reject,
                true,
                now,
            )
            .is_err()
        );
    }

    #[test]
    fn new_york_gap_is_always_rejected() {
        let now = UtcTimestamp::from_str("2024-01-01T00:00:00Z").expect("valid timestamp");
        for policy in [
            DstResolution::Reject,
            DstResolution::Earlier,
            DstResolution::Later,
        ] {
            assert!(
                resolve_calendar(
                    "2024-03-10T02:30",
                    &TimeZoneSelection::Named("America/New_York".to_owned()),
                    policy,
                    false,
                    now,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn new_york_fold_uses_explicit_choice() {
        let now = UtcTimestamp::from_str("2024-01-01T00:00:00Z").expect("valid timestamp");
        let earlier = resolve_calendar(
            "2024-11-03T01:30",
            &TimeZoneSelection::Named("America/New_York".to_owned()),
            DstResolution::Earlier,
            false,
            now,
        )
        .expect("earlier fold");
        let later = resolve_calendar(
            "2024-11-03T01:30",
            &TimeZoneSelection::Named("America/New_York".to_owned()),
            DstResolution::Later,
            false,
            now,
        )
        .expect("later fold");

        assert_eq!(earlier.resolved_utc().to_string(), "2024-11-03T05:30:00Z");
        assert_eq!(later.resolved_utc().to_string(), "2024-11-03T06:30:00Z");
    }

    #[test]
    fn persisted_resolution_records_bundled_version() {
        let now = UtcTimestamp::from_str("2026-01-01T00:00:00Z").expect("valid timestamp");
        let resolved = resolve_calendar(
            "2026-08-01T09:30",
            &TimeZoneSelection::Named("America/Sao_Paulo".to_owned()),
            DstResolution::Reject,
            false,
            now,
        )
        .expect("calendar should resolve");

        assert_eq!(resolved.timezone_database_version(), bundled_tzdb_version());
        assert_ne!(resolved.timezone_database_version(), "unknown");
        assert_eq!(resolved.original_input(), "2026-08-01T09:30");
        assert_eq!(resolved.timezone(), "America/Sao_Paulo");
        assert_eq!(resolved.dst_resolution(), DstResolution::Reject);
    }

    #[test]
    fn rejects_bad_zone_and_past_calendar_values() {
        let now = UtcTimestamp::from_str("2026-01-01T00:00:00Z").expect("valid timestamp");
        assert!(
            resolve_calendar(
                "2027-01-01",
                &TimeZoneSelection::Named("No/Such_Zone".to_owned()),
                DstResolution::Reject,
                false,
                now,
            )
            .is_err()
        );
        assert!(
            resolve_calendar(
                "2025-01-01",
                &TimeZoneSelection::Utc,
                DstResolution::Reject,
                false,
                now,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_timezone_selections() {
        let now = UtcTimestamp::from_str("2026-01-01T00:00:00Z").expect("valid timestamp");
        let over_cap = "A/".repeat(128);
        let past_cap = "A/".repeat(127) + "AB";
        let zones = [
            "",                      // empty
            "America/N\u{e9}w_York", // non-ASCII
            "America/New_York\0",    // embedded NUL
            over_cap.as_str(),       // over the 255-byte cap
            past_cap.as_str(),       // exactly 256 bytes: one past the cap
        ];
        for zone in zones {
            assert!(
                matches!(
                    resolve_calendar(
                        "2027-01-01T09:30",
                        &TimeZoneSelection::Named(zone.to_owned()),
                        DstResolution::Reject,
                        false,
                        now,
                    ),
                    Err(super::CalendarError::InvalidTimeZone)
                ),
                "{zone}"
            );
        }

        // Exactly 255 bytes passes the length gate (it fails lookup later,
        // with a different error) — this pins the >/>= boundary.
        let at_cap = "A/".repeat(127) + "B";
        assert_eq!(at_cap.len(), 255);
        assert!(matches!(
            resolve_calendar(
                "2027-01-01T09:30",
                &TimeZoneSelection::Named(at_cap),
                DstResolution::Reject,
                false,
                now,
            ),
            Err(super::CalendarError::TimeZoneNotFound)
        ));
    }

    proptest::proptest! {
        /// Every accepted resolution must map back to the exact wall clock
        /// that was asked for, in the selected zone. DST policies may reject
        /// ambiguous or skipped times, but an accepted instant may never land
        /// on a different civil time than the input.
        #[test]
        fn accepted_resolutions_round_trip_to_requested_wall_clock(
            day in 1u8..=28,
            month in 1u8..=12,
            hour in 0u8..=23,
            minute in proptest::prelude::prop::sample::select(vec![0u8, 15, 30, 45]),
            policy in proptest::prelude::prop::sample::select(vec![
                DstResolution::Reject,
                DstResolution::Earlier,
                DstResolution::Later,
            ]),
            zone in proptest::prelude::prop::sample::select(vec![
                "UTC",
                "America/New_York",
                "Europe/Berlin",
                "Australia/Lord_Howe",
            ]),
        ) {
            use proptest::prelude::*;
            use std::str::FromStr;

            let now = UtcTimestamp::from_str("2026-01-01T00:00:00Z").expect("valid timestamp");
            let input = format!("2026-{month:02}-{day:02}T{hour:02}:{minute:02}");
            let Ok(resolution) = resolve_calendar(
                &input,
                &TimeZoneSelection::Named(zone.to_owned()),
                policy,
                false,
                now,
            ) else {
                // Rejections are legal for gaps and, under Reject, folds.
                return Ok(());
            };

            let time_zone = jiff::tz::TimeZone::get(zone).expect("bundled zone");
            let instant = jiff::Timestamp::from_str(&resolution.resolved_utc().to_string())
                .expect("valid resolved timestamp");
            let civil = instant.to_zoned(time_zone).datetime();
            let expected = format!("2026-{month:02}-{day:02}T{hour:02}:{minute:02}:00");
            let detail = format!("{zone} resolved {instant} for input {input}");
            prop_assert_eq!(civil.to_string(), expected, "{}", detail);
        }
    }
}
