//! Typed command-line arguments.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "atx",
    version,
    about = "Run commands later without keeping a terminal open",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct RawCli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(flatten)]
    schedule: ScheduleOptions,

    /// Time, date, datetime, or relative duration.
    #[arg(value_name = "WHEN", num_args = 0..)]
    when: Vec<String>,

    /// Command and its arguments.
    #[arg(last = true, value_name = "ARGV", num_args = 1..)]
    argv: Vec<OsString>,

    #[command(subcommand)]
    management: Option<ManagementCommand>,
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct GlobalArgs {
    /// Suppress successful human output.
    #[arg(short, long, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Print extra diagnostics. Repeat for more detail.
    #[arg(short, long, action = ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Print machine-readable JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Control colored output.
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    pub(crate) color: ColorArg,

    /// Use a different state directory.
    #[arg(long, value_name = "PATH")]
    pub(crate) state_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct ScheduleOptions {
    /// Validate and print the job without saving it.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Interpret calendar input as UTC.
    #[arg(long, conflicts_with = "tz")]
    pub(crate) utc: bool,

    /// Interpret calendar input in this IANA timezone.
    #[arg(long, value_name = "IANA_ZONE")]
    pub(crate) tz: Option<String>,

    /// Resolve a repeated local time.
    #[arg(long, value_enum)]
    pub(crate) dst: Option<DstArg>,

    /// Reject a time of day that already passed today.
    #[arg(long)]
    pub(crate) no_rollover: bool,

    /// Run from this absolute directory.
    #[arg(long, value_name = "DIRECTORY")]
    pub(crate) cwd: Option<PathBuf>,

    /// Run one command string through /bin/sh.
    #[arg(long)]
    pub(crate) shell: bool,

    /// Set a display name.
    #[arg(long)]
    pub(crate) name: Option<String>,

    /// Add a longer note.
    #[arg(long)]
    pub(crate) description: Option<String>,

    /// Set or replace an environment value.
    #[arg(long, value_name = "KEY=VALUE", action = ArgAction::Append)]
    pub(crate) env: Vec<String>,

    /// Read environment values from a file.
    #[arg(long, value_name = "PATH", conflicts_with = "capture_env")]
    pub(crate) env_file: Option<PathBuf>,

    /// Save the complete submitting environment.
    #[arg(long)]
    pub(crate) capture_env: bool,

    /// Choose what recovery does after a missed deadline.
    #[arg(long, value_enum)]
    pub(crate) missed: Option<MissedArg>,

    /// Ask the operating-system service to own this job.
    #[arg(long)]
    pub(crate) durable: bool,

    /// Repeat on a fixed-rate interval.
    #[arg(long, value_name = "DURATION")]
    pub(crate) every: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum ColorArg {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum DstArg {
    #[default]
    Reject,
    Earlier,
    Later,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MissedArg {
    Hold,
    RunLatest,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SchedulingArgs {
    pub(crate) global: GlobalArgs,
    pub(crate) options: ScheduleOptions,
    pub(crate) when: Vec<String>,
    pub(crate) argv: Vec<OsString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParsedCli {
    Schedule(SchedulingArgs),
    Management {
        global: GlobalArgs,
        command: ManagementCommand,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum ManagementCommand {
    /// List saved jobs.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Show one job.
    Show { job: String },
    /// Cancel one job.
    Cancel {
        job: String,
        #[arg(long)]
        grace: Option<String>,
    },
    /// Remove one job.
    #[command(name = "rm")]
    Remove {
        job: String,
        #[arg(long)]
        cancel: bool,
        #[arg(long)]
        keep_history: bool,
    },
    /// Run a saved job again.
    Run {
        job: String,
        #[arg(long)]
        yes: bool,
    },
    /// List live ATX processes.
    Ps,
    /// List completed runs.
    History {
        job: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Check local configuration and state.
    Doctor,
    /// Manage durable service integration.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print version information.
    Version,
    #[command(name = "__supervisor", hide = true)]
    Supervisor {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        runtime_dir: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum ServiceAction {
    Install,
    Status,
    Uninstall,
}

pub(crate) fn parse_from(
    args: impl IntoIterator<Item = OsString>,
) -> Result<ParsedCli, clap::Error> {
    let raw = RawCli::try_parse_from(args)?;
    if let Some(command) = raw.management {
        return Ok(ParsedCli::Management {
            global: raw.global,
            command,
        });
    }
    if raw.argv.is_empty() {
        return Err(RawCli::command().error(
            clap::error::ErrorKind::MissingRequiredArgument,
            "scheduling requires `-- PROGRAM [ARG ...]`",
        ));
    }
    if raw.when.is_empty() == raw.schedule.every.is_none() {
        return Err(RawCli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "provide either WHEN or `--every DURATION`, but not both",
        ));
    }
    Ok(ParsedCli::Schedule(SchedulingArgs {
        global: raw.global,
        options: raw.schedule,
        when: raw.when,
        argv: raw.argv,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{ManagementCommand, ParsedCli, parse_from};

    #[test]
    fn separator_preserves_command_arguments() {
        let parsed = parse_from(
            [
                "atx", "30s", "--env", "A=1", "--", "printf", "--flag", "hello",
            ]
            .map(Into::into),
        )
        .expect("schedule");
        let ParsedCli::Schedule(schedule) = parsed else {
            unreachable!("schedule expected");
        };
        assert_eq!(schedule.when, ["30s"]);
        assert_eq!(schedule.options.env, ["A=1"]);
        assert_eq!(
            schedule.argv,
            ["printf", "--flag", "hello"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn schedule_requires_exact_separator_and_one_time_grammar() {
        for args in [
            vec!["atx", "30s", "echo"],
            vec!["atx", "30s", "--every", "1m", "--", "echo"],
            vec!["atx", "--", "echo"],
            vec!["atx", "30s", "--"],
            vec!["atx", "30s", "--wat", "--", "echo"],
        ] {
            assert!(parse_from(args.into_iter().map(Into::into)).is_err());
        }
    }

    #[test]
    fn options_enforce_conflicts_and_shell_shape_is_left_for_validation() {
        for args in [
            vec!["atx", "--utc", "--tz", "UTC", "12:00", "--", "echo"],
            vec![
                "atx",
                "--env-file",
                "values.env",
                "--capture-env",
                "30s",
                "--",
                "echo",
            ],
            vec!["atx", "-q", "-v", "30s", "--", "echo"],
        ] {
            assert!(parse_from(args.into_iter().map(Into::into)).is_err());
        }
    }

    #[test]
    fn reserved_management_commands_do_not_parse_as_schedules() {
        let parsed =
            parse_from(["atx", "ls", "--limit", "7"].map(Into::into)).expect("management command");
        assert!(matches!(
            parsed,
            ParsedCli::Management {
                command: ManagementCommand::List { limit: 7, .. },
                ..
            }
        ));
    }
}
