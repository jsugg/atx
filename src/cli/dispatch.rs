//! Top-level command dispatch.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::Serialize;

use super::args::{
    ColorArg, DstArg, ManagementCommand, MissedArg, ParsedCli, SchedulingArgs, parse_from,
};
use super::exit;
use crate::application::{
    ElapsedClock, SubmissionOutcome, SubmissionStore, SubmissionStoreError, SupervisorAckError,
    SupervisorAcknowledger, WallClock, submit_job,
};
use crate::domain::{
    CalendarSyntax, Description, DstResolution, DurationSeconds, Environment, ExecutionMode,
    ExecutionSpec, Job, MissedPolicy, Name, RuntimeTier, Schedule, TimeZoneSelection, UtcTimestamp,
    parse_calendar, relative_deadline, resolve_calendar,
};
use crate::infrastructure::config::{ColorMode, Config, ConfigOverrides, Verbosity, load_config};
use crate::infrastructure::paths::{PathEnvironment, Platform, ensure_private_dir, resolve_paths};
use crate::infrastructure::runtime::start_session_supervisor;
use crate::infrastructure::sqlite::{Database, JobStore};
use crate::infrastructure::time::NativeClock;
use crate::supervisor::{SocketAcknowledger, run_session_supervisor};

const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let parsed = match parse_from(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let exit_code = if error.use_stderr() {
                exit::usage()
            } else {
                ExitCode::SUCCESS
            };
            let _ = error.print();
            return exit_code;
        }
    };
    match parsed {
        ParsedCli::Schedule(args) => run_schedule(&args),
        ParsedCli::Management { global, command } => run_management(global.json, &command),
    }
}

fn run_management(json: bool, command: &ManagementCommand) -> ExitCode {
    match command {
        ManagementCommand::Version => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"version": env!("CARGO_PKG_VERSION")})
                );
            } else {
                println!("atx {}", env!("CARGO_PKG_VERSION"));
            }
            ExitCode::SUCCESS
        }
        ManagementCommand::Supervisor {
            state_dir,
            runtime_dir,
        } => match run_session_supervisor(state_dir, runtime_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("atx supervisor: {error}");
                exit::supervision()
            }
        },
        _ => {
            print_error(
                json,
                "CAPABILITY_UNAVAILABLE",
                "This command is not wired yet.",
            );
            exit::capability()
        }
    }
}

