# Command guide

## Schedule something

Put the time first, then a literal `--`, then the command:

```console
atx 30s -- notify-send "tea is ready"
atx 2m30s -- make check
atx --utc 23:00 -- ./backup-home
atx --tz America/Sao_Paulo "2099-01-01 09:00" -- ./report
```

ATX passes everything after `--` straight to the program. It does not join or
reinterpret arguments. Use `--shell` only when you need pipes, redirects, or
other shell syntax:

```console
atx --shell 10m -- 'printf "done\n" >>"$HOME/notes"'
```

Times without a date roll over to tomorrow when they already passed today.
`--no-rollover` turns that into an error. Calendar input uses the local
timezone unless `--utc` or `--tz IANA_ZONE` says otherwise. A repeated clock
time during a daylight-saving change is rejected unless `--dst earlier` or
`--dst later` resolves it.

Useful scheduling options:

```text
--dry-run                 validate without writing anything
--cwd DIRECTORY           choose an existing absolute working directory
--name NAME               give the job a short label
--description TEXT        attach a note
--env KEY=VALUE           add or replace one environment value
--env-file PATH           read KEY=VALUE lines
--capture-env              keep the complete current environment
--missed hold|run-latest|skip
--durable                 require installed service integration
--every DURATION          fixed-rate recurring job
--tty                     echo captured output to the submitting terminal
                          when the run finishes
```

`--tty` records the terminal device of your stdout at submit time. When the
run finishes, the monitor appends the captured stdout and stderr there as a
best-effort fire-and-forget write: if the terminal is closed or gone by then,
the run outcome is unaffected and the output stays available via `atx
output JOB`.

By default ATX keeps a small safe environment rather than copying every
variable from the terminal. Environment values are stored for execution but
never shown by `show`, JSON output, or `doctor`.

## Manage jobs

```console
atx list
atx list --state running --limit 20
```

`--state` accepts one job state: `scheduled`, `waiting`, `starting`,
`running`, `cancel-requested`, `succeeded`, `failed`, `cancelled`,
`interrupted`, or `missed`.

```console
atx show JOB
atx ps
atx history
atx history JOB --limit 20
atx output RUN
atx output JOB
atx cancel JOB
atx cancel JOB --grace 3s
atx rm JOB
atx rm JOB --cancel
atx run JOB
atx run JOB --yes
```

`JOB` may be a complete ID or any unique prefix. Removing a job also deletes
its completed-run history unless `--keep-history` is given. `run --yes` is
required when the last outcome was interrupted because ATX cannot know
whether that command took effect.

`atx output RUN` prints the stdout and stderr captured for one run, labeling
any stream shortened at the capture cap (10 MiB by default). A job ID resolves
to its most recent run; a stream truncated in JSON is marked with
`stdout_truncated`/`stderr_truncated`. Captured logs also live on disk under
the state directory at `runs/<run-id>/stdout.log` and `stderr.log`.

## Output

Use `--json` before or after a management command for scripts:

```console
atx --json list
atx show --json 019...
```

Successful output has `schema_version`, `ok: true`, and `data`. Errors use the
same envelope with `ok: false` and an error code. Existing version 1 fields and
their meanings stay compatible; later releases may add fields. Every field is
listed in the [JSON output reference](json-api.md).

`--quiet` hides successful human output. `--color` accepts `auto`, `always`, or
`never` and controls color. Data goes to stdout and diagnostics go to stderr.

## Configuration

ATX reads an optional TOML file at `<state-dir>/config.toml`
(`~/Library/Application Support/atx/config.toml` on macOS,
`~/.local/state/atx/config.toml` on Linux by default). Values are layered:
file, then environment, then command-line flags win.

```toml
default_timezone = "America/Sao_Paulo" # calendar input without --tz/--utc; default: local
default_runtime = "session"            # "session" or "durable"; default: session
default_shell = "/bin/sh"              # absolute path used by --shell
cancel_grace = "10s"                   # default for cancel --grace
history_days = 30                      # completed-run retention
terminal_job_days = 30                 # hidden-job retention
max_log_bytes_per_stream = 10485760    # per-stream capture cap
color = "auto"                         # auto | always | never
verbosity = "normal"                   # quiet | normal | verbose
```

Shown values are the defaults; omit a key to keep it. Unknown keys and invalid
values are errors, never ignored. Each key also has an `ATX_` uppercase
environment equivalent, e.g. `ATX_HISTORY_DAYS`.

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | success |
| 1 | operation finished with a negative job outcome |
| 2 | invalid command line |
| 3 | missing or ambiguous job ID |
| 4 | job state conflict |
| 5 | requested platform feature unavailable |
| 10 | state database failure |
| 11 | process or supervisor failure |
| 12 | ownership or permission failure |
| 70 | unexpected internal failure |

Scheduling success means the job was saved and its supervisor acknowledged it.
It does not mean the command has already succeeded.

## Service and diagnostics

```console
atx doctor
atx doctor --json
atx service install
atx service status
atx service uninstall
```

`service install` registers a per-user supervisor with launchd (macOS) or
systemd (Linux). It points at the current binary and state directory, so run
it again after moving either. `service status` lists the installed files and
whether the supervisor is running; `service uninstall` removes them. ATX only
touches its own service file and refuses to modify one it did not write.

Durable submissions (`--durable`) require this integration to be installed;
they never quietly fall back to session mode. Session jobs survive closing
the terminal, but not necessarily logout or reboot. See
[reliability](reliability.md) and [platform support](platform-support.md).

## Shell completion

`atx completions --shell SHELL` prints a completion script for `bash`, `zsh`,
`fish`, or `power-shell`. Load it from your shell's startup file:

```console
# bash
atx completions --shell bash > ~/.local/share/bash-completion/completions/atx

# zsh
atx completions --shell zsh > "${fpath[1]}/_atx"

# fish
atx completions --shell fish > ~/.config/fish/completions/atx.fish
```

PowerShell users add the script's content to their profile:
`atx completions --shell power-shell >> $PROFILE`.

## Man pages

The full manual ships as roff pages generated from the same source as the
`atx --help` output. Packagers render them with:

```console
cargo xtask dist-man   # writes dist/man/*.1
```

Set `SOURCE_DATE_EPOCH` to make the page dates reproducible.
