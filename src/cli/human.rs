//! Human-readable output.

use std::io::IsTerminal;

use super::args::ColorArg;
use super::view::{JobView, ProcessView, RunView, SubmissionView};
use crate::application::{DiagnosticStatus, DoctorReport, RunOutput, ServiceChange, ServiceStatus};
use crate::domain::{JobState, RunOutcome, Schedule, UtcTimestamp};

pub(crate) struct HumanRenderer {
    color: bool,
}

impl HumanRenderer {
    pub(crate) fn new(color: ColorArg) -> Self {
        Self {
            color: match color {
                ColorArg::Always => true,
                ColorArg::Never => false,
                ColorArg::Auto => std::io::stdout().is_terminal(),
            },
        }
    }

    pub(crate) fn submission(&self, view: &SubmissionView) -> String {
        let prefix = if view.dry_run { "Dry run" } else { "Scheduled" };
        format!(
            "{prefix} {} for {} ({})",
            view.job_id,
            local_timestamp(view.next_due_utc),
            self.state(view.state)
        )
    }

    pub(crate) fn jobs(&self, jobs: &[JobView]) -> String {
        if jobs.is_empty() {
            return "No jobs.".to_owned();
        }
        let mut lines = vec!["JOB\tSTATE\tDUE\tREMAINING\tNAME".to_owned()];
        lines.extend(jobs.iter().map(|job| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                job.job_id,
                self.state(job.state),
                local_timestamp(job.next_due_utc),
                remaining(job.remaining_seconds),
                job.name.as_deref().unwrap_or("-")
            )
        }));
        lines.join("\n")
    }

    /// `show` adds the most recent run's outcome; callers without a run to
    /// show pass `None`.
    pub(crate) fn job_with_outcome(
        &self,
        job: &JobView,
        last_outcome: Option<crate::domain::RunOutcome>,
    ) -> String {
        let environment = if job.execution.environment_keys.is_empty() {
            "-".to_owned()
        } else {
            job.execution.environment_keys.join(", ")
        };
        let name = job.name.as_deref().unwrap_or("-");
        let description = job.description.as_deref().unwrap_or("-");
        let outcome =
            last_outcome.map_or_else(|| "-".to_owned(), |outcome| format_outcome(&outcome));
        format!(
            "Job: {}\nName: {name}\nDescription: {description}\nState: {}\nDue: {}\nRemaining: {}\nSchedule: {}\nRuntime: {:?}\nLast outcome: {outcome}\nCommand: {}\nWorking directory: {}\nEnvironment keys: {}",
            job.job_id,
            self.state(job.state),
            local_timestamp(job.next_due_utc),
            remaining(job.remaining_seconds),
            schedule(&job.schedule),
            job.runtime_tier,
            display_argv(&job.execution.argv),
            job.execution.working_directory,
            environment
        )
    }

    pub(crate) fn runs(runs: &[RunView]) -> String {
        if runs.is_empty() {
            return "No runs.".to_owned();
        }
        let mut lines = vec!["RUN\tJOB\tSTATE\tSCHEDULED\tFINISHED".to_owned()];
        lines.extend(runs.iter().map(|run| {
            format!(
                "{}\t{}\t{:?}\t{}\t{}",
                run.run_id,
                run.job_id,
                run.state,
                local_timestamp(run.scheduled_for_utc),
                run.finished_at_utc
                    .map_or_else(|| "-".to_owned(), local_timestamp)
            )
        }));
        lines.join("\n")
    }

    pub(crate) fn processes(processes: &[ProcessView]) -> String {
        if processes.is_empty() {
            return "No live ATX processes.".to_owned();
        }
        let mut lines = vec!["JOB\tRUN\tROLE\tPID\tPGID\tSTATE".to_owned()];
        lines.extend(processes.iter().map(|process| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{:?}",
                process.job_id,
                process.run_id,
                process.role,
                process.pid,
                process.process_group_id,
                process.state
            )
        }));
        lines.join("\n")
    }

    pub(crate) fn run_output(output: &RunOutput) -> String {
        let snapshot_header = format!(
            "Run: {}\nJob: {}\nState: {:?}\nOutcome: {}",
            output.run_id,
            output.job_id,
            output.state,
            output
                .outcome
                .as_ref()
                .map_or_else(|| "-".to_owned(), format_outcome)
        );
        let mut sections = vec![snapshot_header];
        for (label, stream) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            let marker = if stream.truncated {
                " (truncated at capture cap)"
            } else {
                ""
            };
            sections.push(format!("--- {label}{marker} ---"));
            sections.push(String::from_utf8_lossy(&stream.content).into_owned());
        }
        sections.join("\n")
    }

    pub(crate) fn doctor(&self, report: &DoctorReport) -> String {
        let mut lines = vec![if report.healthy {
            "ATX is ready.".to_owned()
        } else {
            "ATX needs attention.".to_owned()
        }];
        lines.extend(report.checks.iter().map(|check| {
            let marker = match check.status {
                DiagnosticStatus::Pass => "ok",
                DiagnosticStatus::Warning => "warn",
                DiagnosticStatus::Fail => "fail",
            };
            let marker = if self.color {
                let color = match check.status {
                    DiagnosticStatus::Pass => 32,
                    DiagnosticStatus::Warning => 33,
                    DiagnosticStatus::Fail => 31,
                };
                format!("\x1b[{color}m{marker}\x1b[0m")
            } else {
                marker.to_owned()
            };
            format!("[{marker}] {}: {}", check.name, check.message)
        }));
        lines.push(format!("tzdb: {}", report.tzdb_version));
        lines.join("\n")
    }

    pub(crate) fn service_status(status: &ServiceStatus) -> String {
        let files = status
            .files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Manager: {}\nInstalled: {}\nRunning: {}\nFiles: {}\nGuarantee: {}\nStatus: {}",
            status.manager,
            status.installed,
            status.running,
            if files.is_empty() { "-" } else { &files },
            status.guarantee,
            status.detail
        )
    }

    pub(crate) fn service_change(change: &ServiceChange) -> String {
        let action = if change.changed {
            "Service changed."
        } else {
            "No change needed."
        };
        format!("{action}\n{}", Self::service_status(&change.status))
    }

    fn state(&self, state: JobState) -> String {
        let text = format!("{state:?}");
        if !self.color {
            return text;
        }
        let color = match state {
            JobState::Succeeded => 32,
            JobState::Failed | JobState::Interrupted | JobState::Missed => 31,
            JobState::Cancelled => 33,
            _ => 36,
        };
        format!("\x1b[{color}m{text}\x1b[0m")
    }
}

