# Changelog

This file tracks user-visible changes. It follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic
versioning.

## Unreleased

### Added

## [0.1.1] - 2026-08-26

### Changed

- Human-readable job and run output now uses local timestamps with a numeric
  offset, clearer outcome text, and short messages for empty lists.
- `-v` is a single verbose switch; repeated `-vv` is rejected instead of
  implying additional undocumented verbosity levels.

### Fixed

- Run updates now use the same checked transition table as the rest of the
  scheduler, including cancellation precedence.
- Late cancellation no longer races against an unfinished history row in the
  acceptance test.
- `list`, `history`, and `ps` return an empty result on a fresh install instead
  of reporting a missing database.
- Invalid relative durations keep their duration-specific error messages.
- Cancellation update conflicts return the documented state-conflict status,
  and `show` no longer hides run-history storage errors.
- Release archives now include checked SBOMs, checksums, and provenance, and
  both protected release environments accept version tags.

## [0.1.0] - 2026-08-26

### Added

- One-shot and fixed-rate job scheduling with relative times, calendar
  times, UTC, and explicit IANA timezones.
- Direct argv execution by default; opt-in `--shell` mode with an
  interactive warning.
- Per-user session supervisor with secure single-instance IPC, deadline
  heap, and startup reconciliation for interrupted and missed jobs.
- Short-lived run monitors that capture bounded output and record
  identity-checked cancellation.
- Durable mode via `launchd` (macOS) or a systemd user service (Linux),
  installed and managed with `atx service`.
- `atx doctor` diagnostics covering permissions, SQLite, clocks,
  process identity, supervisor state, timezone data, configuration, and
  durable-service support.
- Stable human and `--json` output envelopes.
- Shell completion scripts (`atx completions`) for bash, zsh, fish, and
  PowerShell.
- Man-page generation from CLI metadata (`cargo xtask dist-man`).
- Embedded SQLite storage: WAL, revision-safe transactions, run
  history, retention limits, and corruption detection.

### Changed

### Deprecated

### Removed

### Fixed

- The supervisor now stops cleanly when a service manager sends
  `SIGTERM`/`SIGINT`: the runtime socket and lock are removed and the
  process exits with status 0 instead of dying on the default signal
  disposition. It also creates its state directory when the service
  unit did not provide one.
- Two supervisors or clients initializing the same new state
  directory concurrently no longer race: the loser briefly waits for
  the winner's schema transaction instead of reporting corruption.
- `atx rm` now honors its documented history contract: removing a job
  deletes its completed-run history unless `--keep-history` is given;
  previously the flag was accepted but ignored.
- launchd availability probing no longer depends on the host
  environment during tests.

### Security

- State and runtime paths reject unsafe owners, modes, and symlink
  substitution.
- Cancellation verifies boot identity, PID start token, and process
  group before signaling.
- IPC frames are size-capped, version-checked, and accepted only from
  the socket owner's process.
- Environment values are stored protected and redacted from all normal
  output.
