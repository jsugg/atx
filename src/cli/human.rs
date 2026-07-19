//! Human-readable output.

use std::io::IsTerminal;

use super::args::ColorArg;
use super::view::{JobView, ProcessView, RunView, SubmissionView};
use crate::application::{DiagnosticStatus, DoctorReport, ServiceChange, ServiceStatus};
use crate::domain::{JobState, Schedule};

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
            view.next_due_utc,
            self.state(view.state)
        )
    }

    pub(crate) fn jobs(&self, jobs: &[JobView]) -> String {
        let mut lines = vec!["JOB\tSTATE\tDUE\tREMAINING\tNAME".to_owned()];
        lines.extend(jobs.iter().map(|job| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                job.job_id,
                self.state(job.state),
                job.next_due_utc,
                remaining(job.remaining_seconds),
                job.name.as_deref().unwrap_or("-")
            )
        }));
        lines.join("\n")
    }

    pub(crate) fn job(&self, job: &JobView) -> String {
        let environment = if job.execution.environment_keys.is_empty() {
            "-".to_owned()
        } else {
            job.execution.environment_keys.join(", ")
        };
        format!(
            "Job: {}\nState: {}\nDue: {}\nRemaining: {}\nSchedule: {}\nRuntime: {:?}\nCommand: {}\nWorking directory: {}\nEnvironment keys: {}",
            job.job_id,
            self.state(job.state),
            job.next_due_utc,
            remaining(job.remaining_seconds),
            schedule(&job.schedule),
            job.runtime_tier,
            display_argv(&job.execution.argv),
            job.execution.working_directory,
            environment
        )
    }

    pub(crate) fn runs(runs: &[RunView]) -> String {
        let mut lines = vec!["RUN\tJOB\tSTATE\tSCHEDULED\tFINISHED".to_owned()];
        lines.extend(runs.iter().map(|run| {
            format!(
                "{}\t{}\t{:?}\t{}\t{}",
                run.run_id,
                run.job_id,
                run.state,
                run.scheduled_for_utc,
                run.finished_at_utc.as_deref().unwrap_or("-")
            )
        }));
        lines.join("\n")
    }

    pub(crate) fn processes(processes: &[ProcessView]) -> String {
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
    use super::{HumanRenderer, display_argv, remaining};
    use crate::cli::args::ColorArg;
    use crate::domain::JobState;

    #[test]
    fn remaining_time_is_short_and_readable() {
        assert_eq!(remaining(-1), "due");
        assert_eq!(remaining(42), "42s");
        assert_eq!(remaining(90), "1m 30s");
        assert_eq!(remaining(3_900), "1h 5m");
    }

    #[test]
    fn command_display_quotes_without_changing_argv() {
        assert_eq!(
            display_argv(&["printf".to_owned(), "hello world".to_owned()]),
            "printf 'hello world'"
        );
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
}
