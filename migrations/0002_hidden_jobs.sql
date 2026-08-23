ALTER TABLE jobs
ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1));

CREATE INDEX jobs_visible_state_due_idx
ON jobs(hidden, state, next_due_utc);
