# Contributing

ATX is a small side-project, so a focused patch is much easier to review than a
large cleanup mixed with a feature.

## Before coding

Start with a test that fails for the reason you expect.

Use Red-Green-Refactor: make one behavior fail, make it pass with the smallest
correct change, then clean it up. Bug fixes need a regression test.

## Local checks

```console
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

Run `./scripts/check.sh full` before calling a phase complete.

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
