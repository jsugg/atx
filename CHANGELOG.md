# Changelog

This file tracks user-visible changes. It follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic
versioning.

## Unreleased

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
