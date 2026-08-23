# Platform Support

The planned Tier 1 set is:

| System | Architecture | Notes |
| --- | --- | --- |
| macOS 13+ | Apple silicon, x86-64 | Per-user `launchd` for durable mode |
| Linux 5.4+ glibc | x86-64, AArch64 | `/proc` and `CLOCK_BOOTTIME` required |
| Linux 5.4+ musl | x86-64, AArch64 | Static build; BusyBox-friendly |

Session mode needs a writable user state directory, Unix sockets, POSIX
signals, and process groups. Linux cancellation also needs `/proc` so a PID can
be matched to its start identity.

Missing capabilities must produce an error or show up in `atx doctor`. ATX does
not quietly lower a requested guarantee.
