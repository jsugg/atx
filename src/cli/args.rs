//! Typed command-line arguments.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "atx",
    version,
    about = "Run commands later without keeping a terminal open",
    long_about = None,
    after_long_help = CLI_MANUAL,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    subcommand_precedence_over_arg = true
)]
pub(crate) struct RawCli {
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
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Print extra diagnostics. Repeat for more detail.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Control colored output.
    #[arg(long, global = true, value_enum, default_value_t = ColorArg::Auto)]
    pub(crate) color: ColorArg,

    /// Use a different state directory.
    #[arg(long, global = true, value_name = "PATH")]
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
    /// Emit shell completion scripts.
    Completions {
        /// Target shell for the generated script.
        #[arg(long, value_enum)]
        shell: CompletionShell,
    },
    /// Write roff man pages into a directory. (internal)
    #[cfg(feature = "man")]
    #[command(name = "__man", hide = true)]
    Man {
        /// Destination directory for the generated pages.
        out_dir: PathBuf,
    },
    #[command(name = "__supervisor", hide = true)]
    Supervisor {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        service_managed: bool,
    },
    #[command(name = "__monitor", hide = true)]
    Monitor {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        job: String,
        #[arg(long)]
        run: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum ServiceAction {
    Install,
    Status,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

pub(crate) fn parse_from(
    args: impl IntoIterator<Item = OsString>,
) -> Result<ParsedCli, clap::Error> {
    let mut args = args.into_iter().collect::<Vec<_>>();
    normalize_management_order(&mut args);
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

const CLI_MANUAL: &str = "\
EXAMPLES:
  atx 30s -- notify-send \"tea is ready\"
      Run a command after 30 seconds.

  atx --utc 23:00 -- ./backup-home
      Run tonight at 23:00 UTC. Calendar times roll over to tomorrow
      when they already passed; --no-rollover turns that into an error.

  atx --tz America/Sao_Paulo \"2099-01-01 09:00\" -- ./report
      Schedule in an explicit IANA timezone.

  atx --every 5m -- /usr/bin/cleanup
      Repeat on a fixed-rate interval.

  atx --shell 10m -- 'printf \"done\\n\" >>\"$HOME/notes\"'
      Opt in to /bin/sh only when pipes or redirects are needed.

EXIT STATUS:
  0    success
  1    operation finished with a negative job outcome
  2    invalid command line
  3    missing or ambiguous job ID
  4    job state conflict
  5    requested platform feature unavailable
  10   state database failure
  11   process or supervisor failure
  12   ownership or permission failure
  70   unexpected internal failure

FILES:
  macOS:        ~/Library/Application Support/atx/atx.db (state)
                $TMPDIR/atx-<uid> (runtime)
  Linux:        $XDG_STATE_HOME/atx/atx.db, else ~/.local/state/atx/atx.db
                $XDG_RUNTIME_DIR/atx, else $TMPDIR/atx-<uid>
  launchd:      ~/Library/LaunchAgents/io.github.jsugg.atx.plist
  systemd:      ~/.config/systemd/user/atx.service
  Environment values are stored for execution but never displayed.

SEE ALSO:
  Full guide and JSON schema: https://github.com/jsugg/atx
";

fn normalize_management_order(args: &mut Vec<OsString>) {
    if args.len() < 3 || args.get(1).is_some_and(|value| is_management_name(value)) {
        return;
    }
    let mut skip_value = false;
    for index in 1..args.len() {
        if skip_value {
            skip_value = false;
            continue;
        }
        let value = args[index].to_string_lossy();
        if value == "--" {
            return;
        }
        if option_takes_value(&value) {
            skip_value = !value.contains('=');
            continue;
        }
        if is_management_name(&args[index]) {
            let command = args.remove(index);
            args.insert(1, command);
            return;
        }
    }
}

fn is_management_name(value: &std::ffi::OsStr) -> bool {
    matches!(
        value.to_str(),
        Some(
            "list"
                | "ls"
                | "show"
                | "cancel"
                | "rm"
                | "run"
                | "ps"
                | "history"
                | "doctor"
                | "service"
                | "version"
                | "completions"
                | "help"
                | "__supervisor"
                | "__monitor"
                | "__man"
        )
    )
}

fn option_takes_value(value: &str) -> bool {
    let name = value.split_once('=').map_or(value, |(name, _)| name);
    matches!(
        name,
        "--color"
            | "--state-dir"
            | "--tz"
            | "--dst"
            | "--cwd"
            | "--name"
            | "--description"
            | "--env"
            | "--env-file"
            | "--missed"
            | "--every"
    )
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
            parse_from(["atx", "--state-dir", "/tmp/atx", "ls", "--limit", "7"].map(Into::into))
                .expect("management command");
        assert!(matches!(
            parsed,
            ParsedCli::Management {
                command: ManagementCommand::List { limit: 7, .. },
                ..
            }
        ));
    }
}

#[cfg(test)]
mod manual_tests {
    use super::{CLI_MANUAL, RawCli};
    use clap::CommandFactory;

    #[test]
    fn long_help_includes_manual_sections() {
        let mut cmd = RawCli::command();
        let rendered = cmd.render_long_help().to_string();
        assert!(rendered.contains("EXAMPLES:"), "missing EXAMPLES in:\n{rendered}");
        assert!(rendered.contains("EXIT STATUS:"));
        assert!(rendered.contains("FILES:"));
        assert_eq!(cmd.get_after_long_help().map(|s| s.to_string()), Some(CLI_MANUAL.to_owned()));
    }
}

#[cfg(test)]
mod completions_tests {
    #![allow(clippy::expect_used)]
    use super::{CompletionShell, RawCli};
    use clap::CommandFactory;
    use clap_complete::{Shell, generate};

    fn render(shell: Shell) -> String {
        let mut cmd = RawCli::command();
        let mut buf = Vec::new();
        generate(shell, &mut cmd, "atx", &mut buf);
        String::from_utf8(buf).expect("completion script is UTF-8")
    }

    #[test]
    fn completion_scripts_are_stable_per_shell() {
        for (shell, marker) in [
            (CompletionShell::Bash, "_atx() {"),
            (CompletionShell::Zsh, "#compdef atx"),
            (CompletionShell::Fish, "__fish_atx_global_optspecs"),
            (CompletionShell::PowerShell, "Register-ArgumentCompleter"),
        ] {
            let script = render(match shell {
                CompletionShell::Bash => Shell::Bash,
                CompletionShell::Zsh => Shell::Zsh,
                CompletionShell::Fish => Shell::Fish,
                CompletionShell::PowerShell => Shell::PowerShell,
            });
            assert!(script.contains(marker), "{shell:?} missing `{marker}`");
            // Fish emits flags as `-l state-dir`, so match the bare flag name.
            assert!(script.contains("state-dir"), "{shell:?} missing state-dir");
            assert!(script.contains("completions") || script.contains("Completions"));
        }
    }
}
