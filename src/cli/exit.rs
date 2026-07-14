//! Stable process exit-code mapping.

use std::process::ExitCode;

pub(crate) fn usage() -> ExitCode {
    ExitCode::from(2)
}

pub(crate) fn capability() -> ExitCode {
    ExitCode::from(5)
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
