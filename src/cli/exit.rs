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
    #![allow(clippy::expect_used)]

    use super::*;

    // The numeric values are a documented stability contract (README and
    // docs): scripts match on them, so remapping silently would break users.
    #[test]
    fn exit_codes_are_stable_across_releases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/exit-codes-v1.json"))
                .expect("valid exit-code fixture");
        let expected = [
            ("success", ExitCode::SUCCESS),
            ("usage", usage()),
            ("not_found", not_found()),
            ("conflict", conflict()),
            ("capability", capability()),
            ("storage", storage()),
            ("supervision", supervision()),
            ("permission", permission()),
            ("internal", internal()),
        ];
        assert_eq!(
            fixture.as_object().map(serde_json::Map::len),
            Some(expected.len())
        );
        for (name, actual) in expected {
            let value = fixture[name].as_u64().expect("numeric fixture value");
            assert_eq!(
                actual,
                ExitCode::from(u8::try_from(value).expect("u8 exit code"))
            );
        }
    }
}
