# Local checks

`check.sh quick` is the normal edit loop. It checks formatting, builds, tests, and runs
Clippy. Any failing command stops the script and keeps its exit status.

`check.sh msrv` selects the installed Rust 1.85.0 compiler, checks and prints its
version, then checks all targets and features with that compiler. It ignores
compiler overrides and wrappers for this check. CI and `check.sh full` use the
same stage.

`check.sh full` adds MSRV, rustdoc, dependency policy, docs, package contents,
and a crates.io dry run. Run it from a clean checkout because Cargo rejects a
dirty publish rehearsal.

Pinned extra tools:

```sh
cargo install cargo-audit --version 0.22.2 --locked
cargo install cargo-deny --version 0.20.2 --locked
cargo install markdownlint-rs --version 0.3.22 --locked
cargo install lychee --version 0.24.2 --locked
```

The scripts install nothing and never write to GitHub or crates.io. Missing or
wrong tool versions fail with an actionable message.

`check-package-list.sh PACKAGE_LIST` checks Cargo's package file list. Embedded
test fixtures are required and allowed; other tests and internal directories are
excluded. Paths must be relative and contain no traversal components.

The required CI package job also tests the bin from a freshly extracted source
archive. Workspace tests alone cannot catch a missing packaged test fixture.
