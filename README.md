# ATX

[![quality gates](https://github.com/jsugg/atx/actions/workflows/ci.yml/badge.svg)](https://github.com/jsugg/atx/actions/workflows/ci.yml)
[![tests](https://github.com/jsugg/atx/actions/workflows/platform.yml/badge.svg)](https://github.com/jsugg/atx/actions/workflows/platform.yml)
[![security](https://github.com/jsugg/atx/actions/workflows/security.yml/badge.svg)](https://github.com/jsugg/atx/actions/workflows/security.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE-MIT)

> **Pre-release.** ATX is not yet published: there is no crates.io crate, no
> GitHub release, and no prebuilt binaries yet. The install instructions and
> badges below activate when the first release ships.

<!-- RELEASE: remove this notice and enable the two badges below once the
     first release exists on crates.io. -->
<!-- [![crates.io](https://img.shields.io/crates/v/atx.svg)](https://crates.io/crates/atx) -->
<!-- [![Documentation](https://docs.rs/atx/badge.svg)](https://docs.rs/atx) -->

ATX schedules commands for later without making you write a cron entry.

```console
atx 30s -- say "coffee is ready"        # macOS; use notify-send on Linux
atx 2m30s -- make check
atx 15:00 -- ./backup-home
atx 5h -- tmux send-keys -t coding-agent "continue" C-m
```

It runs commands directly by default, keeps a local history in SQLite, and has
no server or account. An optional per-user service can restart the scheduler if
it crashes.

## Install

Not published yet. Until the first release, build from source:

```console
git clone https://github.com/jsugg/atx
cd atx
cargo install --path .
```

Once released: `cargo install atx` (recommended; the binary is
self-contained), or grab a prebuilt archive from the
[releases page](https://github.com/jsugg/atx/releases) — static musl binaries
for Linux (`x86_64`, `aarch64`) plus macOS builds, with SHA-256 checksums.

## What to expect

- Closing the submitting terminal does not cancel a session job.
- Direct mode never joins arguments into a shell command.
- Unknown outcomes are recorded and never retried on their own.
- Cancellation checks process identity before sending a signal.
- Logs, history, retries, and result sets have limits.

Session jobs are not guaranteed to survive logout or reboot. Durable mode uses
`launchd` on macOS or a systemd user service on Linux and must be installed
explicitly.

Shell completion scripts (`atx completions --shell <bash, zsh, fish,
power-shell>`) and roff man pages are generated from the same CLI metadata as
`--help`.

## Documentation

- [Command guide](https://github.com/jsugg/atx/blob/main/docs/cli.md) — commands,
  options, configuration, exit statuses, and man pages.
- [JSON API](https://github.com/jsugg/atx/blob/main/docs/json-api.md) — output
  for scripts.
- [Reliability](https://github.com/jsugg/atx/blob/main/docs/reliability.md) —
  execution, recovery, and limits.
- [Platform support](https://github.com/jsugg/atx/blob/main/docs/platform-support.md)
  — session and durable-mode boundaries.
- [Security model](https://github.com/jsugg/atx/blob/main/docs/security-model.md)
  — local trust boundaries and file protections.
- [Architecture](https://github.com/jsugg/atx/blob/main/docs/architecture.md) —
  processes, state, storage, and IPC.
- [Troubleshooting](https://github.com/jsugg/atx/blob/main/docs/troubleshooting.md)
  — diagnostic commands and recovery pointers.
- [Releasing](https://github.com/jsugg/atx/blob/main/docs/releasing.md) — making
  and reviewing a release.
- [Dependencies](https://github.com/jsugg/atx/blob/main/docs/dependencies.md) —
  direct dependencies and why they are here.
- [Readiness](https://github.com/jsugg/atx/blob/main/docs/readiness.md) — the
  latest check results.
- [Architecture decisions](https://github.com/jsugg/atx/tree/main/docs/adr) —
  short records of durable design choices.

## Platforms

The release target is macOS 13+ and Linux glibc/musl builds for x86-64 and
AArch64; static musl builds are smoke-tested inside BusyBox in CI.
Durability guarantees per platform live in
[platform
support](https://github.com/jsugg/atx/blob/main/docs/platform-support.md).

## Upgrading and uninstalling

- **Cargo installs:** `cargo install --force atx` (or `--path .` from a
  checkout) replaces the binary; `cargo uninstall atx` removes it.
- **Archive installs:** replace the old binary on your `PATH` with the new
  one, then restart the service so the running supervisor is the new build:
  `atx service uninstall && atx service install` (see `atx service --help`).
  Moving or upgrading the binary without restarting leaves the old image
  running until logout or reboot.
- **Man pages from an archive:** copy `man-pages/*.1` to a directory searched by
  your system (for example `/usr/local/share/man/man1/`), then refresh its
  index if your platform needs that (`mandb` on many Linux systems).
- Uninstalling does not delete your state directory (history, config, logs).
  Delete `$XDG_STATE_HOME/atx` (Linux) or
  `~/Library/Application Support/atx` (macOS) yourself if you want history
  gone.

## Hacking on it

The pinned Rust toolchain is installed automatically by `rustup`.

```console
./scripts/check.sh quick
./scripts/check.sh full
```

Read
[CONTRIBUTING.md](https://github.com/jsugg/atx/blob/main/CONTRIBUTING.md)
before changing behavior.

## License

MIT. See [LICENSE-MIT](https://github.com/jsugg/atx/blob/main/LICENSE-MIT).
