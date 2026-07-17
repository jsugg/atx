# Command guide

## Schedule something

Put the time first, then a literal `--`, then the command:

```console
atx 30s -- notify-send "tea is ready"
atx 2m30s -- make check
atx --utc 23:00 -- ./backup-home
atx --tz America/Sao_Paulo "2026-08-03 20:30" -- ./report
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
```

By default ATX keeps a small safe environment rather than copying every
variable from the terminal. Environment values are stored for execution but
never shown by `show`, JSON output, or `doctor`.

## Manage jobs

```console
atx list
atx list --state running --limit 20
atx show JOB
atx ps
atx history
atx history JOB --limit 20
atx cancel JOB
atx cancel JOB --grace 3s
atx rm JOB
atx rm JOB --cancel
atx run JOB
atx run JOB --yes
```

`JOB` may be a complete ID or any unique prefix. Removing a job hides it but
keeps its run history. `run --yes` is required when the last outcome was
interrupted because ATX cannot know whether that command took effect.

## Output

Use `--json` before or after a management command for scripts:

```console
atx --json list
atx show --json 019...
```

Successful output has `schema_version`, `ok: true`, and `data`. Errors use the
same envelope with `ok: false` and an error code. Existing version 1 fields and
their meanings stay compatible; later releases may add fields.

`--quiet` hides successful human output. `--color auto|always|never` controls
color. Data goes to stdout and diagnostics go to stderr.

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

Session jobs survive closing the terminal, but not necessarily logout or
reboot. `--durable` never quietly falls back to session mode. See
[reliability](reliability.md) and [platform support](platform-support.md).
