//! Wall and suspend-aware elapsed clock ports.

use thiserror::Error;

use crate::domain::{ElapsedInstant, UtcTimestamp};

pub(crate) trait WallClock: Send + Sync {
    fn now_utc(&self) -> Result<UtcTimestamp, ClockError>;
}

/// Elapsed time must advance during system suspend on supported platforms.
pub(crate) trait ElapsedClock: Send + Sync {
    fn now_elapsed(&self) -> Result<ElapsedInstant, ClockError>;
    fn boot_identity(&self) -> Result<String, ClockError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ClockError {
    #[error("required platform clock is unavailable")]
    Unavailable,
    #[error("platform clock returned an out-of-range value")]
    OutOfRange,
}
