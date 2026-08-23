//! Operational diagnostic report.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DiagnosticCheck {
    pub(crate) name: String,
    pub(crate) status: DiagnosticStatus,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remediation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) healthy: bool,
    pub(crate) checks: Vec<DiagnosticCheck>,
    pub(crate) tzdb_version: String,
    pub(crate) schema_version: Option<u32>,
    pub(crate) durable_available: bool,
    pub(crate) config: serde_json::Value,
}

#[derive(Default)]
pub(crate) struct DoctorReportBuilder {
    checks: Vec<DiagnosticCheck>,
}

impl DoctorReportBuilder {
    pub(crate) fn push(
        &mut self,
        name: impl Into<String>,
        status: DiagnosticStatus,
        message: impl Into<String>,
        remediation: Option<String>,
    ) {
        self.checks.push(DiagnosticCheck {
            name: name.into(),
            status,
            message: message.into(),
            remediation,
        });
    }

    pub(crate) fn finish(
        self,
        tzdb_version: String,
        schema_version: Option<u32>,
        durable_available: bool,
        config: serde_json::Value,
    ) -> DoctorReport {
        DoctorReport {
            healthy: !self
                .checks
                .iter()
                .any(|check| check.status == DiagnosticStatus::Fail),
            checks: self.checks,
            tzdb_version,
            schema_version,
            durable_available,
            config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DiagnosticStatus, DoctorReportBuilder};

    #[test]
    fn warnings_are_degraded_but_failures_make_report_unhealthy() {
        let mut degraded = DoctorReportBuilder::default();
        degraded.push("service", DiagnosticStatus::Warning, "unavailable", None);
        assert!(
            degraded
                .finish("test".to_owned(), None, false, serde_json::json!({}))
                .healthy
        );

        let mut failed = DoctorReportBuilder::default();
        failed.push("state", DiagnosticStatus::Fail, "wrong owner", None);
        assert!(
            !failed
                .finish("test".to_owned(), None, false, serde_json::json!({}))
                .healthy
        );
    }
}
