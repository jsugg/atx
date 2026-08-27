# Architecture

ATX is a local scheduler with three process roles:

- the CLI validates a request, writes it to SQLite, and wakes the scheduler;
- one per-user supervisor owns waiting deadlines;
- one short-lived run monitor starts and waits for each active command.

Ten thousand waiting jobs still use one supervisor. There is no sleeping process
per job.

## Boundaries

The dependency direction is:

```text
CLI / supervisor / monitor -> application -> domain
                                      |
                                      +-> infrastructure ports
```

The domain holds validated values, schedules, process identities, execution
details, and valid state changes. It does not know about SQLite, files, signals,
sockets, or platform clocks. The application layer coordinates submission,
cancellation, history, recovery, and services. Infrastructure provides the
database, paths, IPC socket, clocks, processes, and native service manager.

The CLI creates jobs with direct argv execution by default. The supervisor claims
due occurrences; the monitor owns process creation, output capture, completion,
and the final run record.

## State machines

A job starts as `scheduled`, then the supervisor moves it to `waiting` and,
when due, to `starting`. A started command reaches `running`, then one of
`succeeded`, `failed`, `cancelled`, or `interrupted`; `missed` means execution
never started before recovery handled the overdue deadline.
Cancellation first records `cancel-requested`; completion may still win the
race as success, failure, or interruption.

```text
scheduled -> waiting -> starting -> running -> succeeded, failed, or interrupted
scheduled, waiting, starting, or running -> cancel-requested -> cancelled
scheduled or waiting -> missed
```

The matching run state begins at `starting`, may move to `running` or
`cancel-requested`, and then becomes `succeeded`, `failed`, `cancelled`,
or `interrupted`. Terminal run states do not transition again. A cancellation
completion is only `cancelled` after cancellation was committed; otherwise it
is a state-machine error rather than a made-up cancelled result.

For recurring jobs, each completed, failed, interrupted, missed, starting, or
running occurrence may advance the job back to `waiting` for the next anchored
deadline. One-shot jobs do not leave a terminal state.

ATX records every accepted job transition with its actor, reason, timestamp, and
revision. The actors are the CLI, supervisor, monitor, and recovery.

## Persistence and migration

State is one per-user SQLite database, `atx.db`, under the state directory.
It stores jobs, runs, and transition history. Schedules and execution
specifications are JSON fields checked by SQLite; job and run state columns are
also constrained to the known state values.

The current schema version is 3. Migrations are embedded in the executable and
run in one immediate SQLite transaction. It applies the needed migrations,
updates `PRAGMA user_version`, and commits them together. A failed migration
rolls back. ATX refuses to change a database newer than the executable. It also
treats an existing database with no schema as corruption, rather than replacing
it with an empty one.

SQLite uses foreign keys, WAL mode, full synchronous writes, and a bounded busy
timeout. The main indexes cover due-job selection, run history, transition
history, visible-job listing, and hidden-job ID pagination.

## IPC and acknowledgement

The CLI wakes the supervisor through a per-user Unix socket after committing the
job. The protocol is length-prefixed JSON with protocol version 1. Its messages
are `wake`, `ack`, `nack`, and `shutdown`; wake and acknowledgement carry
the job ID and revision.

Frames must be non-empty and no larger than 64 KiB. The receiver checks that
length before allocating the JSON buffer, checks the protocol version, and only
accepts messages through the checked per-user socket. This only wakes the local
supervisor; it is not a network API.

A submission normally succeeds after the job is saved and the supervisor
acknowledges it. If the commit succeeds but the supervisor cannot be reached,
the job stays saved, but the CLI returns exit 11. That makes the gap visible:
the request was stored, but the supervisor may not have started it.

## Recovery and execution semantics

Starting a process cannot be atomic with the command's side effects, so ATX
does not promise exactly-once execution. It records a run as `starting` before
creating the command. After a crash, ATX checks the saved boot and process-start
identities; an uncertain result is `interrupted` and is not retried automatically.

Cancellation validates boot identity, PID start identity, and process group
before signalling. A PID alone is never enough. One-shot jobs missed while no
supervisor is available follow their configured `hold`, `run-latest`, or
`skip` policy. Recurring jobs retain their fixed-rate anchor and do not replay
every old occurrence.

See [reliability](reliability.md) for the user-facing limits and guarantees, and
[security model](security-model.md) for the local trust boundaries.
