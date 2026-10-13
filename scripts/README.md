# Checks and helpers

`check.sh quick` is the edit loop. `check.sh pre-merge` adds static, hygiene and
man stages; `full` is its compatibility alias. Every failed command stops the
gate and keeps its status. CI uses the same named stages on its runners.

`check.sh msrv` selects the installed Rust 1.85.0 compiler, checks and prints its
version, then checks every workspace member, target and feature with that compiler. It ignores
compiler overrides and wrappers for this check. CI and `check.sh full` use the
same stage.

Run pre-merge from a clean checkout because Cargo rejects dirty publish
rehearsals. Install Rust 1.85.0, Node.js 22+/npm, Git, ShellCheck, actionlint
1.7.12, mandoc and sqlite3, plus the pinned Rust tools below.

| Stage | Work |
| --- | --- |
| `fmt`, `check`, `test`, `clippy` | Format/build/test/lint the workspace |
| `msrv`, `docs`, `dependencies` | Actual Rust 1.85, rustdoc, audit/deny |
| `markdown`, `links-offline` | Tracked Markdown rules and local links |
| `links-online` | Live URLs, 20-second timeout, two retries, concurrency eight |
| `package` | File policy and publish dry run |
| `hygiene` | Shellcheck-all, workflow guards and fixture harnesses |
| `man` | Xtask tests/lint, pages under `dist/man`, mandoc lint |
| `quick` | Format, check, test and Clippy |
| `static` | MSRV, docs, dependencies, Markdown, offline links and package |

Coverage floors, commit titles, native architecture/libc/user variants, extracted
package consumers, CodeQL/dependency review and release archives are separate CI
evidence. Fuzz, mutation and performance have scheduled workflows. A local green
gate does not claim those checks ran. Online links run on documentation PRs and
weekly, separate from offline pre-merge. A read-only `GITHUB_TOKEN` can avoid
GitHub rate limits; no broad failure-status acceptance is configured.

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

## Script contracts

- `check.sh MODE` composes the named stages and makes no remote mutation.
- `check-hygiene.sh` shellchecks every shell script and runs workflow guards,
  aggregate failure tests, Markdown scope tests and mocked link scope tests.
- `check-man.sh` tests/lints xtask, renders `dist/man` and lints its pages. Build
  outputs stay in the selected Cargo target; normal Cargo cache fills may occur.
- `check-links.sh offline|online` selects tracked Markdown with NUL-safe paths,
  escapes linter glob syntax, pins lychee and preserves failures. Offline mode
  makes no network request; online uses the bounds above.
- `test-links-harness.sh [--real]` tests scope, tool versions and statuses in a
  temporary fixture repository. Real mode starts an owned loopback server,
  proves success/404 rejection, then stops and reaps that child.
- `test-check-harness.sh` copies the real gate to an owned fixture, replaces
  external/helper commands at test boundaries, and checks failure propagation.
  It performs no real build, upload or remote Git command and cannot recurse
  into the real hygiene harness. Fixture cleanup does not change this index.
- `check-workflow-guards.sh` actionlints all workflows and asserts required
  policy values. It reads files and makes no remote change.
- `mock_registry.py [PORT [WORKDIR]]` binds the loopback test registry with
  dummy credentials, recording `state.json` and `requests.log` in its work dir.
  It never talks to crates.io and is not a real credential exchange.
- `assert-mock-published.sh VERSION REQUESTS_LOG CRATE_FILE` requires exactly
  one expected mock upload with matching metadata/checksum, reading those files.

Markdown scope, package policy and scanner helper contracts follow below.

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
