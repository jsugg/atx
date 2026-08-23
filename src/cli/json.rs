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