fn run_schedule(args: &SchedulingArgs) -> ExitCode {
    match schedule(args) {
        Ok((outcome, json, quiet)) => {
            render_submission(&outcome, json, quiet);
            if matches!(outcome, SubmissionOutcome::CommittedUnsupervised { .. }) {
                exit::supervision()
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            print_error(error.json, error.code, &error.message);
            error.exit
        }
    }
}

fn schedule(args: &SchedulingArgs) -> Result<(SubmissionOutcome, bool, bool), CliError> {
    let json = args.global.json;
    let quiet = args.global.quiet;
    if args.options.capture_env && !quiet {
        eprintln!("Warning: --capture-env may save credentials in the state database.");
    }
    let paths = platform_paths(args.global.state_dir.as_deref())
        .map_err(|error| CliError::permission(json, error))?;
    let config =
        load_effective_config(&paths, args).map_err(|error| CliError::usage(json, error))?;
    let clock = NativeClock;
    let wall_now = clock
        .now_utc()
        .map_err(|error| CliError::internal(json, error))?;
    let elapsed_now = clock
        .now_elapsed()
        .map_err(|error| CliError::internal(json, error))?;
    let job = build_job(args, &config, wall_now, elapsed_now)
        .map_err(|error| CliError::usage(json, error))?;

    if job.snapshot().runtime_tier == RuntimeTier::Durable {
        return Err(CliError::capability(
            json,
            "durable runtime is not available on this installation",
        ));
    }

    if args.options.dry_run {
        let mut store = DryRunBoundary;
        let outcome = submit_job(&mut store, &DryRunBoundary, job, true)
            .map_err(|error| CliError::internal(json, error))?;
        return Ok((outcome, json, quiet));
    }

    ensure_private_dir(paths.state_dir()).map_err(|error| CliError::permission(json, error))?;
    let database = Database::open(&paths.state_dir().join("atx.db"), Duration::from_secs(2))
        .map_err(|error| CliError::storage(json, error))?;
    let mut store = JobStore::new(database);
    let acknowledger =
        SessionAcknowledger::new(paths.state_dir().to_owned(), paths.runtime_dir().to_owned());
    let outcome = submit_job(&mut store, &acknowledger, job, false)
        .map_err(|error| CliError::storage(json, error))?;
    Ok((outcome, json, quiet))
}

fn platform_paths(
    state_override: Option<&Path>,
) -> Result<crate::infrastructure::paths::PlatformPaths, impl std::fmt::Display> {
    let environment = PathEnvironment {
        home: std::env::var_os("HOME").map(PathBuf::from),
        xdg_state_home: std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        temporary_directory: Some(std::env::temp_dir()),
    };
    #[cfg(target_os = "linux")]
    let platform = Platform::Linux;
    #[cfg(target_os = "macos")]
    let platform = Platform::MacOs;
    resolve_paths(
        platform,
        &environment,
        state_override,
        rustix::process::geteuid().as_raw(),
    )
}

fn load_effective_config(
    paths: &crate::infrastructure::paths::PlatformPaths,
    args: &SchedulingArgs,
) -> Result<Config, String> {
    let config_path = paths.state_dir().join("config.toml");
    let file = match fs::read_to_string(&config_path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", config_path.display())),
    };
    let environment = std::env::vars().collect::<Vec<_>>();
    let overrides = ConfigOverrides {
        default_runtime: args.options.durable.then_some(RuntimeTier::Durable),
        color: Some(match args.global.color {
            ColorArg::Auto => ColorMode::Auto,
            ColorArg::Always => ColorMode::Always,
            ColorArg::Never => ColorMode::Never,
        }),
        verbosity: Some(if args.global.quiet {
            Verbosity::Quiet
        } else if args.global.verbose > 0 {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }),
        ..ConfigOverrides::default()
    };
    load_config(file.as_deref(), &environment, overrides).map_err(|error| error.to_string())
}

fn build_job(
    args: &SchedulingArgs,
    config: &Config,
    wall_now: UtcTimestamp,
    elapsed_now: crate::domain::ElapsedInstant,
) -> Result<Job, String> {
    let recurring = args.options.every.is_some();
    let schedule = build_schedule(args, config, wall_now, elapsed_now)?;
    let missed_policy = args.options.missed.map_or(
        if recurring {
            MissedPolicy::Skip
        } else {
            MissedPolicy::Hold
        },
        map_missed,
    );
    let runtime_tier = if args.options.durable {
        RuntimeTier::Durable
    } else {
        config.default_runtime()
    };
    let execution = build_execution(args, config)?;
    let mut job = Job::new(
        wall_now,
        schedule,
        missed_policy,
        runtime_tier,
        execution,
        rustix::process::geteuid().as_raw(),
    )
    .map_err(|error| error.to_string())?;
    let name = args
        .options
        .name
        .clone()
        .map(Name::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let description = args
        .options
        .description
        .clone()
        .map(Description::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    job.set_metadata(name, description);
    Ok(job)
}

fn build_schedule(
    args: &SchedulingArgs,
    config: &Config,
    wall_now: UtcTimestamp,
    elapsed_now: crate::domain::ElapsedInstant,
) -> Result<Schedule, String> {
    if let Some(every) = &args.options.every {
        if args.options.utc
            || args.options.tz.is_some()
            || args.options.dst.is_some()
            || args.options.no_rollover
        {
            return Err("calendar options cannot be used with --every".to_owned());
        }
        let interval = every
            .parse::<DurationSeconds>()
            .map_err(|error| error.to_string())?;
        let deadline = relative_deadline(wall_now, elapsed_now, interval)
            .map_err(|error| error.to_string())?;
        return Ok(Schedule::RecurringInterval {
            interval,
            persisted_anchor_utc: deadline.persisted_due_utc(),
        });
    }

    let joined_duration = args.when.concat();
    if let Ok(duration) = joined_duration.parse::<DurationSeconds>() {
        if args.options.utc
            || args.options.tz.is_some()
            || args.options.dst.is_some()
            || args.options.no_rollover
        {
            return Err("calendar options cannot be used with a relative duration".to_owned());
        }
        let deadline = relative_deadline(wall_now, elapsed_now, duration)
            .map_err(|error| error.to_string())?;
        return Ok(Schedule::one_shot_relative(
            duration,
            deadline.persisted_due_utc(),
        ));
    }

    let input = args.when.join(" ");
    let syntax = parse_calendar(&input).map_err(|error| error.to_string())?;
    if args.options.no_rollover && !matches!(syntax, CalendarSyntax::Time(_)) {
        return Err("--no-rollover is only valid with a time of day".to_owned());
    }
    if args.options.utc && args.options.dst.is_some() {
        return Err("--dst is not meaningful with --utc".to_owned());
    }
    let timezone = if args.options.utc {
        TimeZoneSelection::Utc
    } else if let Some(timezone) = &args.options.tz {
        TimeZoneSelection::Named(timezone.clone())
    } else if config.default_timezone() == "local" {
        TimeZoneSelection::Local
    } else {
        TimeZoneSelection::Named(config.default_timezone().to_owned())
    };
    let resolution = resolve_calendar(
        &input,
        &timezone,
        args.options.dst.map_or(DstResolution::Reject, map_dst),
        args.options.no_rollover,
        wall_now,
    )
    .map_err(|error| error.to_string())?;
    Ok(Schedule::one_shot_absolute(
        resolution.original_input().to_owned(),
        resolution.timezone().to_owned(),
        resolution.timezone_database_version().to_owned(),
        resolution.resolved_utc(),
        resolution.dst_resolution(),
    ))
}

fn build_execution(args: &SchedulingArgs, config: &Config) -> Result<ExecutionSpec, String> {
    let command_args = args
        .argv
        .iter()
        .map(|value| {
            value
                .clone()
                .into_string()
                .map_err(|_| "command arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mode = if args.options.shell {
        ExecutionMode::Shell
    } else {
        ExecutionMode::Direct
    };
    let cwd = match &args.options.cwd {
        Some(path) => fs::canonicalize(path)
            .map_err(|error| format!("cannot use working directory {}: {error}", path.display()))?,
        None => std::env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|error| format!("cannot resolve current directory: {error}"))?,
    };
    if !cwd.is_dir() {
        return Err(format!(
            "working directory {} is not a directory",
            cwd.display()
        ));
    }
    let environment = build_environment(args)?;
    let mut execution = ExecutionSpec::new(
        mode,
        command_args,
        cwd.to_string_lossy().into_owned(),
        environment,
    )
    .map_err(|error| error.to_string())?;
    if mode == ExecutionMode::Shell {
        execution
            .set_shell_path(config.default_shell().to_owned())
            .map_err(|error| error.to_string())?;
    }
    Ok(execution)
}

fn build_environment(args: &SchedulingArgs) -> Result<Environment, String> {
    let mut values = BTreeMap::<String, String>::new();
    if args.options.capture_env {
        values.extend(std::env::vars());
    } else {
        for (key, value) in std::env::vars() {
            if matches!(
                key.as_str(),
                "HOME" | "USER" | "LOGNAME" | "PATH" | "LANG" | "TMPDIR"
            ) || key.starts_with("LC_")
            {
                values.insert(key, value);
            }
        }
    }
    if let Some(path) = &args.options.env_file {
        values.extend(read_env_file(path)?);
    }
    for assignment in &args.options.env {
        let (key, value) = split_assignment(assignment)?;
        values.insert(key.to_owned(), value.to_owned());
    }
    Environment::from_pairs(values).map_err(|error| error.to_string())
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_ENV_FILE_BYTES {
        return Err("environment file must be a regular file no larger than 1 MiB".to_owned());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if contents.contains('\0') {
        return Err("environment file cannot contain NUL".to_owned());
    }
    let mut values = BTreeMap::new();
    let mut seen = HashSet::new();
    for (index, line) in contents.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) =
            split_assignment(line).map_err(|error| format!("line {}: {error}", index + 1))?;
        if !seen.insert(key.to_owned()) {
            return Err(format!("line {} repeats environment key {key}", index + 1));
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    Environment::from_pairs(values.clone()).map_err(|error| error.to_string())?;
    Ok(values)
}

fn split_assignment(value: &str) -> Result<(&str, &str), String> {
    value
        .split_once('=')
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| "environment value must use KEY=VALUE".to_owned())
}

const fn map_dst(value: DstArg) -> DstResolution {
    match value {
        DstArg::Reject => DstResolution::Reject,
        DstArg::Earlier => DstResolution::Earlier,
        DstArg::Later => DstResolution::Later,
    }
}

const fn map_missed(value: MissedArg) -> MissedPolicy {
    match value {
        MissedArg::Hold => MissedPolicy::Hold,
        MissedArg::RunLatest => MissedPolicy::RunLatest,
        MissedArg::Skip => MissedPolicy::Skip,
    }
}

#[derive(Serialize)]
struct SubmissionView<'a> {
    job_id: String,
    state: crate::domain::JobState,
    schedule: &'a Schedule,
    next_due_utc: String,
    runtime_tier: RuntimeTier,
    supervised: bool,
}

fn render_submission(outcome: &SubmissionOutcome, json: bool, quiet: bool) {
    let snapshot = outcome.job().snapshot();
    let view = SubmissionView {
        job_id: snapshot.id.to_string(),
        state: snapshot.state,
        schedule: &snapshot.schedule,
        next_due_utc: snapshot.next_due_utc.to_string(),
        runtime_tier: snapshot.runtime_tier,
        supervised: outcome.is_supervised(),
    };
    if json {
        if let Ok(value) = serde_json::to_string(&view) {
            println!("{value}");
        }
    } else if !quiet {
        let prefix = if outcome.is_dry_run() {
            "Dry run:"
        } else {
            "Scheduled"
        };
        println!("{prefix} {} for {}", view.job_id, view.next_due_utc);
    }
    if let SubmissionOutcome::CommittedUnsupervised { error, .. } = outcome {
        eprintln!("Job was saved, but {error}");
    }
}

fn print_error(json: bool, code: &str, message: &str) {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({"error": {"code": code, "message": message}})
        );
    } else {
        eprintln!("atx: {message}");
    }
}

struct DryRunBoundary;

impl SubmissionStore for DryRunBoundary {
    fn create_job(&mut self, _job: &Job) -> Result<(), SubmissionStoreError> {
        Err(SubmissionStoreError(
            "dry-run crossed its storage boundary".to_owned(),
        ))
    }
}

impl SupervisorAcknowledger for DryRunBoundary {
    fn acknowledge(
        &self,
        _job_id: crate::domain::JobId,
        _revision: crate::domain::Revision,
    ) -> Result<(), SupervisorAckError> {
        Err(SupervisorAckError(
            "dry-run crossed its supervisor boundary".to_owned(),
        ))
    }
}

struct SessionAcknowledger {
    socket: SocketAcknowledger,
    state_directory: PathBuf,
    runtime_directory: PathBuf,
}

impl SessionAcknowledger {
    fn new(state_directory: PathBuf, runtime_directory: PathBuf) -> Self {
        Self {
            socket: SocketAcknowledger::new(runtime_directory.join("supervisor.sock"), ACK_TIMEOUT),
            state_directory,
            runtime_directory,
        }
    }
}

impl SupervisorAcknowledger for SessionAcknowledger {
    fn acknowledge(
        &self,
        job_id: crate::domain::JobId,
        revision: crate::domain::Revision,
    ) -> Result<(), SupervisorAckError> {
        if self.socket.acknowledge(job_id, revision).is_ok() {
            return Ok(());
        }
        start_session_supervisor(&self.state_directory, &self.runtime_directory)
            .map_err(|error| SupervisorAckError(error.to_string()))?;
        let mut last_error = SupervisorAckError("supervisor did not become ready".to_owned());
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            match self.socket.acknowledge(job_id, revision) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

#[derive(Debug)]
struct CliError {
    exit: ExitCode,
    code: &'static str,
    message: String,
    json: bool,
}

impl CliError {
    fn usage(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::usage(), "INVALID_ARGUMENT", json, error)
    }

    fn capability(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::capability(), "CAPABILITY_UNAVAILABLE", json, error)
    }

    fn storage(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::storage(), "STORAGE_ERROR", json, error)
    }

    fn permission(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::permission(), "PERMISSION_ERROR", json, error)
    }

    fn internal(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::internal(), "INTERNAL_ERROR", json, error)
    }

    fn new(exit: ExitCode, code: &'static str, json: bool, error: impl std::fmt::Display) -> Self {
        Self {
            exit,
            code,
            message: error.to_string(),
            json,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use std::ffi::OsString;
    use std::fs;

    use tempfile::tempdir;

    use super::{build_job, read_env_file, schedule};
    use crate::application::{ElapsedClock, WallClock};
    use crate::cli::args::{ParsedCli, parse_from};
    use crate::infrastructure::config::{ConfigOverrides, load_config};
    use crate::infrastructure::time::NativeClock;

    #[test]
    fn schedule_matrix_builds_relative_calendar_and_recurring_jobs() {
        let config = load_config(None, &[], ConfigOverrides::default()).expect("config");
        let clock = NativeClock;
        let wall = clock.now_utc().expect("wall");
        let elapsed = clock.now_elapsed().expect("elapsed");
        for args in [
            vec!["atx", "30s", "--", "printf", "%s", "hello"],
            vec!["atx", "--utc", "23:59:59", "--", "true"],
            vec!["atx", "--every", "5m", "--", "true"],
        ] {
            let ParsedCli::Schedule(args) =
                parse_from(args.into_iter().map(OsString::from)).expect("parse")
            else {
                unreachable!("schedule");
            };
            assert!(build_job(&args, &config, wall, elapsed).is_ok());
        }
    }

    #[test]
    fn shell_and_calendar_conflicts_fail_before_mutation() {
        let config = load_config(None, &[], ConfigOverrides::default()).expect("config");
        let clock = NativeClock;
        let wall = clock.now_utc().expect("wall");
        let elapsed = clock.now_elapsed().expect("elapsed");
        for args in [
            vec!["atx", "--shell", "30s", "--", "echo", "extra"],
            vec!["atx", "--utc", "--dst", "earlier", "23:59", "--", "true"],
            vec!["atx", "--no-rollover", "2099-01-01", "--", "true"],
        ] {
            let ParsedCli::Schedule(args) =
                parse_from(args.into_iter().map(OsString::from)).expect("parse")
            else {
                unreachable!("schedule");
            };
            assert!(build_job(&args, &config, wall, elapsed).is_err());
        }
    }

    #[test]
    fn env_file_rejects_duplicates_and_accepts_comments() {
        let root = tempdir().expect("root");
        let valid = root.path().join("valid.env");
        fs::write(&valid, "# note\nA=1\n\nB=two=parts\n").expect("write");
        let values = read_env_file(&valid).expect("environment");
        assert_eq!(values.get("B").map(String::as_str), Some("two=parts"));

        let duplicate = root.path().join("duplicate.env");
        fs::write(&duplicate, "A=1\nA=2\n").expect("write");
        assert!(read_env_file(&duplicate).is_err());
    }

    #[test]
    fn dry_run_does_not_create_state_or_database_files() {
        let root = tempdir().expect("root");
        let state = root.path().join("state");
        let ParsedCli::Schedule(args) = parse_from([
            OsString::from("atx"),
            OsString::from("--state-dir"),
            state.clone().into_os_string(),
            OsString::from("--dry-run"),
            OsString::from("30s"),
            OsString::from("--"),
            OsString::from("true"),
        ])
        .expect("parse") else {
            unreachable!("schedule");
        };

        let (outcome, _, _) = schedule(&args).expect("dry run");
        assert!(outcome.is_dry_run());
        assert!(!state.exists());
    }
}
