# Security Model

ATX runs commands only as the current user. It does not elevate privileges,
listen on a network interface, or load code from its state directory.

The main trust boundaries are local command input, configuration, environment
files, SQLite records, runtime sockets, and process identities. Sensitive paths
must be owned by the current user and reject symlink substitution.

Direct execution preserves argv without shell parsing. Shell execution is
explicit and warns interactively. Environment values are stored in protected
state when needed but are redacted from normal output and diagnostics.

SQLite records are validated when decoded; a truncated or schema-less
database file is reported as corruption instead of silently rebuilt.

IPC frames are size-capped before allocation, version-checked, and only
honored from the socket's owner.

Cancellation uses a boot identity, PID start token, and process group. A PID by
itself is never enough to send a signal.
