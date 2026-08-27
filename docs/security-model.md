# Security model

ATX is a local, per-user scheduler. It runs commands as the current user, does
not elevate privileges, listen on a network interface, or load code from its
state directory. This is not a sandbox: a scheduled command has the same access
as the user who submitted it.

## Things ATX checks

ATX checks command-line arguments, configuration, environment files, SQLite
records, runtime sockets, run logs, and saved process identities. A private
directory alone is not proof that its contents are safe.

Direct execution preserves argv and does not invoke a shell. `--shell` is an
explicit opt-in to the configured shell and prints a warning because the shell
interprets its command string. Never build that string from untrusted text.

## Private paths and files

The state and runtime directories must be real directories owned by the current
user with mode `0700`. For existing directories, ATX checks metadata without
following symlinks. It rejects symlinks, non-directories, another owner, and
any other mode.

Private regular files such as databases, locks, and logs are created owner-only
with mode `0600`. For an existing protected file, ATX opens it without following
symlinks and checks its owner, mode, and file type. It rejects symlinks,
directories, FIFOs, and looser permissions. Unix sockets must also be
owner-owned, mode `0600`, and real socket nodes. A stored leaf name is one normal
path component, so it cannot escape the checked directory.

The SQLite database is also opened with SQLite's no-follow flag. A pre-existing
database is checked for owner, regular-file type, and mode before use. A
truncated or schema-less existing database is reported as corruption; ATX does
not quietly replace it with a new empty database.

## Local IPC

The supervisor wake socket is inside the private runtime directory. ATX checks
its ownership before using it. The JSON frame codec accepts only non-empty
frames up to 64 KiB, checks the protocol version, and rejects an oversize length
before allocating its buffer. The protocol only works locally; it does not let a
different user gain access.

## Secrets and diagnostics

Environment values are stored only when a job needs them. Their type redacts
values from debug formatting, human output, JSON output, and `doctor`; `show`
may list environment **keys**. The default inherited environment is small.
`--capture-env` copies everything from the submitting process and warns that it
may save credentials in the state database.

ATX keeps stdout and stderr in private files with a per-stream cap. The cap
limits accidental disk use but does not make command output safe to share.
Bug reports should include `atx version`, `atx doctor --json`, and a
redacted failing command, not environment values or private output.

## Process control

Before signalling a process group, ATX compares the saved boot identity, PID
start identity, and process group. This prevents a reused PID from being treated
as the original command. It cannot make a command's side effects transactional;
an uncertain result is recorded as interrupted and is not retried automatically.

## What this does not protect

Someone who can read or modify your account can generally submit jobs as you.
ATX does not protect a command from its dependencies, network requests, or shell
code. Keep the state directory private, use direct argv mode where possible, and
treat `--shell`, environment files, and `--capture-env` as sensitive inputs.
