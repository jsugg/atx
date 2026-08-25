//! Top-level command dispatch.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use super::args::{
    ColorArg, DstArg, GlobalArgs, ManagementCommand, MissedArg, ParsedCli, SchedulingArgs,
    ServiceAction, parse_from,
};
use super::exit;
use super::human::HumanRenderer;
use super::view::{JobView, ProcessView, RunOutputView, RunView, SubmissionView};
use crate::application::{
    CancelRunResult, DiagnosticStatus, DoctorReport, DoctorReportBuilder, ElapsedClock,
    ManagementError, ManagementStore, RunOutput, RunOutputError, ServiceManager, SubmissionOutcome,
    SubmissionStore, SubmissionStoreError, SupervisorAckError, SupervisorAcknowledger, WallClock,
    cancel_claimed_run, install_service, list_jobs, list_runs, read_run_output, remove_job,
    rerun_job, resolve_job, submit_job, uninstall_service,
};
use crate::domain::{
    CalendarSyntax, Description, DstResolution, DurationSeconds, Environment, ExecutionMode,
    ExecutionSpec, Job, JobState, MissedPolicy, Name, Run, RunOutcome, RunState, RuntimeTier,
    Schedule, TimeZoneSelection, TransitionActor, UtcTimestamp, parse_calendar, relative_deadline,
    resolve_calendar,
};
use crate::infrastructure::config::{ColorMode, Config, ConfigOverrides, Verbosity, load_config};
use crate::infrastructure::paths::{
    PathEnvironment, Platform, PlatformPaths, ensure_private_dir, resolve_paths,
    validate_private_dir_for_uid,
};
use crate::infrastructure::process::{
    IdentityStatus as NativeIdentityStatus, NativeGroupCanceller, NativeProcessInspector,
};
use crate::infrastructure::runtime::start_session_supervisor;
use crate::infrastructure::service::NativeServiceManager;
use crate::infrastructure::sqlite::{Database, JobStore, StoreError};
use crate::infrastructure::time::NativeClock;
use crate::run_monitor::run_monitor_process;
use crate::supervisor::{SocketAcknowledger, run_session_supervisor};

const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(10);
const SUPERVISOR_READY_POLL: Duration = Duration::from_millis(50);

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let args = args.into_iter().collect::<Vec<_>>();
    let json_requested = args
        .iter()
        .take_while(|value| value.as_os_str() != "--")
        .any(|value| value == "--json");
    let parsed = match parse_from(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let exit_code = if error.use_stderr() {
                exit::usage()
            } else {
                ExitCode::SUCCESS
            };
            if json_requested && error.use_stderr() {
                print_error(true, "INVALID_ARGUMENT", &error.to_string());
            } else {
                let _ = error.print();
            }
            return exit_code;
        }
    };
    match parsed {
        ParsedCli::Schedule(args) => run_schedule(&args),
        ParsedCli::Management { global, command } => run_management(&global, &command),
    }
}

fn generate_completions(shell: crate::cli::args::CompletionShell) {
    use clap_complete::{Shell, generate};
    use std::io::stdout;

    let shell = match shell {
        crate::cli::args::CompletionShell::Bash => Shell::Bash,
        crate::cli::args::CompletionShell::Zsh => Shell::Zsh,
        crate::cli::args::CompletionShell::Fish => Shell::Fish,
        crate::cli::args::CompletionShell::PowerShell => Shell::PowerShell,
    };
    // RawCli is the derive root; its Command impl carries every subcommand.
    let mut cmd = <crate::cli::args::RawCli as clap::CommandFactory>::command();
    generate(shell, &mut cmd, "atx", &mut stdout());
}

/// Write `atx.1` plus one page per subcommand as roff into `out_dir`.
#[cfg(feature = "man")]
fn export_man_pages(out_dir: &std::path::Path) -> Result<(), String> {
    use clap::CommandFactory;
    use std::io::Write;

    fn render(man: &clap_mangen::Man, path: &std::path::Path) -> Result<(), String> {
        let file = std::fs::File::create(path)
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        man.render(&mut writer)
            .map_err(|error| format!("render {}: {error}", path.display()))?;
        writer.flush().map_err(|error| format!("flush: {error}"))
    }

    std::fs::create_dir_all(out_dir).map_err(|error| format!("create dir: {error}"))?;
    let mut cmd = <crate::cli::args::RawCli as CommandFactory>::command();
    cmd.build();
    // mandoc rejects an empty or non-date TH date, so stamp a real date;
    // SOURCE_DATE_EPOCH keeps distro reproducible builds deterministic.
    let date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .and_then(|epoch| jiff::Timestamp::from_second(epoch).ok())
        .unwrap_or_else(jiff::Timestamp::now)
        .strftime("%Y-%m-%d")
        .to_string();
    let name = cmd.get_name().to_owned();
    render(
        &clap_mangen::Man::new(cmd.clone()).date(&date),
        &out_dir.join(format!("{name}.1")),
    )?;
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let path = out_dir.join(format!("{name}-{}.1", sub.get_name()));
        render(
            &clap_mangen::Man::new(sub.clone())
                .title(name.clone())
                .date(&date),
            &path,
        )?;
    }
    Ok(())
}

fn run_management(global: &GlobalArgs, command: &ManagementCommand) -> ExitCode {
    let json = global.json;
    match command {
        ManagementCommand::Version => {
            if json {
                print_json_success(&serde_json::json!({"version": env!("CARGO_PKG_VERSION")}));
            } else {
                println!("atx {}", env!("CARGO_PKG_VERSION"));
            }
            ExitCode::SUCCESS
        }
        ManagementCommand::Completions { shell } => {
            generate_completions(*shell);
            ExitCode::SUCCESS
        }
        #[cfg(feature = "man")]
        ManagementCommand::Man { out_dir } => match export_man_pages(out_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("atx __man: {error}");
                exit::internal()
            }
        },
        ManagementCommand::Supervisor {
            state_dir,
            runtime_dir,
            service_managed,
        } => match run_session_supervisor(state_dir, runtime_dir, *service_managed) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("atx supervisor: {error}");
                exit::supervision()
            }
        },
        ManagementCommand::Monitor {
            state_dir,
            runtime_dir,
            job,
            run,
        } => match run_monitor_process(state_dir, runtime_dir, job, run) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("atx monitor: {error}");
                exit::supervision()
            }
        },
        ManagementCommand::Doctor => run_doctor(global),
        ManagementCommand::Service { action } => run_service(global, *action),
        _ => match manage(global, command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                print_error(error.json, error.code, &error.message);
                error.exit
            }
        },
    }
}

fn run_service(global: &GlobalArgs, action: ServiceAction) -> ExitCode {
    match service_operation(global, action) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(error.json, error.code, &error.message);
            error.exit
        }
    }
}

