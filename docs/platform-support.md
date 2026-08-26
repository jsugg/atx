# Platform Support

The Tier 1 set is:

| System | Architecture | Notes |
| --- | --- | --- |
| macOS 13+ | Apple silicon, x86-64 | Per-user `launchd` for durable mode |
| Linux glibc | x86-64, AArch64 | `/proc` and `CLOCK_BOOTTIME` required |
| Linux musl (static) | x86-64, AArch64 | Static build; BusyBox-friendly |

Session mode needs a writable user state directory, Unix sockets, POSIX
signals, and process groups. Linux cancellation also needs `/proc` so a PID can
be matched to its start identity.

## What "durable" means on each platform

A **session** job survives its submitting terminal closing but dies with your
login session. A **durable** job is owned by the platform service manager and
survives logout and reboot.

- **macOS:** the `atx-service` launchd agent starts at login (`RunAtLoad`) and
  is kept alive while you are logged in; a rebooted machine resumes jobs when
  you next log in. `service status` reports whether that agent is installed
  and running.
- **Linux (systemd):** the `atx-service` user unit starts with your systemd
  user instance. To keep it running after logout, enable lingering for your
  user (`loginctl enable-linger "$USER"`) — without lingering, the user
  manager stops at logout and durable jobs pause until the next login.
  `service status` maps to `systemctl --user status atx-service`.
- **Non-systemd Linux:** durable mode is unavailable; submission fails with a
  capability error instead of silently degrading, and `atx doctor` reports the
  gap.

Recovery after a crash or reboot follows the documented missed-policy: jobs
whose deadlines passed while nothing was running are held, skipped, or run
once per `--missed`.

Missing capabilities must produce an error or show up in `atx doctor`. ATX does
not quietly lower a requested guarantee.
