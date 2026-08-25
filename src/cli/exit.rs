//! Stable process exit-code mapping.

use std::process::ExitCode;

pub(crate) fn usage() -> ExitCode {
    ExitCode::from(2)
}

pub(crate) fn capability() -> ExitCode {
    ExitCode::from(5)
}

pub(crate) fn not_found() -> ExitCode {
    ExitCode::from(3)
}

pub(crate) fn conflict() -> ExitCode {
    ExitCode::from(4)
}

pub(crate) fn storage() -> ExitCode {
    ExitCode::from(10)
}

pub(crate) fn supervision() -> ExitCode {
    ExitCode::from(11)
}

pub(crate) fn permission() -> ExitCode {
    ExitCode::from(12)
}

pub(crate) fn internal() -> ExitCode {
    ExitCode::from(70)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The numeric values are a documented stability contract (README and
    // docs): scripts match on them, so remapping silently would break users.
    #[test]
    fn exit_codes_are_stable_across_releases() {
        assert_eq!(usage(), std::process::ExitCode::from(2));
        assert_eq!(not_found(), std::process::ExitCode::from(3));
        assert_eq!(conflict(), std::process::ExitCode::from(4));
        assert_eq!(capability(), std::process::ExitCode::from(5));
        assert_eq!(storage(), std::process::ExitCode::from(10));
        assert_eq!(supervision(), std::process::ExitCode::from(11));
        assert_eq!(permission(), std::process::ExitCode::from(12));
        assert_eq!(internal(), std::process::ExitCode::from(70));
    }
}
