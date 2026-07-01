//! Stable domain error taxonomy.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ErrorCode {
    InvalidIdentifier,
    InvalidValue,
    InvalidTransition,
    RevisionConflict,
    TimeOutOfRange,
    JobNotFound,
    AmbiguousJob,
    StorageFailure,
    ExecutionFailure,
    PlatformUnavailable,
    PermissionDenied,
    InternalFailure,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::ErrorCode;

    #[test]
    fn error_codes_have_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidIdentifier).expect("code should serialize"),
            "\"INVALID_IDENTIFIER\""
        );
    }
}
