# Troubleshooting

Start with:

```console
atx doctor
```

It checks state and runtime permissions, SQLite, clocks, process identity,
supervisor files, timezone data, configuration, and durable-service support.
The JSON form is handy when filing a bug:

```console
atx doctor --json
```

Warnings describe a feature that is absent or degraded. A failing check exits
nonzero and includes a suggested fix. Missing state on a fresh install is only
a warning.

## A job did not run

```console
atx show JOB
atx history JOB
atx ps
```

Session jobs are not promised across logout or reboot. After either event, the
next ATX command reconciles saved work and applies its missed-job policy.
`interrupted` means the command may have started but ATX cannot prove its
outcome; it will not retry that run by itself.

Run output lives below the private state directory at `runs/<run-id>/stdout.log`
and `stderr.log`. The history and output views show the relevant paths. Output
is bounded, so a noisy command may have truncated logs.

## State or supervisor errors

ATX refuses state directories, databases, locks, and sockets with unsafe
owners, modes, or file types. Do not replace them with symlinks or make them
group/world accessible.

Keep a suspect database for diagnosis. Deleting it destroys useful recovery
evidence and history. A stale supervisor warning normally clears on the next
submission, which replaces stale runtime files after validating their owner.

## Shell jobs

Direct mode is safer and is the default. With `--shell`, quoting, expansion,
redirects, and pipelines follow `/bin/sh`; ATX does not make the string safe.
Never build a shell command from untrusted text.

When reporting a problem, include `atx version`, `atx doctor --json`, the
failing command with secrets removed, and the platform/version. Do not include
environment values or private command output.
