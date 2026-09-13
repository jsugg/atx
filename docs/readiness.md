# Release check

This is a short record of the checks behind the `0.1.1` release candidate.
Run them again after any meaningful change.

Checked on 2026-11-27. The final proof is the set of required GitHub checks on
the commit that receives the `v0.1.1` tag; this file does not try to name its own
commit hash.

## Build and tests

- `./scripts/check.sh pre-merge` (`full` alias) covers formatting, warnings,
  clippy, the full workspace test suite, actual Rust 1.89 MSRV verification,
  docs, dependency checks, shared Markdown lint, offline links, the package
  file list, an isolated crates.io dry run, shell/workflow hygiene harnesses,
  and generated man-page lint.
- Required checks run tests on Linux and native Apple Silicon and Intel macOS,
  plus a Linux root suite, extracted-crate bin tests, and BusyBox musl smoke
  tests.
- Clippy, the Rust 1.89 MSRV check, docs, coverage, and CodeQL run on both
  Linux and macOS.

## Coverage, mutation, and fuzzing

- CI removes old coverage binaries before measuring with the pinned nightly and
  requires at least 95% lines and 85% regions on both Linux and macOS.
- The scheduled mutation workflow checks duration parsing, calendar syntax,
  recurrence, and state transitions and saves the complete `mutants.out`
  report.
- The five fuzz targets cover duration input, calendar input, environment
  files, saved execution data, and IPC frames under the scheduled fuzz
  workflow.

## Release files

- The tag must match `Cargo.toml` and a dated changelog section, and each release
  job checks out that exact tag.
- Six native archives cover Apple Silicon and Intel macOS plus x86_64 and arm64
  Linux with GNU libc and musl. Each archive is unpacked and smoke-tested.
- Archives include the `cargo-auditable` binary, licence, README, changelog,
  and all 17 `mandoc`-clean man pages with resolved cross-references. Each
  archive has a non-empty CycloneDX SBOM validated against the locked Cargo
  runtime graph by `cargo xtask check-sbom`.
- `SHA256SUMS` covers all six archives and six SBOMs. A real tag run attests
  the archives, SBOMs, and checksum file, then verifies those attestations
  before making a draft release. PR and manual rehearsals publish and attest
  nothing.
- The `release` and `crates-io` environments each allow `v*` **tag** refs. The
  `protect-main` ruleset requires merge-commit PR integration, a current base,
  and all 17 required Linux/macOS checks.

## Security and dependencies

- `cargo audit` and `cargo deny` pass on the locked dependency set (`rustix`
  1.1.6, `rusqlite` 0.43.0 / `libsqlite3-sys` 0.40.0).
- GitHub Actions use full commit SHAs. The nightly toolchain and the audit,
  deny, Markdown, link, and mutation tools have fixed versions.
- State and runtime paths check owners, modes, file types, and symlinks.
  Cancellation checks boot identity, process start identity, and process group
  before signalling.
- Saved environment values are redacted from normal output. IPC frames and
  external inputs have explicit size and encoding checks. Private vulnerability
  reporting is enabled and linked from the issue chooser.

## Before publishing

1. Merge the candidate only after every required check is green.
2. Tag that exact merge commit as `v0.1.1` and inspect the resulting draft
   release, checksums, SBOMs, and attestations.
3. Publish the draft when it looks right.
4. Do not publish the crate without the owner's exact
   `GREENLIGHT PUBLISH CRATE` authorization.
