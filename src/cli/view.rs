//! Output models shared by human and JSON renderers.

use serde::Serialize;

use crate::application::SubmissionOutcome;
use crate::domain::{
    ExecutionMode, Job, JobState, Run, RunOutcome, RunState, RuntimeTier, Schedule, UtcTimestamp,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SubmissionView {
    pub(crate) job_id: String,
    pub(crate) state: JobState,
    pub(crate) schedule: Schedule,
    pub(crate) next_due_utc: UtcTimestamp,
    pub(crate) runtime_tier: RuntimeTier,
    pub(crate) supervised: bool,
    pub(crate) dry_run: bool,
}

impl SubmissionView {
    pub(crate) fn from_outcome(outcome: &SubmissionOutcome) -> Self {
        let snapshot = outcome.job().snapshot();
        Self {
            job_id: snapshot.id.to_string(),
            state: snapshot.state,
            schedule: snapshot.schedule,
            next_due_utc: snapshot.next_due_utc,
            runtime_tier: snapshot.runtime_tier,
            supervised: outcome.is_supervised(),
            dry_run: outcome.is_dry_run(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct JobView {
    pub(crate) job_id: String,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) schedule: Schedule,
    pub(crate) next_due_utc: UtcTimestamp,
    pub(crate) remaining_seconds: i64,
    pub(crate) state: JobState,
    pub(crate) runtime_tier: RuntimeTier,
    pub(crate) execution: ExecutionView,
    pub(crate) active_run_id: Option<String>,
}

impl JobView {
    pub(crate) fn from_job(job: &Job, now: UtcTimestamp) -> Self {
        let snapshot = job.snapshot();
        let remaining = snapshot
            .next_due_utc
            .as_jiff()
            .duration_since(now.as_jiff())
            .as_nanos()
            / 1_000_000_000;
        Self {
            job_id: snapshot.id.to_string(),
            name: snapshot.name.map(|name| name.as_str().to_owned()),
            description: snapshot
                .description
                .map(|description| description.as_str().to_owned()),
            schedule: snapshot.schedule,
            next_due_utc: snapshot.next_due_utc,
            remaining_seconds: i64::try_from(remaining).unwrap_or(if remaining.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }),
            state: snapshot.state,
            runtime_tier: snapshot.runtime_tier,
            execution: ExecutionView::from_job(job),
            active_run_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ExecutionView {
    pub(crate) mode: ExecutionMode,
    pub(crate) argv: Vec<String>,
    pub(crate) working_directory: String,
    pub(crate) environment_keys: Vec<String>,
    pub(crate) shell_path: Option<String>,
}

impl ExecutionView {
    fn from_job(job: &Job) -> Self {
        let execution = job.execution();
        Self {
            mode: execution.mode(),
            argv: execution.argv().to_vec(),
            working_directory: execution.working_directory().to_string_lossy().into_owned(),
            environment_keys: execution
                .environment()
                .iter()
                .map(|(key, _)| key.to_owned())
                .collect(),
            shell_path: execution
                .shell_path()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RunView {
    pub(crate) run_id: String,
    pub(crate) job_id: String,
    pub(crate) sequence: u64,
    pub(crate) scheduled_for_utc: UtcTimestamp,
    pub(crate) started_at_utc: Option<UtcTimestamp>,
    pub(crate) finished_at_utc: Option<UtcTimestamp>,
    pub(crate) state: RunState,
    pub(crate) outcome: Option<RunOutcome>,
    pub(crate) stdout_path: Option<String>,
    pub(crate) stderr_path: Option<String>,
}

impl RunView {
    pub(crate) fn from_run(run: &Run) -> Self {
        let snapshot = run.snapshot();
        Self {
            run_id: snapshot.id.to_string(),
            job_id: snapshot.job_id.to_string(),
            sequence: snapshot.sequence.get(),
            scheduled_for_utc: snapshot.scheduled_for_utc,
            started_at_utc: snapshot.started_at_utc,
            finished_at_utc: snapshot.finished_at_utc,
            state: snapshot.state,
            outcome: snapshot.outcome,
            stdout_path: snapshot.stdout_path,
            stderr_path: snapshot.stderr_path,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProcessView {
    pub(crate) job_id: String,
    pub(crate) run_id: String,
    pub(crate) role: &'static str,
    pub(crate) pid: u32,
    pub(crate) process_group_id: i32,
    pub(crate) state: RunState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RunOutputView {
    pub(crate) run_id: String,
    pub(crate) job_id: String,
    pub(crate) state: RunState,
    pub(crate) outcome: Option<RunOutcome>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    /// Lossy UTF-8 of the captured stream; JSON consumers get text, raw
    /// bytes remain on disk under the logged paths.
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl RunOutputView {
    pub(crate) fn from_output(output: &crate::application::RunOutput) -> Self {
        Self {
            run_id: output.run_id.to_string(),
            job_id: output.job_id.to_string(),
            state: output.state,
            outcome: output.outcome.clone(),
            stdout_truncated: output.stdout.truncated,
            stderr_truncated: output.stderr.truncated,
            stdout: String::from_utf8_lossy(&output.stdout.content).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr.content).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::JobView;
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, MissedPolicy, RuntimeTier,
        Schedule, UtcTimestamp,
    };

    #[test]
    fn job_view_lists_environment_keys_without_values() {
        let now = UtcTimestamp::from_second(100).expect("timestamp");
        let job = Job::new(
            now,
            Schedule::one_shot_relative(
                DurationSeconds::new(30).expect("duration"),
                UtcTimestamp::from_second(130).expect("timestamp"),
            ),
            MissedPolicy::Hold,
            RuntimeTier::Session,
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["true".to_owned()],
                "/tmp".to_owned(),
                Environment::from_pairs([("TOKEN", "secret-value")]).expect("environment"),
            )
            .expect("execution"),
            501,
        )
        .expect("job");

        let json = serde_json::to_string(&JobView::from_job(&job, now)).expect("JSON");
        assert!(json.contains("TOKEN"));
        assert!(!json.contains("secret-value"));
    }
}