fn service_operation(global: &GlobalArgs, action: ServiceAction) -> Result<(), CliError> {
    let paths = platform_paths(global.state_dir.as_deref())
        .map_err(|error| CliError::permission(global.json, error))?;
    if action == ServiceAction::Install {
        ensure_private_dir(paths.state_dir())
            .map_err(|error| CliError::permission(global.json, error))?;
        ensure_private_dir(paths.runtime_dir())
            .map_err(|error| CliError::permission(global.json, error))?;
    }
    let mut manager = native_service_manager(&paths, global.json)?;
    match action {
        ServiceAction::Status => {
            let status = manager
                .status()
                .map_err(|error| CliError::capability(global.json, error))?;
            if global.json {
                print_json_success(&status);
            } else if !global.quiet {
                println!("{}", HumanRenderer::service_status(&status));
            }
        }
        ServiceAction::Install => {
            let change = install_service(&mut manager)
                .map_err(|error| CliError::capability(global.json, error))?;
            if global.json {
                print_json_success(&change);
            } else if !global.quiet {
                println!("{}", HumanRenderer::service_change(&change));
            }
        }
        ServiceAction::Uninstall => {
            let change = uninstall_service(&mut manager)
                .map_err(|error| CliError::capability(global.json, error))?;
            if global.json {
                print_json_success(&change);
            } else if !global.quiet {
                println!("{}", HumanRenderer::service_change(&change));
            }
        }
    }
    Ok(())
}

fn native_service_manager(
    paths: &PlatformPaths,
    json: bool,
) -> Result<NativeServiceManager, CliError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| CliError::capability(json, "HOME is unavailable or not absolute"))?;
    let executable =
        fs::canonicalize(std::env::current_exe().map_err(|error| CliError::internal(json, error))?)
            .map_err(|error| CliError::capability(json, error))?;
    Ok(NativeServiceManager::detect(
        executable,
        paths.state_dir().to_owned(),
        paths.runtime_dir().to_owned(),
        &home,
        rustix::process::geteuid().as_raw(),
    ))
}

fn run_doctor(global: &GlobalArgs) -> ExitCode {
    match build_doctor_report(global) {
        Ok(report) => {
            if global.json {
                print_json_success(&report);
            } else if !global.quiet {
                println!("{}", HumanRenderer::new(global.color).doctor(&report));
            }
            doctor_exit(&report)
        }
        Err(error) => {
            print_error(error.json, error.code, &error.message);
            error.exit
        }
    }
}

fn build_doctor_report(global: &GlobalArgs) -> Result<DoctorReport, CliError> {
    let paths = platform_paths(global.state_dir.as_deref())
        .map_err(|error| CliError::permission(global.json, error))?;
    let uid = rustix::process::geteuid().as_raw();
    let mut builder = DoctorReportBuilder::default();
    check_private_directory(&mut builder, "state directory", paths.state_dir(), uid);
    check_private_directory(&mut builder, "runtime directory", paths.runtime_dir(), uid);

    let config = match load_effective_config(&paths, global, false) {
        Ok(config) => {
            builder.push(
                "configuration",
                DiagnosticStatus::Pass,
                "configuration is valid",
                None,
            );
            serde_json::to_value(config.redacted()).unwrap_or_else(|error| {
                builder.push(
                    "configuration output",
                    DiagnosticStatus::Fail,
                    error.to_string(),
                    None,
                );
                serde_json::json!({})
            })
        }
        Err(error) => {
            builder.push(
                "configuration",
                DiagnosticStatus::Fail,
                error,
                Some("Fix or remove the invalid config file.".to_owned()),
            );
            serde_json::json!({})
        }
    };

    let schema_version = check_database(&mut builder, paths.state_dir());
    let clock = NativeClock;
    let boot_identity = if let (Ok(_), Ok(_), Ok(identity)) =
        (clock.now_utc(), clock.now_elapsed(), clock.boot_identity())
    {
        builder.push(
            "clocks",
            DiagnosticStatus::Pass,
            "wall and suspend-aware elapsed clocks are available",
            None,
        );
        Some(identity)
    } else {
        builder.push(
            "clocks",
            DiagnosticStatus::Fail,
            "a required platform clock is unavailable",
            Some("This platform cannot safely supervise elapsed deadlines.".to_owned()),
        );
        None
    };
    if let Some(identity) = boot_identity.as_deref() {
        let inspector = NativeProcessInspector::new(identity.to_owned());
        match inspector.inspect(std::process::id()) {
            Ok(Some(_)) => builder.push(
                "process identity",
                DiagnosticStatus::Pass,
                "PID start identity is available",
                None,
            ),
            Ok(None) | Err(_) => builder.push(
                "process identity",
                DiagnosticStatus::Fail,
                "current process identity could not be validated",
                Some("Process-safe cancellation is unavailable.".to_owned()),
            ),
        }
        check_supervisor(&mut builder, paths.runtime_dir(), &inspector, uid);
    }
    let durable_available = check_durable_service(&mut builder, &paths, global.json);
    Ok(builder.finish(
        crate::domain::bundled_tzdb_version().to_owned(),
        schema_version,
        durable_available,
        config,
    ))
}

fn check_durable_service(
    builder: &mut DoctorReportBuilder,
    paths: &PlatformPaths,
    json: bool,
) -> bool {
    match native_service_manager(paths, json).and_then(|manager| {
        manager
            .status()
            .map_err(|error| CliError::capability(json, error))
    }) {
        Ok(status) if status.installed && status.running => {
            builder.push(
                "durable service",
                DiagnosticStatus::Pass,
                status.detail,
                None,
            );
            true
        }
        Ok(status) => {
            builder.push(
                "durable service",
                DiagnosticStatus::Warning,
                status.detail,
                Some("Run `atx service install` to enable durable jobs.".to_owned()),
            );
            false
        }
        Err(error) => {
            builder.push(
                "durable service",
                DiagnosticStatus::Warning,
                error.message,
                Some("Session jobs remain available.".to_owned()),
            );
            false
        }
    }
}

fn check_private_directory(builder: &mut DoctorReportBuilder, name: &str, path: &Path, uid: u32) {
    if !path.exists() {
        builder.push(
            name,
            DiagnosticStatus::Warning,
            format!("{} does not exist yet", path.display()),
            Some("It will be created on the first saved job.".to_owned()),
        );
        return;
    }
    match validate_private_dir_for_uid(path, uid) {
        Ok(()) => builder.push(
            name,
            DiagnosticStatus::Pass,
            format!("{} is private and owned by this user", path.display()),
            None,
        ),
        Err(error) => builder.push(
            name,
            DiagnosticStatus::Fail,
            error.to_string(),
            Some(format!(
                "Fix ownership and mode 0700 on {}.",
                path.display()
            )),
        ),
    }
}

fn check_database(builder: &mut DoctorReportBuilder, state_directory: &Path) -> Option<u32> {
    let database_path = state_directory.join("atx.db");
    if !database_path.exists() {
        builder.push(
            "SQLite",
            DiagnosticStatus::Warning,
            "state database does not exist yet",
            None,
        );
        return None;
    }
    match Database::open(&database_path, Duration::from_secs(2))
        .and_then(|database| database.schema_version())
    {
        Ok(version) => {
            builder.push(
                "SQLite",
                DiagnosticStatus::Pass,
                format!("database schema {version} is readable and writable"),
                None,
            );
            Some(version)
        }
        Err(error) => {
            builder.push(
                "SQLite",
                DiagnosticStatus::Fail,
                error.to_string(),
                Some("Keep the database for diagnosis; do not delete it.".to_owned()),
            );
            None
        }
    }
}

