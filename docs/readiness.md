# Production readiness

A dated snapshot of what was checked at the current release candidate.
Re-run the relevant pieces if you change anything big.

Candidate: commit `75f625e` on `main`, checked 2026-08-26 from a clean
worktree with no local modifications. The previous snapshot (commit
`35b6b4d`) was superseded after an independent audit found release-blocking
defects; every finding is now closed or explicitly deferred in the working
notes, and this document records fresh evidence for the new candidate.

## Build and tests

- Clean worktree at the candidate commit:
  `./scripts/check.sh quick` and `./scripts/check.sh full` pass
  (fmt, clippy `-D warnings` pedantic, MSRV, doc build, audit, deny,
  mdlint, lychee, package file-list, full unit + integration suites).
- Full test matrix green locally on macOS ARM64, and required CI checks
  cover Linux plus both Intel/ARM macOS runners.
- Acceptance suite waits for terminal states and was stress-repeated
  locally ×10; S11 exercises the real capture cap with 12 MiB streams.

## Performance evidence

- Release-profile budget test
  (`infrastructure::sqlite::job_store::tests::
  ten_thousand_jobs_meet_submission_and_list_budgets`) passes under
  `--release`: submission p95 ≈ 313 µs, listing p95 ≈ 1.6 ms per
  100-job page. A nightly workflow re-measures this on fixed hardware.

## Coverage / mutation / fuzz

- Branch coverage at the candidate (llvm-cov with branch mode,
  all targets and features, pinned nightly): lines 94.8 percent,
  regions 87.3, functions 96.6. HTML report uploads as a CI artifact.
- cargo-mutants over `src/domain/` and `src/infrastructure/config/`:
  442 mutants — 33 caught, 408 unviable, 1 missed. The survivor
  (`>` vs `>=` on the 255-byte timezone-name cap) was killed by a new
  boundary test. A nightly mutation workflow keeps future survivors
  visible.
- Five fuzz targets (duration, calendar, env_file,
  execution_persistence, ipc_frame) each ran a 300 s budget against
  committed seed corpora with the pinned nightly; no crashes.

## JSON contract

- `docs/json-api.md` documents the versioned envelope (`schema_version`)
  and every field of every machine-readable payload; golden key-set
  end-to-end tests pin those shapes against the real binary so doc and
  code cannot drift silently.

## Release artifacts

- `release.yml` checks out the exact tag for every job and proves
  HEAD == tagged commit before anything builds; tag must equal
  Cargo.toml version and match a changelog section whose text becomes
  the release notes.
- Archives carry the binary, LICENSE-MIT, README, and generated man
  pages; CI extracts and inspects each archive like a consumer before
  upload. SBOMs, SHA256SUMS, and build-provenance attestations are
  produced and verified inside the run.
- The publish path uses short-lived OIDC credentials; no workflow reads
  a stored registry token. Publishing to crates.io additionally requires
  the repository variable `CRATES_IO_PUBLISH_ENABLED=true`
  (currently unset — zero repository variables exist) and stays gated on
  the owner's separate recorded authorization.

## Supply chain

- `cargo deny` licenses/sources clean; `cargo audit` reports no known
  vulnerabilities across the locked dependency set.
- All GitHub Actions are pinned to full commit SHAs.
- Dependency-review and CodeQL run on every PR.

## Security posture

- Trust boundaries documented in `docs/security-model.md`.
- Malformed external input (including non-UTF-8 environment entries)
  produces typed errors instead of panics; one failing due job cannot
  suppress its batch and is retried with a bounded backoff.
- State and runtime paths reject unsafe owners, modes, and symlink
  substitution; background processes open logs through a
  directory-relative no-follow helper.
- Cancellation verifies boot identity, PID start token, and process
  group before signaling.
- Configuration keys (`history_days`, `terminal_job_days`,
  `max_log_bytes_per_stream`) demonstrably change background-process
  behavior, verified by end-to-end tests with non-default values.

## What remains before publishing

- Remote CI green on the candidate commit (required ruleset checks).
- A GitHub release rehearsal from an annotated tag proving archives,
  checksums, SBOMs, and attestations end-to-end.
- Owner's recorded `GREENLIGHT PUBLISH CRATE` authorization.
