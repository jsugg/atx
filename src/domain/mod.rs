//! Platform-independent scheduling domain.

mod calendar;
mod calendar_syntax;
mod duration;
mod execution;
mod id;
mod job;
mod primitives;
mod recurrence;
mod run;
mod schedule;
mod state;
mod transition;

pub(crate) use calendar::{TimeZoneSelection, bundled_tzdb_version, resolve_calendar};
pub(crate) use calendar_syntax::{CalendarSyntax, parse_calendar};
pub(crate) use execution::{Environment, ExecutionMode, ExecutionSpec};
pub(crate) use id::{JobId, RunId};
pub(crate) use job::{Job, JobSnapshot};
pub(crate) use primitives::{Description, Name, Revision, Sequence, UtcTimestamp};
pub(crate) use recurrence::{ElapsedInstant, next_fixed_rate_utc, relative_deadline};
pub(crate) use run::{ClaimToken, ProcessIdentitySnapshot, Run, RunOutcome, RunSnapshot};
pub(crate) use schedule::{DstResolution, DurationSeconds, MissedPolicy, RuntimeTier, Schedule};
pub(crate) use state::{JobState, RunState};
pub(crate) use transition::TransitionActor;
