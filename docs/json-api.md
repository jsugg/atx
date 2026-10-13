# JSON output reference

Use `--json` to read command results from a script. Put it before the scheduling
separator, for example `atx --json 30s -- echo hello`. Anything after `--` belongs
to the command being scheduled.

Successful data commands print one compact JSON object to stdout, followed by a
newline. Command errors print an error envelope to stderr and leave stdout empty.
Warnings and other diagnostics also use stderr. Check the exit status before
reading stdout. See [the CLI guide](cli.md) for exit codes.

An unexpected CLI panic exits `70` and writes `atx: unexpected internal failure`
as text to stderr, not an error envelope. Ignore stdout in that case.

`completions`, `--help`, and the built-in `--version` print text, not JSON.
Use the `version` subcommand for machine-readable version information. The optional
`man` command writes manual files rather than a JSON result.

## Envelope

Every success envelope has these fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | JSON contract version, currently `1` |
| `ok` | boolean | `true` for a result, `false` for a command error |
| `data` | object or array | command-specific result, present only on success |

Errors replace `data` with an `error` object:

| Field | Type | Meaning |
| --- | --- | --- |
| `code` | string | stable error code, such as `JOB_NOT_FOUND` or `INVALID_ARGUMENT` |
| `message` | string | explanation for the user, not a value to match in scripts |
| `remediation` | string, optional | suggested next step; omitted when absent |

Consumers must ignore unknown fields. Adding a field does not change
`schema_version`; removing a field or changing its meaning does. Fields marked
`null` below remain present when they have no value. Optional fields are omitted.

### Version example

`atx --json version` prints:

```json
{"schema_version":1,"ok":true,"data":{"version":"0.1.1"}}
```

### Error example

