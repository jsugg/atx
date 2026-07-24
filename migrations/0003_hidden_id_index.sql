-- Listing pages walk hidden jobs in id order; the (hidden, state, next_due)
-- index cannot serve ORDER BY id, forcing a full scan plus temp b-tree sort
-- per page. This index makes each page a bounded index range seek.
CREATE INDEX jobs_hidden_id_idx ON jobs(hidden, id);
