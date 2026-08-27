# Release check

This is a short record of the checks behind the `0.1.1` release candidate.
Run them again after any meaningful change.

Checked on 2026-08-26. The final proof is the set of required GitHub checks on
the commit that receives the `v0.1.1` tag; this file does not try to name its own
commit hash.

## Build and tests

- `./scripts/check.sh full` covers formatting, warnings, clippy, the full test
  suite, Rust 1.85, docs, dependency checks, Markdown, links, the package file
  list, and a crates.io dry run.
- Required checks run tests on Linux and native Apple Silicon and Intel macOS.
  Linux also runs the full suite once as root and once as the ordinary runner
  user.
- Clippy, the Rust 1.85 check, docs, coverage, and CodeQL run on both Linux and
  macOS.
- The S15 late-cancellation regression waits for a terminal run and passes ten
  repeated runs on macOS.

## Coverage, mutation, and fuzzing

- A clean macOS branch-coverage run with the pinned nightly measured 96.57%
  lines, 94.71% regions, and 91.45% branches. CI removes old coverage binaries
  before measuring and requires at least 95% lines and 85% regions on both
  Linux and macOS.
- A local mutation run over duration parsing, calendar syntax, recurrence, and
  state transitions finished all 104 mutants in about eight minutes: 79 were
  caught and 25 could not compile, with no survivors. The nightly workflow uses
  the same four-file scope and saves the complete `mutants.out` report.
- The five fuzz targets cover duration input, calendar input, environment
  files, saved execution data, and IPC frames. Each previously completed its
  five-minute CI budget without a crash; the scheduled workflow keeps running
  them.

## Release files

- The tag must match `Cargo.toml` and a dated changelog section, and each release
  job checks out that exact tag.
- Six native archives cover Apple Silicon and Intel macOS plus x86_64 and arm64
  Linux with GNU libc and musl. Each archive is unpacked and smoke-tested.
- Archives include the binary, licence, README, changelog, and lint-clean man
  pages. Each archive has a CycloneDX SBOM.
- `SHA256SUMS` covers archives and SBOMs. A real tag run attests the archives,
  SBOMs, and checksum file, then verifies those attestations before making a
  draft release. A manual rehearsal publishes and attests nothing.
- The `release` and `crates-io` environments each allow `v*` **tag** refs. The
  main-branch ruleset requires the new Linux/macOS matrix checks and the root
  test job.

## Security and dependencies

- `cargo audit` and `cargo deny` pass on the locked dependency set.
- GitHub Actions use full commit SHAs. The nightly toolchain and the audit,
  deny, Markdown, link, and mutation tools have fixed versions.
- State and runtime paths check owners, modes, file types, and symlinks.
  Cancellation checks boot identity, process start identity, and process group
  before signalling.
- Saved environment values are redacted from normal output. IPC frames and
  external inputs have explicit size and encoding checks.

## Before publishing

1. Merge the candidate only after every required check is green.
2. Tag that exact merge commit as `v0.1.1` and inspect the resulting draft
   release, checksums, SBOMs, and attestations.
3. Publish the draft when it looks right.
4. Do not publish the crate without the owner's exact
   `GREENLIGHT PUBLISH CRATE` authorization.
