//! Stable machine-readable output.

use serde::Serialize;

pub(crate) const SCHEMA_VERSION: u8 = 1;

#[derive(Serialize)]
struct SuccessEnvelope<'a, T> {
    schema_version: u8,
    ok: bool,
    data: &'a T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: u8,
    ok: bool,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation: Option<&'a str>,
}

pub(crate) fn success<T: Serialize>(data: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(&SuccessEnvelope {
        schema_version: SCHEMA_VERSION,
        ok: true,
        data,
    })
}

pub(crate) fn error(
    code: &str,
    message: &str,
    remediation: Option<&str>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ErrorEnvelope {
        schema_version: SCHEMA_VERSION,
        ok: false,
        error: ErrorBody {
            code,
            message,
            remediation,
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{error, success};
    use crate::application::{
        DiagnosticCheck, DiagnosticStatus, DoctorReport, ServiceAvailability, ServiceChange,
        ServiceStatus,
    };
    use crate::domain::{
        DstResolution, ExecutionMode, JobState, RunOutcome, RunState, RuntimeTier,
    };
    use crate::infrastructure::config::{ColorMode, Verbosity};

    #[test]
    fn public_enum_strings_are_stable() {
        fn expect_strings(values: &impl serde::Serialize, expected: &[&str]) {
            assert_eq!(
                serde_json::to_value(values).expect("enum JSON"),
                serde_json::json!(expected)
            );
        }
        expect_strings(
            &JobState::ALL,
            &[
                "scheduled",
                "waiting",
                "starting",
                "running",
                "cancel_requested",
                "succeeded",
                "failed",
                "cancelled",
                "interrupted",
                "missed",
            ],
        );
        expect_strings(
            &RunState::ALL,
            &[
                "starting",
                "running",
                "cancel_requested",
                "succeeded",
                "failed",
                "cancelled",
                "interrupted",
            ],
        );
        expect_strings(
            &[RuntimeTier::Session, RuntimeTier::Durable],
            &["session", "durable"],
        );
        expect_strings(
            &[ExecutionMode::Direct, ExecutionMode::Shell],
            &["direct", "shell"],
        );
        expect_strings(
            &[
                DstResolution::Reject,
                DstResolution::Earlier,
                DstResolution::Later,
            ],
            &["reject", "earlier", "later"],
        );
        expect_strings(
            &[
                DiagnosticStatus::Pass,
                DiagnosticStatus::Warning,
                DiagnosticStatus::Fail,
            ],
            &["pass", "warning", "fail"],
        );
        expect_strings(
            &[
                ServiceAvailability::Available,
                ServiceAvailability::Unavailable,
            ],
            &["available", "unavailable"],
        );
        expect_strings(
            &[ColorMode::Auto, ColorMode::Always, ColorMode::Never],
            &["auto", "always", "never"],
        );
        expect_strings(
            &[Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose],
            &["quiet", "normal", "verbose"],
        );
    }

    #[test]
    fn outcome_examples_match_all_tagged_variants() {
        let outcomes = [
            RunOutcome::Exit(0),
            RunOutcome::Signal(15),
            RunOutcome::Failure("command could not start".to_owned()),
            RunOutcome::Interrupted("outcome unknown after restart".to_owned()),
            RunOutcome::Cancelled("cancelled by user".to_owned()),
        ];
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/json-outcomes-v1.json"))
                .expect("outcome contract fixture");
        let actual = serde_json::to_value(&outcomes).expect("outcomes");
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::from_value::<Vec<RunOutcome>>(actual).expect("round trip"),
            outcomes
        );
        for value in [i32::MIN, -1, 1, i32::MAX] {
            assert_eq!(
                serde_json::to_value(RunOutcome::Exit(value)).expect("exit"),
                serde_json::json!({"kind":"exit", "value":value})
            );
        }
    }

    #[test]
    fn doctor_schema_version_and_optional_remediation_keep_their_types() {
        for schema_version in [None, Some(42)] {
            let report = DoctorReport {
                healthy: false,
                checks: vec![DiagnosticCheck {
                    name: "SQLite".to_owned(),
                    status: DiagnosticStatus::Fail,
                    message: "database is unreadable".to_owned(),
                    remediation: None,
                }],
                tzdb_version: "2026c".to_owned(),
                schema_version,
                durable_available: false,
                config: serde_json::json!({}),
            };
            let actual: serde_json::Value =
                serde_json::from_str(&success(&report).expect("report envelope")).expect("JSON");
            assert_eq!(
                actual,
                serde_json::json!({
                    "schema_version":1, "ok":true,
                    "data":{
                        "healthy":false,
                        "checks":[{"name":"SQLite", "status":"fail", "message":"database is unreadable"}],
                        "tzdb_version":"2026c", "schema_version":schema_version,
                        "durable_available":false, "config":{},
                    },
                })
            );
        }
    }

    #[test]
    fn service_change_payload_has_a_boolean_and_status_object() {
        for (availability, expected) in [
            (ServiceAvailability::Available, "available"),
            (ServiceAvailability::Unavailable, "unavailable"),
        ] {
            for changed in [false, true] {
                let change = ServiceChange {
                    changed,
                    status: ServiceStatus {
                        manager: "test manager".to_owned(),
                        availability,
                        installed: true,
                        running: false,
                        files: vec!["/tmp/test.service".into()],
                        guarantee: "test guarantee".to_owned(),
                        detail: "test detail".to_owned(),
                    },
                };
                assert_eq!(
                    serde_json::to_value(change).expect("service change"),
                    serde_json::json!({
                        "changed":changed,
                        "status":{
                            "manager":"test manager", "availability":expected, "installed":true,
                            "running":false, "files":["/tmp/test.service"],
                            "guarantee":"test guarantee", "detail":"test detail",
                        },
                    })
                );
            }
        }
    }

    #[test]
    fn envelopes_keep_schema_version_and_channels_stable() {
        let success: serde_json::Value =
            serde_json::from_str(&success(&vec![1, 2]).expect("serialize")).expect("parse");
        assert_eq!(success["schema_version"], 1);
        assert_eq!(success["ok"], true);
        assert_eq!(success["data"], serde_json::json!([1, 2]));

        let error: serde_json::Value = serde_json::from_str(
            &error("JOB_NOT_FOUND", "missing", Some("Run `atx list`.")).expect("serialize"),
        )
        .expect("parse");
        assert_eq!(error["schema_version"], 1);
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "JOB_NOT_FOUND");
    }
}
