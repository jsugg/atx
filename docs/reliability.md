# Reliability

ATX cannot make process creation atomic with whatever side effect a command
causes. It does not claim exactly-once execution.

Once a run enters `Starting`, the default is at-most-once:

1. Record `Starting`.
2. Try to create the command.
3. Record the known result.

A crash between those steps can leave the result unknown. ATX records that run
as `Interrupted` and does not retry it automatically.

Session mode survives terminal closure but not necessarily logout or reboot.
Durable mode asks the user's service manager to restart the supervisor. It
never silently falls back to session mode.

One-shot jobs missed while no supervisor is available are held by default.
Recurring jobs skip old occurrences and continue at the next anchored time.

## Resource budgets

ATX targets laptop-class hardware with up to 10,000 waiting jobs. The
supervisor keeps deadlines in a binary heap, so scheduling work stays
O(log n) per job; due jobs are handed to execution in batches. Job storage
is indexed on `(state, next_due_utc)`, and listings page 100 jobs at a time,
so no supported path scans the whole table repeatedly.

Enforced budgets (release build, `ten_thousand_jobs_meet_submission_and_list_budgets`):

- inserting one job: p95 under 5 ms across 10,000 sequential inserts;
- listing: p95 under 50 ms per 100-job page;
- idle session supervisor exits after 30 seconds of no work, so an empty
  supervisor holds no steady CPU; RSS stays at typical Rust CLI levels
  (single-digit MiB).

The budget test runs as part of `cargo test --release`; it fails loudly if
a change regresses these paths to quadratic behavior.