With no saved database, `atx --json show nosuch` exits `3` and writes this to stderr:

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "code": "JOB_NOT_FOUND",
    "message": "job or run not found",
    "remediation": "Run `atx list` to inspect job IDs."
  }
}
```

The example is expanded for readability. The binary prints it on one line.

## Commands

All commands in this table accept `--json`.

| Command | `data` shape |
| --- | --- |
| scheduling, including `--dry-run` and `--every` | submission object |
| `list` | array of job objects |
| `show JOB` | job object |
| `cancel JOB` | job object after cancellation |
| `rm JOB` | removed job object |
| `run JOB` | job object prepared for another run |
| `history [JOB]` | array of run objects |
| `output RUN` | run-output object |
| `ps` | array of process objects |
| `doctor` | doctor report |
| `version` | object with a `version` string |
| `service status` | service-status object |
| `service install`, `service uninstall` | service-change object |

Empty `list`, unfiltered `history`, and `ps` results are arrays, `[]`, including
when no database exists yet. `run` returns the prepared job, not a completed run.
Read `history` or `output` to learn how the command finished.

## Shared values

Job and run IDs are 26-character lowercase Crockford Base32 strings derived from
UUIDv7, not hyphenated UUID strings. Their alphabet is
`0123456789abcdefghjkmnpqrstvwxyz`; the first character is between `0` and `7`.
They sort lexicographically in UUID order. For example, an ID looks like
`01m4n2rkfzedssxyfwgcag90fn`. CLI prefixes may use either case.

Timestamps are RFC 3339 strings in UTC, ending in `Z`. They may contain fractional
seconds. Durations are strings, not counts of seconds. ATX formats them with
ordered `h`, `m`, and `s` components and omits zero components, such as `30s`, `2m`,
or `1h2m3s`.

JSON enum values use snake case:

- Job states are `scheduled`, `waiting`, `starting`, `running`, `cancel_requested`,
  `succeeded`, `failed`, `cancelled`, `interrupted`, and `missed`.
- Run states are `starting`, `running`, `cancel_requested`, `succeeded`, `failed`,
  `cancelled`, and `interrupted`.
- Runtime tiers are `session` and `durable`.
- Execution modes are `direct` and `shell`.

These are JSON values. CLI option values may use different spelling, such as
`--missed run-latest`. The missed-deadline policy is not part of the job JSON object.

## Submission object

| Field | Type | Meaning |
| --- | --- | --- |
| `job_id` | string | full job ID |
| `state` | string | job state |
| `schedule` | object | schedule variant below |
| `next_due_utc` | string | next deadline in UTC |
| `runtime_tier` | string | `session` or `durable` |
| `supervised` | boolean | submission received supervision acknowledgement |
| `dry_run` | boolean | validated without saving or running the job |

## Job object

| Field | Type | Meaning |
| --- | --- | --- |
| `job_id` | string | full job ID |
| `name` | string or null | optional display name |
| `description` | string or null | optional note |
| `schedule` | object | schedule variant below |
| `next_due_utc` | string | next deadline in UTC |
| `remaining_seconds` | integer | signed seconds until due, truncated toward zero |
| `state` | string | job state |
| `runtime_tier` | string | `session` or `durable` |
| `execution` | object | execution settings below |
| `active_run_id` | string or null | currently always null; use `ps` or `history` for runs |

The last run's outcome is not part of the job object. Read it from `history` or
`output`. `remaining_seconds` can be zero just after the deadline and becomes
negative once the job is at least one second overdue.

### Execution object

| Field | Type | Meaning |
| --- | --- | --- |
| `mode` | string | `direct` or `shell` |
| `argv` | array of strings | saved command arguments |
| `working_directory` | string | canonical absolute working directory |
| `environment_keys` | array of strings | saved environment keys, never their values |
| `shell_path` | string or null | shell executable in `shell` mode; null in `direct` mode |

### Schedule variants

Each schedule has a `kind` string and the fields listed for that kind.

`one_shot_relative`:

| Field | Type | Meaning |
| --- | --- | --- |
| `duration` | string | canonical duration, such as `30s` |
| `persisted_due_utc` | string | saved UTC deadline |

`one_shot_absolute`:

| Field | Type | Meaning |
| --- | --- | --- |
| `original_input` | string | submitted calendar expression |
| `timezone` | string | resolved timezone name, such as `UTC` or `Europe/London` |
| `timezone_database_version` | string | bundled timezone data version used to resolve the input |
| `resolved_due_utc` | string | resolved UTC deadline |
| `dst_resolution` | string | `reject`, `earlier`, or `later` |

`recurring_interval`:

| Field | Type | Meaning |
| --- | --- | --- |
| `interval` | string | canonical duration, such as `2m` |
| `persisted_anchor_utc` | string | saved fixed-rate UTC anchor |

## Run object

| Field | Type | Meaning |
| --- | --- | --- |
| `run_id` | string | full run ID |
| `job_id` | string | full job ID |
| `sequence` | integer | positive per-job sequence, starting at `1` |
| `scheduled_for_utc` | string | intended UTC deadline |
| `started_at_utc` | string or null | UTC start time, if started |
| `finished_at_utc` | string or null | UTC finish time, if finished |
| `state` | string | run state |
| `outcome` | object or null | terminal outcome below; null while unfinished |
| `stdout_path` | string or null | raw stdout file under the state directory, if available |
| `stderr_path` | string or null | raw stderr file under the state directory, if available |

### Outcome object

Outcomes are tagged objects with `kind` and `value`, not strings.

| `kind` | `value` type | Meaning |
| --- | --- | --- |
| `exit` | integer | command exit code; `0` means success |
| `signal` | integer | signal that terminated the command |
| `failure` | string | failure explanation, such as an execution setup error |
| `interrupted` | string | explanation when the outcome cannot be established |
| `cancelled` | string | cancellation explanation |

### Outcome examples

These examples show all five variants. Explanation text is not a stable code.

```json
[
  {"kind": "exit", "value": 0},
  {"kind": "signal", "value": 15},
  {"kind": "failure", "value": "command could not start"},
  {"kind": "interrupted", "value": "outcome unknown after restart"},
  {"kind": "cancelled", "value": "cancelled by user"}
]
```

## Run-output object

| Field | Type | Meaning |
| --- | --- | --- |
| `run_id` | string | full run ID |
| `job_id` | string | full job ID |
| `state` | string | run state |
| `outcome` | object or null | same outcome object as `history` |
| `stdout_truncated` | boolean | capture cap cut the stdout stream |
| `stderr_truncated` | boolean | capture cap cut the stderr stream |
| `stdout` | string | captured stdout decoded as UTF-8 |
| `stderr` | string | captured stderr decoded as UTF-8 |

Invalid UTF-8 becomes U+FFFD in this view. ATX stores the captured bytes unchanged
in the files named by `stdout_path` and `stderr_path`. Read those files rather than
JSON for binary output.

## Process object

`ps` lists the processes for active runs. It does not list the supervisor.

| Field | Type | Meaning |
| --- | --- | --- |
| `job_id` | string | full job ID |
| `run_id` | string | full run ID |
| `role` | string | `monitor` or `command` |
| `pid` | integer | positive process ID |
| `process_group_id` | integer | process group ID |
| `state` | string | run state |

## Doctor report

`doctor` can return a success envelope with `ok: true` and still exit nonzero when
the report is unhealthy. `ok` says a report was produced; `healthy` describes its
checks. Warnings alone do not make the report unhealthy.

| Field | Type | Meaning |
| --- | --- | --- |
| `healthy` | boolean | false if any check has status `fail` |
| `checks` | array of check objects | diagnostic results |
| `tzdb_version` | string | bundled timezone data version |
| `schema_version` | integer or null | SQLite schema version, null when no database exists |
| `durable_available` | boolean | durable service is available and running |
| `config` | object | effective configuration without environment values |

The report's `schema_version` is the SQLite schema version, not the envelope's
JSON contract version.

### Check object

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | string | check name |
| `status` | string | `pass`, `warning`, or `fail` |
| `message` | string | diagnostic explanation |
| `remediation` | string, optional | suggested next step; omitted when absent |

Check messages and names are diagnostic text. Do not match them as stable codes.

### Effective configuration object

| Field | Type | Meaning |
| --- | --- | --- |
| `cancel_grace` | string | canonical cancellation grace duration |
| `color` | string | `auto`, `always`, or `never` |
| `default_runtime` | string | `session` or `durable` |
| `default_shell` | string | default shell path |
| `default_timezone` | string | timezone setting, such as `local` |
| `history_days` | integer | run-history retention period |
| `max_log_bytes_per_stream` | integer | byte cap for each captured stream |
| `terminal_job_days` | integer | terminal-job retention period |
| `verbosity` | string | `quiet`, `normal`, or `verbose` |

## Service objects

### Service-status object

| Field | Type | Meaning |
| --- | --- | --- |
| `manager` | string | platform service manager name |
| `availability` | string | `available` or `unavailable` |
| `installed` | boolean | service files are installed |
| `running` | boolean | service is running |
| `files` | array of strings | platform service file paths |
| `guarantee` | string | explanation of the platform's durable behavior |
| `detail` | string | current status explanation |

### Service-change object

| Field | Type | Meaning |
| --- | --- | --- |
| `changed` | boolean | install or uninstall changed the service |
| `status` | object | service-status object after the operation |
