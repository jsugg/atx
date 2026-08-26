# JSON output reference

Every command that prints data accepts a global `--json` flag. Machine
output always is a single JSON object on stdout, one line, ending in a
newline.

## Envelope

```json
{
  "schema_version": 1,
  "ok": true,
  "data": { }
}
```

Errors swap `data` for `error`:

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "code": "JOB_NOT_FOUND",
    "message": "no job matches prefix 'nosuch'",
    "remediation": "Run `atx list` to inspect job IDs."
  }
}
```

- `schema_version` is `1` today. It increases when a field changes meaning
  or is removed; additive new fields do not bump it.
- `remediation` is optional and may be added to any error at any time.
- Unknown fields must be ignored by consumers.
- Diagnostics and warnings always go to stderr; stdout carries only the
  envelope. Exit codes are documented in the CLI guide.

## Commands

| Command | `data` shape |
| --- | --- |
| `list --json` | array of job objects |
| `show JOB --json` | one job object |
| `history [JOB] --json` | array of run objects |
| `output RUN --json` | run-output object |
| `ps --json` | array of process objects |

## Object shapes

Job object (`list`, `show`):

| Field | Type | Notes |
| --- | --- | --- |
| `job_id` | string | full UUID |
| `name` | string \| null | |
| `description` | string \| null | |
| `schedule` | object | one of `one_shot_relative`, `one_shot_absolute`, `recurring_interval` |
| `next_due_utc` | string | RFC 3339 UTC timestamp |
| `remaining_seconds` | integer | negative once due |
| `state` | string | see state names below |

| `runtime_tier` | string | `Session` or `Durable` |
| `execution` | object | see below |
| `active_run_id` | string \| null | |

The last run's outcome is not part of the job object; read it from
`history` or `output`.

Job states: `scheduled`, `waiting`, `starting`, `running`,
`cancel_requested`, `succeeded`, `failed`, `cancelled`, `interrupted`,
`missed`.

Execution object:

| Field | Type | Notes |
| --- | --- | --- |
| `mode` | string | `Direct` or `Shell` |
| `argv` | array of strings | |
| `working_directory` | string | |
| `environment_keys` | array of strings | keys only, never values |
| `shell_path` | string \| null | `Shell` mode only |

Run object (`history`):

| Field | Type | Notes |
| --- | --- | --- |
| `run_id` | string | full UUID |
| `job_id` | string | |
| `sequence` | integer | per-job, starts at 1 |
| `scheduled_for_utc` | string | RFC 3339 UTC timestamp |
| `started_at_utc` | string \| null | |
| `finished_at_utc` | string \| null | |
| `state` | string | `starting`, `running`, `cancel_requested`, `succeeded`, `failed`, `cancelled`, `interrupted` |
| `outcome` | string \| null | terminal runs only |
| `stdout_path` / `stderr_path` | string \| null | paths under the state directory |

Run-output object (`output RUN --json`):

| Field | Type | Notes |
| --- | --- | --- |
| `run_id`, `job_id` | string | |
| `state` | string | |
| `outcome` | string \| null | |
| `stdout_truncated` / `stderr_truncated` | boolean | true once the capture cap cut the stream |
| `stdout` / `stderr` | string | captured bytes decoded as UTF-8; invalid sequences become U+FFFD |

Captured streams are stored raw on disk under the logged
`stdout_path`/`stderr_path`; only this JSON view applies lossy UTF-8
decoding. Binary output should be read from those files, not from JSON.

Process object (`ps --json`): `job_id`, `run_id`, `role` (`monitor` or
`command`), `pid`, `process_group_id`, `state`.

Submission object (scheduling commands like `atx 30s -- cmd --json`):
`job_id`, `state`, `schedule`, `next_due_utc`, `runtime_tier`,
`supervised` (boolean), `dry_run` (boolean).
