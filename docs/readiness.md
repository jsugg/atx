# Production readiness

A dated snapshot of what was checked before calling this crate
release-ready. Re-run the relevant pieces if you change anything big.

Checked against commit `35b6b4d` on 2026-08-23.

## Build and tests

- Clean-room clone at `main`: `cargo build --locked --all-targets` and
  `cargo test --locked` pass from scratch.
- CI matrix green: Linux/macOS unit tests, MSRV 1.85, clippy with
  warnings as errors, rustfmt, doc build, package file-list check,
  `cargo publish --dry-run --locked`.
- Cross-target smoke tests ran in CI for all three release targets
  (native arm64 runner and matching-arch containers), not just locally.

## Release artifacts

- Dry-run rehearsal produced archives + SBOMs + SHA256SUMS; local bytes
  match the CI-generated checksums.
- Every archive and SBOM carries a build-provenance attestation,
  verified both inside CI and independently with
  `gh attestation verify <file> -R jsugg/atx`.
- Archives unpack to a single stripped binary; the Linux build is a
  static-pie musl executable.

## Supply chain

- `cargo deny` licenses/sources clean; `cargo audit` reports no
  vulnerabilities across 112 locked dependencies.
- All GitHub Actions are pinned to full commit SHAs.
- Dependency-review and CodeQL run on every PR.

## Security posture

- Trust boundaries documented in `docs/security-model.md`.
- The publish path uses short-lived OIDC credentials; no workflow reads
  any stored registry token. A `CARGO_REGISTRY_TOKEN` secret may exist
  in the repository as a manual bootstrap credential — CI never touches
  it, and it should be revoked once trusted publishing is active.
- Publishing is disabled: the crates.io job requires a published release
  *and* the repository variable `CRATES_IO_PUBLISH_ENABLED=true`
  (currently unset — zero repository variables exist).

## Repository state

- `protect-main` ruleset: PRs required, force-push and deletion blocked.
- `release` and `crates-io` environments exist; `crates-io` is where the
  publish job runs.
- Sole curated history lives in PR #1; everything after is reviewed PRs.
- Runbooks: `docs/releasing.md` covers tagging, bootstrap, rollback,
  yank criteria, and compromise response. Activation steps for the owner
  are in the same file.
