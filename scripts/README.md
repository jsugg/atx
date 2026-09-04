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
cargo install lychee --version 0.24.2 --locked
```

`check.sh markdown` and CI use `check-markdown.sh`, which runs
`markdownlint-cli2@0.23.2` with `.markdownlint-cli2.jsonc`. Install Node.js 22+
and npm first. `npx` may fetch that pinned package into the npm cache on first
use; it does not add a package or lockfile to this repository.

The launcher selects tracked Markdown through Git and passes literal filenames,
including spaces and glob characters. Template exclusions live in the config;
untracked or generated files do not widen the scope. New documents enter the
lint scope after `git add`. `test-markdown-harness.sh --real` checks that scope,
literal paths, linter/Git failure statuses, and real malformed-document rejection
in a temporary fixture repository. It never changes this checkout's index.

Other scripts install nothing. None writes to GitHub or crates.io. Missing or
wrong required tool versions fail with an actionable message.

`check-package-list.sh PACKAGE_LIST` checks Cargo's package file list. Embedded
test fixtures are required and allowed; other tests and internal directories are
excluded. Paths must be relative and contain no traversal components.

The required CI package job also tests the bin from a freshly extracted source
archive. Workspace tests alone cannot catch a missing packaged test fixture.
Use a fresh `CARGO_TARGET_DIR` for the extracted crate too. Reusing the workspace
target can run an older test binary even when the archive contains the new code.

`syft-release.yaml` configures the pinned release scanner. It omits temporary
extraction-file metadata, not the Rust dependency inventory, and disables
update checks. Release jobs scan the actual auditable archive and validate its
inventory with `cargo xtask check-sbom FILE TARGET` before upload and finalize.
