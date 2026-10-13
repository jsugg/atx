# Changelog

This file tracks user-visible changes. It follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic
versioning.

## [0.1.1] - Unreleased

First public release candidate. No GitHub release or crate has been published
yet. The public `v0.1.0` tag below was an unpublished rehearsal, not a supported
release. The publication date will be recorded when this version ships.

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
- Local timestamps with numeric offsets in human output, bounded log tails,
  and `atx output` for captured streams. Optional `--tty` echo validates the
  terminal and sanitizes a limited tail without changing raw capture files.
- Shell completion scripts (`atx completions`) for bash, zsh, fish, and
  PowerShell.
- Man-page generation from CLI metadata (`cargo xtask dist-man`).
- Embedded SQLite storage: WAL, revision-safe transactions, run
  history, retention limits, and corruption detection.
- Defined exit codes and a stable exit-70 diagnostic for unexpected CLI
  unwinding panics. Normal command errors keep their human or JSON form.
- Six native release-archive targets, embedded Rust dependency metadata,
  validated CycloneDX inventories, and SHA-256 checksums. Tag builds also
  attest archives, inventories, and the checksum manifest before draft release.

### Security

- State and runtime paths reject unsafe owners, modes, and symlink
  substitution.
- Cancellation verifies boot identity, PID start token, and process
  group before signaling.
- IPC frames are size-capped, version-checked, and accepted only from
  the socket owner's process.
- Environment values are stored protected and redacted from all normal
  output.

## [0.1.0] - 2026-08-26

Unpublished release rehearsal. No GitHub release or crates.io publication was
made for this version. The public tag remains unchanged; its source is
historical rehearsal code, not a supported release.