fn check_supervisor(
    builder: &mut DoctorReportBuilder,
    runtime_dir: &Path,
    inspector: &NativeProcessInspector,
    uid: u32,
) {
    let lock = runtime_dir.join("supervisor.lock");
    let socket = runtime_dir.join("supervisor.sock");
    match (fs::symlink_metadata(&lock), fs::symlink_metadata(&socket)) {
        (Err(lock_error), Err(socket_error))
            if lock_error.kind() == std::io::ErrorKind::NotFound
                && socket_error.kind() == std::io::ErrorKind::NotFound =>
        {
            builder.push(
                "supervisor",
                DiagnosticStatus::Pass,
                "no session supervisor is needed right now",
                None,
            );
        }
        (Ok(lock_metadata), Ok(socket_metadata)) => {
            check_live_supervisor(
                builder,
                &lock,
                &lock_metadata,
                &socket_metadata,
                inspector,
                uid,
            );
        }
        _ => builder.push(
            "supervisor",
            DiagnosticStatus::Warning,
            "runtime lock and socket do not agree",
            Some("Run doctor again after the current supervisor exits.".to_owned()),
        ),
    }
}

fn check_live_supervisor(
    builder: &mut DoctorReportBuilder,
    lock: &Path,
    lock_metadata: &fs::Metadata,
    socket_metadata: &fs::Metadata,
    inspector: &NativeProcessInspector,
    uid: u32,
) {
    let secure = lock_metadata.is_file()
        && !lock_metadata.file_type().is_symlink()
        && lock_metadata.uid() == uid
        && lock_metadata.permissions().mode() & 0o777 == 0o600
        && socket_metadata.file_type().is_socket()
        && socket_metadata.uid() == uid
        && socket_metadata.permissions().mode() & 0o777 == 0o600;
    if !secure {
        builder.push(
            "supervisor",
            DiagnosticStatus::Fail,
            "runtime lock or socket has unsafe ownership, mode, or type",
            Some("Stop ATX processes and remove the unsafe runtime entries.".to_owned()),
        );
        return;
    }
    let status = fs::read(lock)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .and_then(|identity| inspector.classify(&identity).ok());
    match status {
        Some(NativeIdentityStatus::Alive) => builder.push(
            "supervisor",
            DiagnosticStatus::Pass,
            "session supervisor identity and socket agree",
            None,
        ),
        Some(NativeIdentityStatus::Dead | NativeIdentityStatus::Reused) | None => builder.push(
            "supervisor",
            DiagnosticStatus::Warning,
            "supervisor runtime entries are stale or unreadable",
            Some("The next submission will replace stale entries safely.".to_owned()),
        ),
    }
}

fn doctor_exit(report: &DoctorReport) -> ExitCode {
    if report.healthy {
        return ExitCode::SUCCESS;
    }
    if report
        .checks
        .iter()
        .any(|check| check.status == DiagnosticStatus::Fail && check.name == "SQLite")
    {
        exit::storage()
    } else if report
        .checks
        .iter()
        .any(|check| check.status == DiagnosticStatus::Fail && check.name == "supervisor")
    {
        exit::supervision()
    } else {
        exit::permission()
    }
}

fn manage(global: &GlobalArgs, command: &ManagementCommand) -> Result<(), CliError> {
    let (mut store, paths, config) = open_management(global)?;
    match command {
        ManagementCommand::List { state, limit } => {
            let state = state
                .as_deref()
                .map(parse_job_state)
                .transpose()
                .map_err(|error| CliError::usage(global.json, error))?;
            let jobs = list_jobs(&store, state, None, *limit)
                .map_err(|error| management_cli_error(global.json, error))?;
            render_jobs(&jobs, global);
        }
        ManagementCommand::Show { job } => {
            let job = resolve_job(&store, job)
                .map_err(|error| management_cli_error(global.json, error))?;
            render_job(&job, global);
        }
        ManagementCommand::History { job, limit } => {
            let job_id = job
                .as_deref()
                .map(|prefix| resolve_job(&store, prefix).map(|job| job.id()))
                .transpose()
                .map_err(|error| management_cli_error(global.json, error))?;
            let runs = list_runs(&store, job_id, *limit)
                .map_err(|error| management_cli_error(global.json, error))?;
            render_runs(&runs, global);
        }
        ManagementCommand::Output { run } => {
            let output = read_run_output(&store, paths.state_dir(), run)
                .map_err(|error| output_cli_error(global.json, error))?;
            render_run_output(&output, global);
        }
        ManagementCommand::Ps => {
            let runs = store
                .active_runs()
                .map_err(|error| CliError::storage(global.json, error))?;
            render_processes(&runs, global)?;
        }
        ManagementCommand::Cancel { job, grace } => {
            let grace = grace
                .as_deref()
                .map(str::parse::<DurationSeconds>)
                .transpose()
                .map_err(|error| CliError::usage(global.json, error))?
                .unwrap_or(config.cancel_grace());
            let cancelled = cancel_job(&mut store, job, grace)
                .map_err(|error| management_cli_error(global.json, error))?;
            render_job(&cancelled, global);
        }
        ManagementCommand::Remove {
            job,
            cancel,
            keep_history,
        } => {
            let current = resolve_job(&store, job)
                .map_err(|error| management_cli_error(global.json, error))?;
            if !current.state().is_terminal() && *cancel {
                cancel_job(&mut store, job, config.cancel_grace())
                    .map_err(|error| management_cli_error(global.json, error))?;
            }
            let removed = remove_job(&mut store, job, *keep_history)
                .map_err(|error| management_cli_error(global.json, error))?;
            render_job(&removed, global);
        }
        ManagementCommand::Run { job, yes } => {
            let clock = NativeClock;
            let rerun = rerun_job(
                &mut store,
                job,
                *yes,
                clock
                    .now_utc()
                    .map_err(|error| CliError::internal(global.json, error))?,
            )
            .map_err(|error| management_cli_error(global.json, error))?;
            SessionAcknowledger::new(paths.state_dir().to_owned(), paths.runtime_dir().to_owned())
                .acknowledge(rerun.id(), rerun.revision())
                .map_err(|error| CliError::supervision(global.json, error))?;
            render_job(&rerun, global);
        }
        ManagementCommand::Service { .. }
        | ManagementCommand::Version
        | ManagementCommand::Completions { .. }
        | ManagementCommand::Doctor => {}
        #[cfg(feature = "man")]
        ManagementCommand::Man { .. } => {}
        ManagementCommand::Supervisor { .. } | ManagementCommand::Monitor { .. } => {}
    }
    Ok(())
}

fn open_management(global: &GlobalArgs) -> Result<(JobStore, PlatformPaths, Config), CliError> {
    let paths = platform_paths(global.state_dir.as_deref())
        .map_err(|error| CliError::permission(global.json, error))?;
    let config = load_effective_config(&paths, global, false)
        .map_err(|error| CliError::usage(global.json, error))?;
    let database_path = paths.state_dir().join("atx.db");
    if !database_path.is_file() {
        return Err(CliError::not_found(
            global.json,
            "ATX has no state database yet",
        ));
    }
    let database = Database::open(&database_path, Duration::from_secs(2))
        .map_err(|error| CliError::storage(global.json, error))?;
    Ok((JobStore::new(database), paths, config))
}

