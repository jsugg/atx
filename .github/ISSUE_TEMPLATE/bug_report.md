---
name: Bug report
about: Something broke or behaved differently from the docs
labels: bug
---

For security problems, [send a private report](https://github.com/jsugg/atx/security/advisories/new)
instead of using this public form.

**What happened?**

A clear description of the behavior you saw.

**What did you expect?**

The behavior described in the documentation.

**How do you reproduce it?**

Minimal steps, including the exact `atx` command:

```console
atx ...
```

**Diagnostics**

Run this command and paste its sanitized output:

```console
atx doctor --json
```

Use the same `--state-dir` as the failing command, if you set one.
Remove credentials, environment values and private command output.
Redact paths or user names you don't want in a public issue.

Include your OS version and `atx version` output too; doctor does not report them.

- Session or durable mode:
