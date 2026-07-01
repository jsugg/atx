# ATX

ATX schedules commands for later without making you write a cron entry.

```console
atx 30s -- notify-send "tea is ready"
atx 2m30s -- make check
atx 15:00 -- ./backup-home
```

It runs commands directly by default, keeps a local history in SQLite, and has
no server or account. An optional per-user service can keep the scheduler
running across supervisor crashes.

ATX is still being built. The command examples above describe the target
interface, not a published release.

## What it promises

- Closing the submitting terminal does not cancel a session job.
- Direct mode never joins arguments into a shell command.
- Unknown outcomes are recorded and never retried on their own.
- Cancellation checks process identity before sending a signal.
- Logs, history, retries, and result sets have limits.

Session jobs are not guaranteed to survive logout or reboot. Durable mode uses
`launchd` on macOS or a systemd user service on Linux and must be installed
explicitly.

See [reliability](docs/reliability.md) for the less-short version.

## Platforms

The release target is macOS 13+ and Linux 5.4+. Static musl builds are intended
for BusyBox systems. Current details live in
[platform support](docs/platform-support.md).

## Hacking on it

The pinned Rust toolchain is installed automatically by `rustup`.

```console
./scripts/check.sh quick
./scripts/check.sh full
```

The scripts arrive with the crate foundation. Until then, run:

```console
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing behavior.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