fn cancel_job(
    store: &mut JobStore,
    prefix: &str,
    grace: DurationSeconds,
) -> Result<Job, ManagementError> {
    let job = resolve_job(store, prefix)?;
    if matches!(job.state(), JobState::Cancelled | JobState::CancelRequested) {
        // Already cancelled or another canceller is mid-flight; either way
        // the request is satisfied.
        return Ok(job);
    }
    if job.state().is_terminal() {
        return Err(ManagementError::StateConflict(
            "completed job cannot be cancelled",
        ));
    }
    let clock = NativeClock;
    let mut now = clock.now_utc().map_err(management_store_error)?;
    let active = store
        .latest_active_run(job.id())
        .map_err(ManagementError::from)?;
    // A concurrent cancel may win the revision race, and the monitor may have
    // advanced a recurring job (raising its update time past our snapshot's);
    // re-reading with a fresh timestamp turns both races into the same
    // idempotent outcome instead of a storage error.
    let mut current = job;
    let mut requested = None;
    for _ in 0..8 {
        if matches!(
            current.state(),
            JobState::CancelRequested | JobState::Cancelled
        ) {
            return Ok(current);
        }
        if current.state().is_terminal() {
            break;
        }
        now = clock.now_utc().map_err(management_store_error)?;
        match store.transition_job(
            current.id(),
            current.revision(),
            JobState::CancelRequested,
            false,
            TransitionActor::Cli,
            "cancel requested",
            now,
        ) {
            Ok(pending) => {
                requested = Some(pending);
                break;
            }
            Err(error @ (StoreError::Conflict | StoreError::Domain(_))) => {
                // Conflict: another writer moved the revision. Domain: the
                // fresh snapshot carries a later update time than our stale
                // `now`. Both resolve by reloading and retrying.
                let _ = error;
                current = store
                    .load(current.id())
                    .map_err(management_store_error)?
                    .ok_or(ManagementError::NotFound)?;
            }
            Err(error) => return Err(management_store_error(error)),
        }
    }
    let Some(requested) = requested else {
        return Err(ManagementError::StateConflict(
            "job changed state during cancellation",
        ));
    };

    let Some(run) = active else {
        return store
            .transition_job(
                requested.id(),
                requested.revision(),
                JobState::Cancelled,
                false,
                TransitionActor::Cli,
                "cancelled before command start",
                now,
            )
            .map_err(management_store_error);
    };
    let terminal_run = cancel_active_run(store, &run, grace, clock)?;
    finish_job_cancellation(store, requested.id(), terminal_run.state(), clock)
}

