# Local Checks

`check.sh quick` is the normal edit loop. It formats, checks, tests, and runs
Clippy. Any failing command stops the script and keeps its exit status.

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
