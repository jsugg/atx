//! Platform-independent scheduling domain.

// Domain contracts are built before their application services are wired.
#[allow(dead_code)]
mod calendar;
#[allow(dead_code)]
mod calendar_syntax;
#[allow(dead_code)]
mod duration;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod execution;
#[allow(dead_code)]
mod id;
#[allow(dead_code)]
mod job;
#[allow(dead_code)]
mod primitives;
#[allow(dead_code)]
mod recurrence;
#[allow(dead_code)]
mod run;
#[allow(dead_code)]
mod schedule;
#[allow(dead_code)]
mod state;
#[allow(dead_code)]
mod transition;

#[allow(unused_imports)]
pub(crate) use calendar::{
    CalendarResolution, TimeZoneSelection, bundled_tzdb_version, resolve_calendar,
};
#[allow(unused_imports)]
pub(crate) use calendar_syntax::{CalendarSyntax, parse_calendar};
#[allow(unused_imports)]
pub(crate) use execution::{Environment, ExecutionMode, ExecutionSpec};
pub(crate) use id::{JobId, RunId};
pub(crate) use job::{Job, JobSnapshot};
pub(crate) use primitives::{Description, Name, Revision, Sequence, UtcTimestamp};
pub(crate) use recurrence::{ElapsedInstant, next_fixed_rate_utc, relative_deadline};
pub(crate) use run::{ClaimToken, ProcessIdentitySnapshot, Run, RunOutcome, RunSnapshot};
pub(crate) use schedule::{DstResolution, DurationSeconds, MissedPolicy, RuntimeTier, Schedule};
pub(crate) use state::{JobState, RunState};
pub(crate) use transition::TransitionActor;