fn cancel_active_run(
    store: &mut JobStore,
    run: &Run,
    grace: DurationSeconds,
    clock: NativeClock,
) -> Result<Run, ManagementError> {
    let boot_identity = clock.boot_identity().map_err(management_store_error)?;
    let inspector = NativeProcessInspector::new(boot_identity);
    let grace_duration = Duration::from_secs(grace.get());
    let result = cancel_claimed_run(
        store,
        &NativeGroupCanceller::new(&inspector, grace_duration),
        run.id(),
        run.claim_token(),
    )
    .map_err(management_store_error)?;
    if let CancelRunResult::CommittedBeforeSpawn(run) = result {
        store
            .record_run_terminal(
                run.id(),
                run.claim_token(),
                clock.now_utc().map_err(management_store_error)?,
                RunOutcome::Cancelled("cancelled before command start".to_owned()),
            )
            .map_err(management_store_error)?;
    }

    let deadline = std::time::Instant::now()
        .checked_add(grace_duration + Duration::from_secs(2))
        .ok_or(ManagementError::StateConflict("cancel wait overflowed"))?;
    let terminal_run = loop {
        let current = store
            .load_run(run.id())
            .map_err(management_store_error)?
            .ok_or(ManagementError::StateConflict("active run disappeared"))?;
        if current.state().is_terminal() {
            break current;
        }
        if std::time::Instant::now() >= deadline {
            return Err(ManagementError::StateConflict(
                "run did not stop before the cancellation deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(terminal_run)
}

fn finish_job_cancellation(
    store: &mut JobStore,
    job_id: crate::domain::JobId,
    run_state: RunState,
    clock: NativeClock,
) -> Result<Job, ManagementError> {
    let target = match run_state {
        RunState::Succeeded => JobState::Succeeded,
        RunState::Cancelled => JobState::Cancelled,
        RunState::Failed => JobState::Failed,
        RunState::Interrupted => JobState::Interrupted,
        _ => {
            return Err(ManagementError::StateConflict(
                "run remained nonterminal after cancellation",
            ));
        }
    };
    let current = store.load(job_id).map_err(management_store_error)?;
    let current = current.ok_or(ManagementError::NotFound)?;
    if current.state().is_terminal() {
        return Ok(current);
    }
    // A concurrent canceller may finish the transition first, or the run may
    // have completed naturally and the job moved on (a recurring job returns
    // to Waiting); either way the terminal outcome the caller wanted already
    // happened or no longer applies.
    let finished = match store.transition_job(
        current.id(),
        current.revision(),
        target,
        false,
        TransitionActor::Cli,
        "cancellation finished",
        clock.now_utc().map_err(management_store_error)?,
    ) {
        Ok(finished) => Ok(finished),
        Err(StoreError::Domain(_) | StoreError::Conflict) => {
            let final_state = store
                .load(current.id())
                .map_err(management_store_error)?
                .ok_or(ManagementError::NotFound)?;
            if final_state.state().is_terminal() || matches!(final_state.state(), JobState::Waiting)
            {
                return Ok(final_state);
            }
            Err(ManagementError::StateConflict(
                "job changed state during cancellation",
            ))
        }
        Err(error) => Err(management_store_error(error)),
    };
    finished.map_err(management_store_error)
}

fn management_store_error(error: impl std::fmt::Display) -> ManagementError {
    ManagementError::Store(crate::application::ManagementStoreError(error.to_string()))
}

fn parse_job_state(value: &str) -> Result<JobState, &'static str> {
    match value {
        "scheduled" => Ok(JobState::Scheduled),
        "waiting" => Ok(JobState::Waiting),
        "starting" => Ok(JobState::Starting),
        "running" => Ok(JobState::Running),
        "cancel_requested" => Ok(JobState::CancelRequested),
        "succeeded" => Ok(JobState::Succeeded),
        "failed" => Ok(JobState::Failed),
        "cancelled" => Ok(JobState::Cancelled),
        "interrupted" => Ok(JobState::Interrupted),
        "missed" => Ok(JobState::Missed),
        _ => Err("unknown job state"),
    }
}

fn run_schedule(args: &SchedulingArgs) -> ExitCode {
    match schedule(args) {
        Ok((outcome, json, quiet)) => {
            render_submission(&outcome, json, quiet, args.global.color);
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
    let config = load_effective_config(&paths, &args.global, args.options.durable)
        .map_err(|error| CliError::usage(json, error))?;
    if args.options.shell && !quiet {
        eprintln!(
            "Warning: --shell runs the command string through {}; \
             shell metacharacters are interpreted.",
            config.default_shell().display()
        );
    }
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
        let status = native_service_manager(&paths, json)?
            .status()
            .map_err(|error| CliError::capability(json, error))?;
        if !status.installed || !status.running {
            return Err(CliError::capability(
                json,
                "durable service is not installed and running",
            ));
        }
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
    global: &GlobalArgs,
    durable: bool,
) -> Result<Config, String> {
    let config_path = paths.state_dir().join("config.toml");
    let file = match fs::read_to_string(&config_path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", config_path.display())),
    };
    let environment = std::env::vars().collect::<Vec<_>>();
    let overrides = ConfigOverrides {
        default_runtime: durable.then_some(RuntimeTier::Durable),
        color: Some(match global.color {
            ColorArg::Auto => ColorMode::Auto,
            ColorArg::Always => ColorMode::Always,
            ColorArg::Never => ColorMode::Never,
        }),
        verbosity: Some(if global.quiet {
            Verbosity::Quiet
        } else if global.verbose > 0 {
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
    if args.options.tty {
        let tty = resolve_submitting_tty()
            .map_err(|error| format!("--tty requires stdout to be a terminal: {error}"))?;
        execution
            .set_notify_tty(tty)
            .map_err(|error| error.to_string())?;
    }
    Ok(execution)
}

/// Resolve the device path of the submitter's own stdout terminal.
fn resolve_submitting_tty() -> Result<PathBuf, String> {
    // NOTE: /dev/stdout reflects fd 1's actual target even when stdout is
    // redirected; the metadata check is what rejects pipes and files.
    let metadata = fs::metadata("/dev/stdout").map_err(|error| error.to_string())?;
    if !metadata.file_type().is_char_device() {
        return Err("stdout is not a character device".to_owned());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsFd;

        let stdout = std::io::stdout();
        rustix::fs::getpath(stdout.as_fd())
            .map(|path| PathBuf::from(path.to_string_lossy().into_owned()))
            .map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        fs::canonicalize("/proc/self/fd/1").map_err(|error| error.to_string())
    }
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

fn render_jobs(jobs: &[Job], global: &GlobalArgs) {
    let now = UtcTimestamp::from_jiff(jiff::Timestamp::now());
    let views = jobs
        .iter()
        .map(|job| JobView::from_job(job, now))
        .collect::<Vec<_>>();
    if global.json {
        print_json_success(&views);
    } else if !global.quiet {
        println!("{}", HumanRenderer::new(global.color).jobs(&views));
    }
}

fn render_runs(runs: &[crate::domain::Run], global: &GlobalArgs) {
    let views = runs.iter().map(RunView::from_run).collect::<Vec<_>>();
    if global.json {
        print_json_success(&views);
    } else if !global.quiet {
        println!("{}", HumanRenderer::runs(&views));
    }
}

fn render_run_output(output: &RunOutput, global: &GlobalArgs) {
    if global.json {
        print_json_success(&RunOutputView::from_output(output));
    } else if !global.quiet {
        print!("{}", HumanRenderer::run_output(output));
    }
}

fn render_job(job: &Job, global: &GlobalArgs) {
    let view = JobView::from_job(job, UtcTimestamp::from_jiff(jiff::Timestamp::now()));
    if global.json {
        print_json_success(&view);
    } else if !global.quiet {
        println!("{}", HumanRenderer::new(global.color).job(&view));
    }
}

fn render_processes(runs: &[crate::domain::Run], global: &GlobalArgs) -> Result<(), CliError> {
    let clock = NativeClock;
    let inspector = NativeProcessInspector::new(
        clock
            .boot_identity()
            .map_err(|error| CliError::internal(global.json, error))?,
    );
    let mut processes = Vec::new();
    for run in runs {
        for (role, identity) in [
            ("monitor", run.monitor_identity()),
            ("command", run.command_identity()),
        ] {
            if let Some(identity) = identity {
                if inspector
                    .classify(identity)
                    .map_err(|error| CliError::internal(global.json, error))?
                    == NativeIdentityStatus::Alive
                {
                    processes.push(ProcessView {
                        job_id: run.job_id().to_string(),
                        run_id: run.id().to_string(),
                        role,
                        pid: identity.pid,
                        process_group_id: identity.process_group_id,
                        state: run.state(),
                    });
                }
            }
        }
    }
    if global.json {
        print_json_success(&processes);
    } else if !global.quiet {
        println!("{}", HumanRenderer::processes(&processes));
    }
    Ok(())
}

fn render_submission(outcome: &SubmissionOutcome, json: bool, quiet: bool, color: ColorArg) {
    let view = SubmissionView::from_outcome(outcome);
    if json {
        print_json_success(&view);
    } else if !quiet {
        println!("{}", HumanRenderer::new(color).submission(&view));
    }
    if let SubmissionOutcome::CommittedUnsupervised { error, .. } = outcome {
        eprintln!("Job was saved, but {error}");
    }
}

fn print_error(json: bool, code: &str, message: &str) {
    if json {
        let remediation = match code {
            "JOB_NOT_FOUND" => Some("Run `atx list` to inspect job IDs."),
            "STATE_CONFLICT" => Some("Run `atx show JOB` to inspect its current state."),
            _ => None,
        };
        match super::json::error(code, message, remediation) {
            Ok(value) => eprintln!("{value}"),
            Err(error) => eprintln!("atx: could not serialize error output: {error}"),
        }
    } else {
        eprintln!("atx: {message}");
    }
}

fn print_json_success<T: serde::Serialize>(value: &T) {
    match super::json::success(value) {
        Ok(value) => println!("{value}"),
        Err(error) => eprintln!("atx: could not serialize output: {error}"),
    }
}

fn management_cli_error(json: bool, error: ManagementError) -> CliError {
    match error {
        ManagementError::NotFound
        | ManagementError::Ambiguous(_)
        | ManagementError::InvalidPrefix => CliError::not_found(json, error),
        ManagementError::StateConflict(_) | ManagementError::ConfirmationRequired => {
            CliError::conflict(json, error)
        }
        ManagementError::InvalidLimit => CliError::usage(json, error),
        ManagementError::Store(_) => CliError::storage(json, error),
    }
}

fn output_cli_error(json: bool, error: RunOutputError) -> CliError {
    match error {
        RunOutputError::NotFound
        | RunOutputError::Ambiguous
        | RunOutputError::InvalidPrefix
        | RunOutputError::NoRuns
        | RunOutputError::NotCaptured
        | RunOutputError::MissingLogs => CliError::not_found(json, error),
        RunOutputError::Read(_) | RunOutputError::Store(_) => CliError::storage(json, error),
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
        // The supervisor may still be opening its store and reconciling
        // startup under load; poll until a hard deadline rather than a fixed
        // attempt count.
        let deadline = std::time::Instant::now() + SUPERVISOR_READY_TIMEOUT;
        std::thread::sleep(SUPERVISOR_READY_POLL);
        loop {
            match self.socket.acknowledge(job_id, revision) {
                Ok(()) => return Ok(()),
                // A rejected wake is definitive; retrying cannot fix it.
                Err(error) if error.to_string().contains("rejected the wake") => return Err(error),
                Err(error) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(SUPERVISOR_READY_POLL);
                }
            }
        }
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

    fn not_found(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::not_found(), "JOB_NOT_FOUND", json, error)
    }

    fn conflict(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::conflict(), "STATE_CONFLICT", json, error)
    }

    fn storage(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::storage(), "STORAGE_ERROR", json, error)
    }

    fn permission(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::permission(), "PERMISSION_ERROR", json, error)
    }

    fn supervision(json: bool, error: impl std::fmt::Display) -> Self {
        Self::new(exit::supervision(), "SUPERVISION_ERROR", json, error)
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{
        build_doctor_report, build_job, cancel_job, check_database, check_live_supervisor,
        check_private_directory, check_supervisor, doctor_exit, finish_job_cancellation,
        parse_job_state, print_error, read_env_file, render_job, render_jobs, render_runs,
        render_submission, resolve_submitting_tty, run, schedule, split_assignment,
    };
    use crate::application::{
        DiagnosticStatus, DoctorReportBuilder, ElapsedClock, ManagementError, SubmissionOutcome,
        SupervisorAckError, WallClock,
    };
    use crate::cli::args::{ColorArg, GlobalArgs, ParsedCli, parse_from};
    use crate::cli::exit;
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobState, MissedPolicy,
        RunState, RuntimeTier, Schedule, TransitionActor, UtcTimestamp,
    };
    use crate::infrastructure::config::{ConfigOverrides, load_config};
    use crate::infrastructure::process::NativeProcessInspector;
    use crate::infrastructure::sqlite::{Database, JobStore};
    use crate::infrastructure::time::NativeClock;

    fn shell_execution() -> ExecutionSpec {
        ExecutionSpec::new(
            ExecutionMode::Shell,
            vec!["true".to_owned()],
            "/".to_owned(),
            Environment::from_pairs([("PATH", "/usr/bin:/bin")]).expect("environment"),
        )
        .expect("execution")
    }

    fn fixture_job(now: i64) -> Job {
        Job::new(
            UtcTimestamp::from_second(now).expect("now"),
            Schedule::one_shot_relative(
                DurationSeconds::new(30).expect("duration"),
                UtcTimestamp::from_second(now + 30).expect("due"),
            ),
            MissedPolicy::Hold,
            RuntimeTier::Session,
            shell_execution(),
            501,
        )
        .expect("job")
    }

    fn opened_store(path: &Path) -> JobStore {
        let database =
            Database::open(&path.join("atx.db"), Duration::from_millis(100)).expect("database");
        JobStore::new(database)
    }

    fn global(json: bool, quiet: bool) -> GlobalArgs {
        GlobalArgs {
            quiet,
            verbose: 0,
            json,
            color: ColorArg::Never,
            state_dir: None,
        }
    }

    fn cli(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

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

    #[test]
    fn doctor_reports_fresh_and_stale_runtime_fixtures() {
        let root = tempdir().expect("root");
        let state = root.path().join("state");
        let global = doctor_global(&state);
        let fresh = build_doctor_report(&global).expect("fresh report");
        assert!(fresh.healthy);
        assert!(!fresh.durable_available);

        fs::create_dir(&state).expect("state");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("state mode");
        let runtime = state.join("runtime");
        fs::create_dir(&runtime).expect("runtime");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
        fs::write(runtime.join("supervisor.lock"), b"stale").expect("lock");
        fs::set_permissions(
            runtime.join("supervisor.lock"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("lock mode");

        let stale_report = build_doctor_report(&global).expect("stale report");
        assert!(stale_report.healthy);
        assert!(stale_report.checks.iter().any(|check| {
            check.name == "supervisor" && check.status == DiagnosticStatus::Warning
        }));
    }

    #[test]
    fn doctor_fails_wrong_modes_and_runtime_ownership() {
        let root = tempdir().expect("root");
        let state = root.path().join("state");
        fs::create_dir(&state).expect("state");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("state mode");
        let report = build_doctor_report(&doctor_global(&state)).expect("report");
        assert!(!report.healthy);

        let lock = root.path().join("lock");
        fs::write(&lock, b"{}").expect("lock");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).expect("lock mode");
        let socket = root.path().join("socket");
        let _listener = UnixListener::bind(&socket).expect("socket");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("socket mode");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let mut builder = DoctorReportBuilder::default();
        check_live_supervisor(
            &mut builder,
            &lock,
            &fs::symlink_metadata(&lock).expect("lock metadata"),
            &fs::symlink_metadata(&socket).expect("socket metadata"),
            &inspector,
            rustix::process::geteuid().as_raw() + 1,
        );
        let report = builder.finish("test".to_owned(), None, false, serde_json::json!({}));
        assert!(!report.healthy);
    }

    fn doctor_global(state: &std::path::Path) -> crate::cli::args::GlobalArgs {
        let ParsedCli::Management { global, .. } = parse_from([
            OsString::from("atx"),
            OsString::from("--state-dir"),
            state.as_os_str().to_owned(),
            OsString::from("doctor"),
        ])
        .expect("doctor args") else {
            unreachable!("doctor");
        };
        global
    }

    #[test]
    fn parse_failures_and_help_pick_their_exit_codes() {
        assert_eq!(run(cli(&["atx", "--definitely-not-a-flag"])), exit::usage());
        assert_eq!(
            run(cli(&["atx", "--json", "--definitely-not-a-flag"])),
            exit::usage()
        );
        assert_eq!(run(cli(&["atx", "--help"])), ExitCode::SUCCESS);
    }

    #[test]
    fn version_and_completions_render_in_every_mode() {
        assert_eq!(run(cli(&["atx", "version"])), ExitCode::SUCCESS);
        assert_eq!(run(cli(&["atx", "--json", "version"])), ExitCode::SUCCESS);
        for shell in ["bash", "zsh", "fish", "power-shell"] {
            let code = run(cli(&["atx", "completions", "--shell", shell]));
            assert_eq!(code, ExitCode::SUCCESS, "completions {shell}");
        }
    }

    #[cfg(feature = "man")]
    #[test]
    fn man_pages_export_and_report_write_failures() {
        let root = tempdir().expect("root");
        super::export_man_pages(root.path()).expect("export");
        assert!(root.path().join("atx.1").exists());

        let blocker = root.path().join("blocker");
        fs::write(&blocker, b"").expect("file");
        assert!(super::export_man_pages(&blocker).is_err());
    }

    #[test]
    fn doctor_database_check_covers_missing_corrupt_and_valid() {
        // Missing database: warning diagnostic, no schema version.
        let missing = tempdir().expect("missing root");
        let mut builder = DoctorReportBuilder::default();
        assert_eq!(check_database(&mut builder, missing.path()), None);
        let report = builder.finish("tz".to_owned(), None, false, serde_json::json!({}));
        assert!(report.healthy);

        // Corrupt database file: failing diagnostic.
        let corrupt = tempdir().expect("corrupt root");
        fs::write(corrupt.path().join("atx.db"), b"garbage").expect("corrupt file");
        let mut builder = DoctorReportBuilder::default();
        assert_eq!(check_database(&mut builder, corrupt.path()), None);
        let report = builder.finish("tz".to_owned(), None, false, serde_json::json!({}));
        assert!(!report.healthy);

        // Valid database: schema version is reported.
        let valid = tempdir().expect("valid root");
        let database = Database::open(&valid.path().join("atx.db"), Duration::from_millis(100))
            .expect("database");
        drop(database);
        let mut builder = DoctorReportBuilder::default();
        assert!(check_database(&mut builder, valid.path()).is_some());
    }

    #[test]
    fn private_directory_check_reports_missing_private_and_shared() {
        let root = tempdir().expect("root");
        let mut builder = DoctorReportBuilder::default();
        let uid = rustix::process::geteuid().as_raw();

        check_private_directory(&mut builder, "missing", &root.path().join("nope"), uid);
        fs::create_dir(root.path().join("private")).expect("dir");
        fs::set_permissions(
            root.path().join("private"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("mode");
        check_private_directory(&mut builder, "private", &root.path().join("private"), uid);
        fs::create_dir(root.path().join("shared")).expect("dir");
        fs::set_permissions(
            root.path().join("shared"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        check_private_directory(&mut builder, "shared", &root.path().join("shared"), uid);

        let report = builder.finish("tz".to_owned(), None, false, serde_json::json!({}));
        let statuses: Vec<_> = report
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("missing", DiagnosticStatus::Warning),
                ("private", DiagnosticStatus::Pass),
                ("shared", DiagnosticStatus::Fail),
            ]
        );
    }

    #[test]
    fn supervisor_check_flags_disagreeing_runtime_entries() {
        let root = tempdir().expect("root");
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).expect("runtime dir");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("mode");
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let uid = rustix::process::geteuid().as_raw();

        let mut fresh = DoctorReportBuilder::default();
        check_supervisor(&mut fresh, &runtime, &inspector, uid);

        fs::write(runtime.join("supervisor.lock"), b"x").expect("lock only");
        let mut disagreeing = DoctorReportBuilder::default();
        check_supervisor(&mut disagreeing, &runtime, &inspector, uid);
        let report = disagreeing.finish("tz".to_owned(), None, false, serde_json::json!({}));
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.status == DiagnosticStatus::Warning)
        );
        drop(fresh);
    }

    #[test]
    fn live_supervisor_with_unreadable_identity_is_stale() {
        let root = tempdir().expect("root");
        let lock = root.path().join("supervisor.lock");
        fs::write(&lock, b"not json").expect("lock");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).expect("lock mode");
        let socket = root.path().join("supervisor.sock");
        let _listener = UnixListener::bind(&socket).expect("socket");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("socket mode");

        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let mut builder = DoctorReportBuilder::default();
        check_live_supervisor(
            &mut builder,
            &lock,
            &fs::symlink_metadata(&lock).expect("metadata"),
            &fs::symlink_metadata(&socket).expect("metadata"),
            &inspector,
            rustix::process::geteuid().as_raw(),
        );
        let report = builder.finish("tz".to_owned(), None, false, serde_json::json!({}));
        assert!(report.healthy, "stale identity is a warning, not a failure");
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.status == DiagnosticStatus::Warning)
        );
    }

    #[test]
    fn doctor_exit_maps_failure_kinds_to_exit_codes() {
        let healthy = DoctorReportBuilder::default().finish(
            "tz".to_owned(),
            None,
            false,
            serde_json::json!({}),
        );
        assert_eq!(doctor_exit(&healthy), ExitCode::SUCCESS);

        let mut storage = DoctorReportBuilder::default();
        storage.push("SQLite", DiagnosticStatus::Fail, "broken", None);
        assert_eq!(
            doctor_exit(&storage.finish("tz".to_owned(), None, false, serde_json::json!({}))),
            exit::storage()
        );

        let mut supervision = DoctorReportBuilder::default();
        supervision.push("supervisor", DiagnosticStatus::Fail, "unsafe", None);
        assert_eq!(
            doctor_exit(&supervision.finish("tz".to_owned(), None, false, serde_json::json!({}))),
            exit::supervision()
        );

        let mut other = DoctorReportBuilder::default();
        other.push("state directory", DiagnosticStatus::Fail, "shared", None);
        assert_eq!(
            doctor_exit(&other.finish("tz".to_owned(), None, false, serde_json::json!({}))),
            exit::permission()
        );
    }

    #[test]
    fn job_state_parsing_accepts_known_states_only() {
        for (value, expected) in [
            ("scheduled", JobState::Scheduled),
            ("waiting", JobState::Waiting),
            ("starting", JobState::Starting),
            ("running", JobState::Running),
            ("cancel_requested", JobState::CancelRequested),
            ("succeeded", JobState::Succeeded),
            ("failed", JobState::Failed),
            ("cancelled", JobState::Cancelled),
            ("interrupted", JobState::Interrupted),
            ("missed", JobState::Missed),
        ] {
            assert_eq!(parse_job_state(value), Ok(expected));
        }
        assert!(parse_job_state("").is_err());
        assert!(parse_job_state("bogus").is_err());
    }

    #[test]
    fn cancel_paths_are_idempotent_or_conflicting() {
        let root = tempdir().expect("root");
        let mut store = opened_store(root.path());
        let grace = DurationSeconds::new(5).expect("grace");

        // Already cancel-requested: satisfied without touching storage.
        let pending = fixture_job(1_000);
        store.create(&pending).expect("create");
        store
            .transition_job(
                pending.id(),
                pending.revision(),
                JobState::CancelRequested,
                false,
                TransitionActor::Cli,
                "seed",
                UtcTimestamp::from_second(1_001).expect("ts"),
            )
            .expect("cancel requested");
        let id = pending.id().to_string();
        let satisfied = cancel_job(&mut store, &id, grace).expect("satisfied");
        assert_eq!(satisfied.state(), JobState::CancelRequested);

        // Terminal job: cancellation is refused.
        let done = fixture_job(2_000);
        store.create(&done).expect("create");
        store
            .transition_job(
                done.id(),
                done.revision(),
                JobState::Missed,
                false,
                TransitionActor::Cli,
                "seed",
                UtcTimestamp::from_second(2_001).expect("ts"),
            )
            .expect("missed");
        let done_id = done.id().to_string();
        assert!(matches!(
            cancel_job(&mut store, &done_id, grace),
            Err(ManagementError::StateConflict(_))
        ));
    }

    #[test]
    fn finish_cancellation_maps_run_outcomes_to_terminal_jobs() {
        for (run_state, expected) in [
            (RunState::Succeeded, JobState::Succeeded),
            (RunState::Cancelled, JobState::Cancelled),
            (RunState::Failed, JobState::Failed),
            (RunState::Interrupted, JobState::Interrupted),
        ] {
            let root = tempdir().expect("root");
            let mut store = opened_store(root.path());
            let job = fixture_job(3_000);
            store.create(&job).expect("create");
            let requested = store
                .transition_job(
                    job.id(),
                    job.revision(),
                    JobState::CancelRequested,
                    false,
                    TransitionActor::Cli,
                    "seed",
                    UtcTimestamp::from_second(3_001).expect("ts"),
                )
                .expect("cancel requested");
            let finished = finish_job_cancellation(&mut store, job.id(), run_state, NativeClock)
                .expect("finish");
            assert_eq!(finished.state(), expected);
            // Already-terminal jobs return as-is instead of re-transitioning.
            let again = finish_job_cancellation(&mut store, job.id(), run_state, NativeClock)
                .expect("repeat");
            assert_eq!(again.revision(), finished.revision());
            drop(requested);
        }
    }

    #[test]
    fn finish_cancellation_refuses_nonterminal_runs() {
        let root = tempdir().expect("root");
        let mut store = opened_store(root.path());
        let job = fixture_job(4_000);
        store.create(&job).expect("create");
        store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::CancelRequested,
                false,
                TransitionActor::Cli,
                "seed",
                UtcTimestamp::from_second(4_001).expect("ts"),
            )
            .expect("cancel requested");
        assert!(matches!(
            finish_job_cancellation(&mut store, job.id(), RunState::Starting, NativeClock),
            Err(ManagementError::StateConflict(_))
        ));
    }

    #[test]
    fn submission_rendering_reports_unsupervised_commits() {
        let outcome = SubmissionOutcome::CommittedUnsupervised {
            job: fixture_job(5_000),
            error: SupervisorAckError("no supervisor socket".to_owned()),
        };
        render_submission(&outcome, true, false, ColorArg::Never);
        render_submission(&outcome, false, true, ColorArg::Never);
        render_submission(&outcome, false, false, ColorArg::Never);
        let dry = SubmissionOutcome::DryRun(fixture_job(5_100));
        render_submission(&dry, true, false, ColorArg::Never);
    }

    #[test]
    fn error_output_selects_remediations_per_code() {
        print_error(true, "JOB_NOT_FOUND", "gone");
        print_error(true, "STATE_CONFLICT", "racing");
        print_error(true, "STORAGE_ERROR", "disk");
        print_error(false, "JOB_NOT_FOUND", "gone");
    }

    #[test]
    fn manage_commands_render_without_a_supervisor() {
        let root = tempdir().expect("root");
        let state = root.path().join("state");
        let state_flag = state.clone().into_os_string();

        // No database yet: not-found diagnostics, never a crash.
        assert_eq!(
            run([
                OsString::from("atx"),
                OsString::from("--state-dir"),
                state_flag.clone(),
                OsString::from("list")
            ]),
            exit::not_found()
        );

        let _store = {
            fs::create_dir(&state).expect("state dir");
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode");
            opened_store(&state)
        };
        for command in [vec!["--json", "history"], vec!["--quiet", "ps"]] {
            let mut argv = vec![
                OsString::from("atx"),
                OsString::from("--state-dir"),
                state_flag.clone(),
            ];
            argv.extend(command.iter().map(OsString::from));
            assert_eq!(run(argv), ExitCode::SUCCESS, "command {command:?}");
        }
        assert_eq!(
            run(cli(&[
                "atx",
                "--state-dir",
                state.to_str().expect("utf8"),
                "show",
                "zzzzzzzzzzzzzzzzzzzzzzzzzz",
            ])),
            exit::not_found()
        );

        for extra in [
            vec![OsString::from("--json")],
            vec![OsString::from("--quiet")],
        ] {
            let mut argv = vec![
                OsString::from("atx"),
                OsString::from("--state-dir"),
                state_flag.clone(),
            ];
            argv.extend(extra);
            argv.push(OsString::from("list"));
            assert_eq!(run(argv), ExitCode::SUCCESS);
        }

        assert_eq!(
            run([
                OsString::from("atx"),
                OsString::from("--state-dir"),
                state_flag,
                OsString::from("list"),
                OsString::from("--state"),
                OsString::from("bogus"),
            ]),
            exit::usage()
        );
    }

    #[test]
    fn monitor_and_supervisor_commands_surface_errors() {
        let root = tempdir().expect("root");
        assert_eq!(
            run([
                OsString::from("atx"),
                OsString::from("__monitor"),
                OsString::from("--state-dir"),
                root.path().join("s").into_os_string(),
                OsString::from("--runtime-dir"),
                root.path().join("r").into_os_string(),
                OsString::from("--job"),
                OsString::from("not-a-job-id"),
                OsString::from("--run"),
                OsString::from("not-a-run-id"),
            ]),
            exit::supervision()
        );

        // A regular file cannot serve as the supervisor's state directory.
        let blocker = root.path().join("blocker");
        fs::write(&blocker, b"").expect("file");
        assert_eq!(
            run([
                OsString::from("atx"),
                OsString::from("__supervisor"),
                OsString::from("--state-dir"),
                blocker.clone().into_os_string(),
                OsString::from("--runtime-dir"),
                root.path().join("runtime").into_os_string(),
            ]),
            exit::supervision()
        );
        drop(blocker);
    }

    #[test]
    fn schedule_calendar_option_conflicts_are_rejected() {
        let config = load_config(None, &[], ConfigOverrides::default()).expect("config");
        let clock = NativeClock;
        let wall = clock.now_utc().expect("wall");
        let elapsed = clock.now_elapsed().expect("elapsed");
        for args in [
            vec!["atx", "--every", "5m", "--utc", "--", "true"],
            vec!["atx", "--every", "5m", "--dst", "earlier", "--", "true"],
            vec!["atx", "--every", "5m", "--no-rollover", "--", "true"],
            vec!["atx", "--utc", "30s", "--", "true"],
            vec!["atx", "--tz", "UTC", "30s", "--", "true"],
            vec!["atx", "30s", "--dst", "later", "--", "true"],
            vec!["atx", "--no-rollover", "30s", "--", "true"],
            vec!["atx", "--every", "5m", "--tz", "UTC", "--", "true"],
        ] {
            let ParsedCli::Schedule(args) =
                parse_from(args.into_iter().map(OsString::from)).expect("parse")
            else {
                unreachable!("schedule");
            };
            assert!(
                build_job(&args, &config, wall, elapsed).is_err(),
                "conflict accepted"
            );
        }

        // A non-local default timezone resolves calendar input as named UTC.
        let named = load_config(
            None,
            &[],
            ConfigOverrides {
                default_timezone: Some("UTC".to_owned()),
                ..ConfigOverrides::default()
            },
        )
        .expect("config");
        let ParsedCli::Schedule(args) = parse_from(
            ["atx", "23:59", "--", "true"]
                .into_iter()
                .map(OsString::from),
        )
        .expect("parse") else {
            unreachable!("schedule");
        };
        assert!(build_job(&args, &named, wall, elapsed).is_ok());
    }

    #[test]
    fn tty_echo_requires_a_terminal_stdout() {
        // Test harnesses capture stdout through a pipe, so the submitting-tty
        // probe must refuse rather than resolve a device path.
        assert!(resolve_submitting_tty().is_err());
    }

    #[test]
    fn env_assignments_require_a_key_before_the_equals_sign() {
        assert_eq!(split_assignment("KEY=value"), Ok(("KEY", "value")));
        assert_eq!(
            split_assignment("KEY=with=equals"),
            Ok(("KEY", "with=equals"))
        );
        assert!(split_assignment("NO_EQUALS").is_err());
        assert!(split_assignment("=NO_KEY").is_err());
    }

    #[test]
    fn env_files_reject_directories_nul_bytes_and_oversize() {
        let root = tempdir().expect("root");

        assert!(read_env_file(root.path()).is_err());

        let nul = root.path().join("nul.env");
        fs::write(&nul, b"A=1\0B=2").expect("write");
        assert!(read_env_file(&nul).is_err());

        let oversize = root.path().join("big.env");
        fs::write(&oversize, vec![b'a'; 1024 * 1024 + 1]).expect("write");
        assert!(read_env_file(&oversize).is_err());
    }

    #[test]
    fn job_renderers_honor_json_and_quiet_modes() {
        let root = tempdir().expect("root");
        let mut store = opened_store(root.path());
        let job = fixture_job(6_000);
        store.create(&job).expect("create");
        let run = store
            .claim_run(
                job.id(),
                UtcTimestamp::from_second(6_030).expect("scheduled"),
                UtcTimestamp::from_second(6_001).expect("created"),
            )
            .expect("claim");
        let runs = vec![run];

        for mode in [
            global(true, false),
            global(false, true),
            global(false, false),
        ] {
            render_jobs(std::slice::from_ref(&job), &mode);
            render_runs(&runs, &mode);
            render_job(&job, &mode);
        }
    }
}
