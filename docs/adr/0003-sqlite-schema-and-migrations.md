# ADR 0003: SQLite schema and migrations

Status: accepted

## Context

ATX needs a small local database that can survive crashes without asking
people to install or run another service. Job updates and their history also
need to land together.

## Decision

ATX ships SQLite in the crate and uses one strict, foreign-keyed schema.
Every connection enables WAL, full synchronous writes, foreign keys, and a
five-second busy timeout.

Schema changes live as numbered SQL files compiled into the binary. Pending
migrations run in one transaction. The database `user_version` is the quick
compatibility check; the metadata row is there for inspection and future
format details. A database from a newer ATX version is left alone and rejected.

## Consequences

The binary is a little larger, but installs stay simple and SQLite behavior is
consistent across machines. New migrations need rollback tests and an upgrade
fixture. Destructive migrations will also need the backup step described in
the reliability docs before one is added.