fn remaining(seconds: i64) -> String {
    if seconds <= 0 {
        return "due".to_owned();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        return format!("{}m {}s", seconds / 60, seconds % 60);
    }
    format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
}

fn local_timestamp(timestamp: UtcTimestamp) -> String {
    timestamp
        .as_jiff()
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

fn format_outcome(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Exit(code) => format!("exit {code}"),
        RunOutcome::Signal(signal) => signal_name(*signal).map_or_else(
            || format!("signal {signal}"),
            |name| format!("signal {signal} ({name})"),
        ),
        RunOutcome::Failure(reason) => format!("failure: {reason}"),
        RunOutcome::Interrupted(reason) => format!("interrupted: {reason}"),
        RunOutcome::Cancelled(reason) => format!("cancelled: {reason}"),
    }
}

fn signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        libc::SIGHUP => Some("SIGHUP"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGQUIT => Some("SIGQUIT"),
        libc::SIGKILL => Some("SIGKILL"),
        libc::SIGTERM => Some("SIGTERM"),
        _ => None,
    }
}

fn schedule(value: &Schedule) -> String {
    match value {
        Schedule::OneShotRelative { duration, .. } => format!("after {duration}"),
        Schedule::OneShotAbsolute {
            original_input,
            timezone,
            ..
        } => format!("{original_input} ({timezone})"),
        Schedule::RecurringInterval { interval, .. } => format!("every {interval}"),
    }
}

fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if argument
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_./".contains(&byte))
            {
                argument.clone()
            } else {
                format!("'{}'", argument.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{HumanRenderer, display_argv, format_outcome, local_timestamp};
    use crate::application::{
        DiagnosticCheck, DiagnosticStatus, DoctorReport, RunOutput, RunStream, ServiceAvailability,
        ServiceStatus,
    };
    use crate::cli::args::ColorArg;
    use crate::domain::{JobId, JobState, RunId, RunOutcome, RunState, UtcTimestamp};

    #[test]
    fn command_display_keeps_each_argument_visible() {
        let rendered = display_argv(&["printf".to_owned(), "hello world".to_owned()]);
        assert!(rendered.contains("printf"), "{rendered}");
        assert!(rendered.contains("hello world"), "{rendered}");
        assert_ne!(
            rendered, "printf hello world",
            "arguments must stay distinct"
        );
    }

    #[test]
    fn timestamps_and_outcomes_are_plain_human_text() {
        let rendered =
            local_timestamp(UtcTimestamp::from_second(1_700_000_000).expect("valid timestamp"));
        let mut parts = rendered.split_whitespace();
        let date = parts.next().expect("local date");
        let time = parts.next().expect("local time");
        let offset = parts.next().expect("UTC offset");
        assert!(parts.next().is_none(), "{rendered}");
        assert_eq!(date.matches('-').count(), 2, "{rendered}");
        assert_eq!(time.matches(':').count(), 2, "{rendered}");
        assert!(offset.starts_with(['+', '-']), "{rendered}");
        assert!(offset.contains(':'), "{rendered}");

        for (outcome, detail) in [
            (RunOutcome::Exit(0), "0"),
            (RunOutcome::Signal(libc::SIGKILL), "SIGKILL"),
            (RunOutcome::Signal(999), "999"),
            (RunOutcome::Failure("spawn".to_owned()), "spawn"),
            (RunOutcome::Interrupted("unknown".to_owned()), "unknown"),
            (RunOutcome::Cancelled("requested".to_owned()), "requested"),
        ] {
            let rendered = format_outcome(&outcome);
            assert!(rendered.contains(detail), "{rendered}");
        }
    }

    #[test]
    fn explicit_color_modes_do_not_depend_on_a_tty() {
        assert!(
            HumanRenderer::new(ColorArg::Always)
                .state(JobState::Scheduled)
                .contains("\u{1b}[")
        );
        assert!(
            !HumanRenderer::new(ColorArg::Never)
                .state(JobState::Scheduled)
                .contains("\u{1b}[")
        );
    }

    #[test]
    fn run_output_marks_truncated_streams() {
        let output = RunOutput {
            run_id: RunId::new(),
            job_id: JobId::new(),
            state: RunState::Failed,
            outcome: None,
            stdout: RunStream {
                content: b"kept".to_vec(),
                truncated: true,
            },
            stderr: RunStream::empty(),
        };
        let rendered = HumanRenderer::run_output(&output);
        assert!(rendered.contains("kept"), "{rendered}");
        assert!(rendered.contains("truncated"), "{rendered}");
    }

    #[test]
    fn doctor_reports_unhealthy_and_colored_markers() {
        let report = DoctorReport {
            healthy: false,
            checks: vec![],
            tzdb_version: "test".to_owned(),
            schema_version: None,
            durable_available: true,
            config: serde_json::Value::Null,
        };
        let unhealthy = HumanRenderer::new(ColorArg::Never).doctor(&report);

        let mut healthy = report.clone();
        healthy.healthy = true;
        healthy.checks = vec![
            DiagnosticCheck {
                name: "store".to_owned(),
                status: DiagnosticStatus::Pass,
                message: "open".to_owned(),
                remediation: None,
            },
            DiagnosticCheck {
                name: "tz".to_owned(),
                status: DiagnosticStatus::Fail,
                message: "missing".to_owned(),
                remediation: None,
            },
        ];
        let rendered = HumanRenderer::new(ColorArg::Always).doctor(&healthy);
        assert_ne!(unhealthy, rendered);
        assert!(rendered.contains("store"), "{rendered}");
        assert!(rendered.contains("tz"), "{rendered}");
        assert!(rendered.contains("open"), "{rendered}");
        assert!(rendered.contains("missing"), "{rendered}");
        assert!(rendered.contains("\u{1b}["), "{rendered}");
    }

    #[test]
    fn service_status_keeps_supplied_details_visible() {
        let status = ServiceStatus {
            manager: "fake".to_owned(),
            availability: ServiceAvailability::Available,
            installed: true,
            running: false,
            files: Vec::new(),
            guarantee: "best-effort".to_owned(),
            detail: "test fixture".to_owned(),
        };
        let rendered = HumanRenderer::service_status(&status);
        for detail in ["fake", "best-effort", "test fixture"] {
            assert!(rendered.contains(detail), "{rendered}");
        }
    }
}
