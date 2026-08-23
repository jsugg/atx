# Security

## Reporting a problem

Please email `juanpedrosugg+github [at] gmail [dot] com` with a short reproduction
and the affected version. Do not open a public issue for a vulnerability that could
put someone's commands, environment, or local files at risk.

There is no guaranteed response window yet; this is a one-person side-project.
I will acknowledge a useful report as soon as I can and keep the reporter
updated while checking it.

## Supported versions

No public version is supported yet. Once 1.0 ships, the newest minor release
will receive security fixes. Older releases may be asked to upgrade.

## Scope

Useful reports include:

- command arguments gaining unintended shell interpretation;
- signaling a process without a matching ATX identity;
- unsafe state, runtime, database, or log path handling;
- environment values leaking into normal output or diagnostics;
- malformed local data causing command execution.

ATX runs only as the current user and has no network listener or setuid mode.
