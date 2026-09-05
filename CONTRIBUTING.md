# Contributing

ATX is a small side-project, so a focused patch is much easier to review than a
large cleanup mixed with a feature.

## Before coding

Start with a test that fails for the reason you expect.

Use Red-Green-Refactor: make one behavior fail, make it pass with the smallest
correct change, then clean it up. Bug fixes need a regression test.

## Local checks

```console
./scripts/check.sh quick
./scripts/check.sh pre-merge
```

`quick` is the edit loop. `pre-merge` adds compiler-bound MSRV, docs, dependency
policy, Markdown, offline links, package/dry run, all shell scripts and harnesses,
workflow guards, and man generation/lint. `full` is an alias for `pre-merge`, not
a claim that every release check ran locally. Use a clean checkout. See
[script prerequisites and stages](scripts/README.md).

CI calls those same named stages. It also checks commit titles, coverage floors,
native architecture/libc/user variants, extracted source packages, and applicable
security/dependency-review/release-archive workflows. Fuzz, mutation, and
performance budgets have separate scheduled workflows. A local green result
does not replace these remote checks.

`./scripts/check.sh links-online` checks live URLs with bounded requests. It runs
on documentation PRs and weekly in CI, separately from the offline pre-merge
gate. Broken links fail visibly; use a read-only `GITHUB_TOKEN` if GitHub rate
limits requests.

For Markdown-only edits, run `./scripts/check.sh markdown`. It uses the same
pinned tool, config, and tracked-file scope as CI. Node.js 22+ and npm are
required; `npx` can download the tool into its cache. Stage new Markdown files
with `git add` so the shared check includes them.

The retention integration test uses the `sqlite3` command. Install it before
running the full suite.

Some permission tests intentionally need an ordinary user because root can
bypass the Unix permission checks they exercise. They skip explicitly when run
as root; use a non-root account or container user when changing those tests.

## Commits

Commits use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```text
feat(time): parse checked compound durations
fix(process): reject a reused pid
docs(reliability): explain missed jobs
```

Keep tests, code, and docs in the same commit. Never hide a failed or skipped
check.

## Code

- Keep the domain independent from files, SQLite, processes, and clocks.
- Prefer typed boundaries over strings and unstructured maps.
- No implicit shell execution.
- No production panic for bad input or an environmental failure.
- Add a dependency only when it clearly removes more risk than it adds.

## Docs

Write plainly. Show a real command, state the limitation, and skip marketing or
corporate language.
