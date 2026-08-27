# Command guide

`atx --help` is the quick reference. This page covers the details that are easy
to miss when copying a one-line example.

## Synopsis

```text
atx [GLOBAL OPTIONS] [SCHEDULING OPTIONS] WHEN -- PROGRAM [ARG ...]
atx [GLOBAL OPTIONS] [SCHEDULING OPTIONS] --every DURATION -- PROGRAM [ARG ...]
atx [GLOBAL OPTIONS] COMMAND [COMMAND OPTIONS]
```

The literal `--` separates ATX options from the program. It is required for a
scheduled command. In direct mode, ATX passes every following argument as-is;
it does not join or reinterpret them.

```console
atx 30s -- notify-send "tea is ready"
atx 2m30s -- make check
atx --utc 23:00 -- ./backup-home
atx --tz America/Sao_Paulo "2099-01-01 09:00" -- ./report
```

Use `--shell` only for a command string that needs shell syntax:

```console
atx --shell 10m -- 'printf "done\n" >>"$HOME/notes"'
```

The string runs through the configured shell (`/bin/sh` by default). Quoting,
expansion, redirection, and pipelines are the shell's behavior, not ATX's; do
not assemble this string from untrusted input.

## Global options

- `--state-dir PATH` uses an absolute directory for state and runtime files.
  Its runtime directory is `PATH/runtime`, which is useful for tests and
  isolated installations.
- `--json` writes the documented JSON envelope to stdout instead of human
  output.
- `-q`, `--quiet` suppresses successful human output. It conflicts with
  `--verbose`; diagnostics still go to stderr.
- `-v`, `--verbose` requests extra diagnostics.
- `--color MODE` chooses color behavior. `MODE` is `auto`, `always`, or `never`;
  without the flag, the configured color mode applies.
- `-h`, `--help` prints help and exits successfully. `atx help` is also
  available.
- `-V`, `--version` prints the program version and exits successfully.
  `atx version` also has JSON output.

`--quiet` does not hide errors or the warnings for `--shell` and `--capture-env`.
Those warnings matter because the command string or saved environment can be
sensitive.

## Scheduling

Supply exactly one of `WHEN` and `--every DURATION`, followed by `-- PROGRAM
[ARG ...]`.

### Time input

`WHEN` is a relative duration, a time of day, a date, or a date-time. Relative
durations use ordered units such as `30s`, `2m30s`, or `5h`. A time of day that
already passed today rolls over to tomorrow unless `--no-rollover` is given.
A bare date such as `2026-09-01` means `2026-09-01 00:00:00` in the selected
timezone.

Calendar input uses the local IANA timezone by default. `--utc` selects UTC and
`--tz IANA_ZONE` selects a bundled IANA timezone; they conflict. A local time
that falls in a daylight-saving gap is rejected. A repeated local time is also
rejected unless `--dst earlier` or `--dst later` chooses an offset.

- `--dry-run` validates and prints the job without saving it.
- `--utc` interprets calendar input as UTC and conflicts with `--tz`.
- `--tz IANA_ZONE` interprets calendar input in this IANA timezone and
  conflicts with `--utc`.
- `--dst MODE` resolves a repeated local time. `MODE` is `reject`, `earlier`, or
  `later`; the default is `reject`.
- `--no-rollover` rejects an already-passed time of day instead of choosing
  tomorrow.
- `--every DURATION` creates a fixed-rate recurring job. Use it instead of
  `WHEN`, not with it.
- `--missed POLICY` selects recovery behavior for a deadline missed while no
  supervisor was running. `POLICY` is `hold`, `run-latest`, or `skip`.
- `--durable` requires installed launchd or systemd integration. It never falls
  back to session mode.
- `--cwd DIRECTORY` runs from an existing absolute working directory.
- `--name NAME` sets a short display name; `--description TEXT` stores a longer
  note.
- `--shell` runs one command string through the configured shell.
- `--tty` tries to append captured output to the submitting terminal when the
  run finishes.

`--tty` is best effort. If the terminal has closed, the run's outcome does not
change and `atx output` still has the captured output.

### Job environment

ATX stores an execution environment with the job. By default it copies only
`HOME`, `USER`, `LOGNAME`, `PATH`, `LANG`, `TMPDIR`, and variables whose names
start with `LC_`. `TZ` is **not** copied by default, so a program's own time
formatting may use a different timezone from the one used to schedule it.

- `--env KEY=VALUE` adds or replaces an environment value; it may be repeated.
- `--env-file PATH` reads `KEY=VALUE` lines from a file and conflicts with
  `--capture-env`.
- `--capture-env` copies the complete submitting environment.

Values from `--env-file` and `--env` override earlier values. `--capture-env`
can save credentials in the state database and always emits a warning. Stored
environment values are used for execution but are redacted from `show`, JSON
output, and `doctor`.

## Managing jobs

`JOB` and `RUN` accept a full ID or a unique prefix. A non-unique prefix is an
error. Commands that produce a list default to 100 records.

- `atx list [--state STATE] [--limit N]` lists saved jobs (`ls` is an alias).
  States are `scheduled`, `waiting`, `starting`, `running`, `cancel-requested`,
  `succeeded`, `failed`, `cancelled`, `interrupted`, and `missed`.
- `atx show JOB` shows one job, its schedule, execution details, and last
  outcome.
- `atx cancel JOB [--grace DURATION]` requests cancellation; the configured
  grace period applies when omitted.
- `atx rm JOB [--cancel] [--keep-history]` removes a job. It refuses a live job
  unless `--cancel` is supplied. History is removed unless `--keep-history` is
  supplied.
- `atx run JOB [--yes]` runs a terminal job again. `--yes` confirms rerunning
  an interrupted job, whose previous effect is unknown.
- `atx ps` lists ATX monitor and command processes that are live now.
- `atx history [JOB] [--limit N]` lists completed runs, newest first.
- `atx output RUN` prints captured stdout and stderr. A job ID selects its
  latest run.
- `atx doctor` checks local state, permissions, SQLite, clocks, process
  identity, configuration, timezone data, and durable-service support.
- `atx service install`, `atx service status`, and `atx service uninstall`
  install, inspect, and remove the durable per-user service.
- `atx version` prints version information.
- `atx completions --shell SHELL` prints a completion script for `bash`, `zsh`,
  `fish`, or `power-shell`.

`service install` points the service at the current binary and state directory.
Run it again after moving either. On Linux the unit is `atx.service`; on macOS
the launchd label is `io.github.jsugg.atx`.

## Human and JSON output

Human output uses local time with its numeric offset, for example
`2026-08-26 09:51:56 -03:00`, rather than raw UTC microsecond timestamps. JSON
keeps RFC 3339 UTC timestamps and full identifiers for scripts. Human output is
for reading; use `--json` when parsing fields. Empty human views say `No jobs.`,
`No runs.`, or `No live ATX processes.` instead of printing an empty table.

Captured stdout and stderr are kept separately. The per-stream cap defaults to
10 MiB. When a command exceeds it, ATX keeps the beginning of the stream and
discards its tail; human `output` labels the stream as truncated and JSON sets
`stdout_truncated` or `stderr_truncated`. The files are under
`runs/<run-id>/stdout.log` and `runs/<run-id>/stderr.log` inside the state
directory.

Successful output goes to stdout. Errors, warnings, and diagnostics go to
stderr. JSON successes use `schema_version`, `ok: true`, and `data`; errors
use the same envelope with `ok: false`. Version 1 fields are documented in the
[JSON API](json-api.md); later releases may add fields without changing their
meanings.

## Configuration

ATX reads optional TOML from `<state-dir>/config.toml`. The default location is
`~/Library/Application Support/atx/config.toml` on macOS and
`$XDG_STATE_HOME/atx/config.toml` (or `~/.local/state/atx/config.toml`) on
Linux. Configuration layers are file, then `ATX_*` environment values, then
explicit command-line flags.

```toml
default_timezone = "local"              # local or an IANA timezone
default_runtime = "session"             # session | durable
default_shell = "/bin/sh"               # absolute path used by --shell
cancel_grace = "10s"
history_days = 30
terminal_job_days = 30
max_log_bytes_per_stream = 10485760      # 10 MiB; valid range 1..=1073741824
color = "auto"                          # auto | always | never
verbosity = "normal"                    # quiet | normal | verbose
```

The only recognized environment names are `ATX_DEFAULT_TIMEZONE`,
`ATX_DEFAULT_RUNTIME`, `ATX_DEFAULT_SHELL`, `ATX_CANCEL_GRACE`,
`ATX_HISTORY_DAYS`, `ATX_TERMINAL_JOB_DAYS`, `ATX_MAX_LOG_BYTES_PER_STREAM`,
`ATX_COLOR`, and `ATX_VERBOSITY`. Unknown keys and invalid values report an
error instead of being ignored.

## Exit statuses

- `0`: successful command handling. Scheduling success means the job was saved
  and, when supervised, acknowledged; it does not mean the program ran.
- `2`: invalid command line.
- `3`: missing or ambiguous job or run ID.
- `4`: job state conflict.
- `5`: requested platform feature is unavailable.
- `10`: state database failure.
- `11`: process or supervisor failure. A job can be committed to state but
  return 11 if ATX cannot reach its supervisor; inspect it with `atx show JOB`.
- `12`: ownership or permission failure.
- `70`: unexpected internal failure.

## Completions and man pages

```console
# bash
atx completions --shell bash > ~/.local/share/bash-completion/completions/atx

# zsh
atx completions --shell zsh > "${fpath[1]}/_atx"

# fish
atx completions --shell fish > ~/.config/fish/completions/atx.fish

# PowerShell
atx completions --shell power-shell >> $PROFILE
```

When building from source, `cargo xtask dist-man` writes `dist/man/*.1`. Set
`SOURCE_DATE_EPOCH` if you need reproducible page dates. Released archives
contain the same files in `man-pages/`. Copy them to a `man1` directory such as
`/usr/local/share/man/man1/`, then run `mandb` if your system needs it.
