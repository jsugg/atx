# ADR-0003: SQLite Schema and Migrations

- Status: Accepted
- Date: 2026-07-30

## Context

ATX needs a small local database that can survive crashes without asking
people to install or run another service. Job updates and their history also
need to land together.

## Decision

ATX ships SQLite in the crate and uses one strict, foreign-keyed schema.
Every connection enables WAL, full synchronous writes, foreign keys, and a
five-second busy timeout.

Opening a database uses a bounded exclusive advisory lock on its private
directory before creating the file. That lease covers connection setup and
the migration transaction, then ends when opening finishes. Peers wait for
the lease for up to the busy timeout instead of polling a partially initialized
schema. This is a lock-wait limit, not a deadline for healthy filesystem work. The
lease needs no extra lock file and does not change the database format. An
existing file without a schema is still corruption, including a file left by
a failed initializer. Normal database writes keep SQLite's transaction locks.

The supported local filesystems must provide directory advisory locking.
ATX reports an I/O error if locking is unavailable; it does not open unlocked
as a fallback.

Schema changes live as numbered SQL files compiled into the binary. Pending
migrations run in one transaction. The database `user_version` is the quick
compatibility check; the metadata row is there for inspection and future
format details. A database from a newer ATX version is left alone and rejected.

## Consequences

The binary is a little larger, but installs stay simple and SQLite behavior is
consistent across machines. New migrations need rollback tests and an upgrade
fixture. Destructive migrations will also need the backup step described in
the reliability docs before one is added.
