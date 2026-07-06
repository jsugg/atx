CREATE TABLE metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE migrations (
    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
    applied_at_utc TEXT NOT NULL
) STRICT;

CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    name TEXT,
    description TEXT,
    created_at_utc TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN (
            'scheduled', 'waiting', 'starting', 'running', 'cancel_requested',
            'succeeded', 'failed', 'cancelled', 'interrupted', 'missed'
        )
    ),
    runtime_tier TEXT NOT NULL CHECK (runtime_tier IN ('session', 'durable')),
    schedule_json TEXT NOT NULL CHECK (json_valid(schedule_json)),
    missed_policy TEXT NOT NULL CHECK (missed_policy IN ('hold', 'run-latest', 'skip')),
    execution_json TEXT NOT NULL CHECK (json_valid(execution_json)),
    next_due_utc TEXT,
    timezone_database_version TEXT NOT NULL,
    owner_uid INTEGER NOT NULL CHECK (owner_uid >= 0)
) STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    scheduled_for_utc TEXT NOT NULL,
    created_at_utc TEXT NOT NULL,
    started_at_utc TEXT,
    finished_at_utc TEXT,
    state TEXT NOT NULL CHECK (
        state IN (
            'starting', 'running', 'cancel_requested', 'succeeded', 'failed',
            'cancelled', 'interrupted'
        )
    ),
    claim_token BLOB NOT NULL UNIQUE CHECK (length(claim_token) = 32),
    monitor_identity_json TEXT CHECK (
        monitor_identity_json IS NULL OR json_valid(monitor_identity_json)
    ),
    command_identity_json TEXT CHECK (
        command_identity_json IS NULL OR json_valid(command_identity_json)
    ),
    process_group_id INTEGER,
    exit_code INTEGER,
    terminating_signal INTEGER,
    failure TEXT,
    outcome_json TEXT CHECK (outcome_json IS NULL OR json_valid(outcome_json)),
    stdout_path TEXT,
    stderr_path TEXT,
    stdout_truncated INTEGER NOT NULL DEFAULT 0 CHECK (stdout_truncated IN (0, 1)),
    stderr_truncated INTEGER NOT NULL DEFAULT 0 CHECK (stderr_truncated IN (0, 1)),
    UNIQUE (job_id, sequence),
    UNIQUE (job_id, scheduled_for_utc),
    CHECK (exit_code IS NULL OR terminating_signal IS NULL)
) STRICT;

CREATE TABLE transitions (
    id INTEGER PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES runs(id) ON DELETE SET NULL,
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    occurred_at_utc TEXT NOT NULL,
    actor TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(reason) > 0 AND instr(reason, char(0)) = 0),
    revision INTEGER NOT NULL CHECK (revision > 0)
) STRICT;

CREATE INDEX jobs_state_due_idx ON jobs(state, next_due_utc);
CREATE INDEX runs_job_sequence_idx ON runs(job_id, sequence DESC);
CREATE INDEX runs_state_idx ON runs(state);
CREATE INDEX transitions_job_revision_idx ON transitions(job_id, revision);

INSERT INTO metadata(key, value) VALUES ('schema_version', '1');
INSERT INTO migrations(version, applied_at_utc)
VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
