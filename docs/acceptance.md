# Acceptance checks

These are the behavior checks for ATX v1. Each scenario has one primary test.
Some use the CLI, while others check a native process, SQLite, or a domain
invariant directly. A primary test is a starting point for review, not a claim
that it covers every fault on every platform.

S1 through S25 keep the IDs used by the existing CLI cases. S26 names the
separate reconciliation requirement. The CLI matrix also has smoke checks for
several scenarios whose primary tests live elsewhere. Release checks have
their own IDs because running a local test suite does not verify a release.

## Behavior scenarios

The suite column is the Cargo test target. In the `atx` target, the test name
includes its module path. Source links open the file containing that test.

| ID | What we check | Suite | Primary test |
| --- | --- | --- | --- |
| S1 | Direct argv preserves spaces, quotes, metacharacters and Unicode. | `acceptance_matrix` | [`s1_direct_argv_preserves_special_characters_and_unicode`](../tests/acceptance_matrix.rs) |
| S2 | Shell operators require explicit `--shell`. | `acceptance_matrix` | [`s2_shell_operators_require_explicit_shell_flag`](../tests/acceptance_matrix.rs) |
| S3 | Relative, local, UTC and IANA schedule forms resolve. | `acceptance_matrix` | [`s3_schedule_forms_resolve_across_clock_kinds`](../tests/acceptance_matrix.rs) |
| S4 | Explicit policy resolves DST folds; gaps are refused. | `acceptance_matrix` | [`s4_dst_policy_resolves_fold_and_gap_always_refuses`](../tests/acceptance_matrix.rs) |
| S5 | Dry-run creates no state or runtime files. | `acceptance_matrix` | [`s5_dry_run_creates_nothing`](../tests/acceptance_matrix.rs) |
| S6 | Submission reports persisted state and supervision separately. | `acceptance_matrix` | [`s6_submission_reports_persisted_and_supervised_separately`](../tests/acceptance_matrix.rs) |
| S7 | Success records timestamps, exit code and bounded captured logs. | `atx` | [`run_monitor::lifecycle::tests::monitor_runs_direct_child_and_persists_output_summary`](../src/run_monitor/lifecycle.rs) |
| S8 | Spawn failure, nonzero exit and signal termination stay distinct. | `atx` | [`run_monitor::lifecycle::tests::spawn_and_nonzero_exit_failures_become_terminal_runs`](../src/run_monitor/lifecycle.rs) |
| S9 | A missing executable records a known failure, not an unknown outcome. | `acceptance_matrix` | [`s9_missing_executable_fails_deterministically`](../tests/acceptance_matrix.rs) |
| S10 | A session job finishes after its submitting controlling terminal hangs up. | `cli_schedule` | [`session_job_survives_a_controlling_terminal_hangup`](../tests/cli_schedule.rs) |
| S11 | Output caps bound stored artifacts without blocking completion. | `acceptance_matrix` | [`s11_output_caps_bound_storage`](../tests/acceptance_matrix.rs) |
| S12 | Listing and showing jobs never reveals saved environment values. | `acceptance_matrix` | [`s12_environment_values_stay_redacted_in_reports`](../tests/acceptance_matrix.rs) |
| S13 | Cancellation reaches the command process group, including descendants. | `atx` | [`infrastructure::process::cancel::tests::term_then_kill_reaches_the_whole_command_group`](../src/infrastructure/process/cancel.rs) |
| S14 | TERM grace escalates to KILL for a TERM-immune command. | `cli_schedule` | [`cancel_grace_escalates_to_kill_for_term_immune_process`](../tests/cli_schedule.rs) |
| S15 | Dead or changed process identities are not signalled. | `atx` | [`infrastructure::process::cancel::tests::mismatched_and_dead_identities_are_never_signalled`](../src/infrastructure/process/cancel.rs) |
| S16 | Killing a live supervisor does not erase the monitor's eventual outcome. | `resilience` | [`supervisor_crash_preserves_the_live_monitors_outcome`](../tests/resilience.rs) |
| S17 | Monitor loss records Interrupted once, without retrying the job during later recovery. | `resilience` | [`killed_monitor_classifies_unknown_outcome_exactly_once`](../tests/resilience.rs) |
| S18 | A missed one-shot holds by default and does not execute late. | `acceptance_matrix` | [`s18_oneshot_missed_default_holds_without_late_execution`](../tests/acceptance_matrix.rs) |
| S19 | A recurring job skips elapsed occurrences without a catch-up burst. | `acceptance_matrix` | [`s19_recurring_missed_skips_elapsed_occurrences_without_burst`](../tests/acceptance_matrix.rs) |
| S20 | Concurrent updates leave legal job and run transitions. | `acceptance_matrix` | [`s20_concurrent_updates_preserve_legal_transitions`](../tests/acceptance_matrix.rs) |
| S21 | A newer database schema is refused before commands execute. | `acceptance_matrix` | [`s21_newer_schema_never_executes_commands`](../tests/acceptance_matrix.rs) |
| S22 | Private state directories reject links, unsafe modes and the wrong owner. | `atx` | [`infrastructure::paths::tests::private_directory_rejects_symlinks_modes_and_owners`](../src/infrastructure/paths/mod.rs) |
| S23 | Doctor reports concrete capability failures rather than an opaque error. | `acceptance_matrix` | [`s23_doctor_diagnoses_capability_problems`](../tests/acceptance_matrix.rs) |
| S24 | Retention includes every terminal job state and repeated cleanup removes nothing else. | `atx` | [`infrastructure::sqlite::retention::tests::cleanup_covers_every_terminal_job_state_and_is_idempotent`](../src/infrastructure/sqlite/retention.rs) |
| S25 | Oversized environment input is refused before execution. | `acceptance_matrix` | [`s25_inputs_are_bounded`](../tests/acceptance_matrix.rs) |
| S26 | Recovery applies a current revision atomically and remains idempotent. | `atx` | [`infrastructure::sqlite::reconcile::tests::missed_job_recovery_is_atomic_and_idempotent`](../src/infrastructure/sqlite/reconcile.rs) |

## Checks beyond the primary tests

- S9's working-directory checks are in
  [CLI validation](../src/cli/dispatch.rs) and
  [native spawning](../src/infrastructure/process/spawn.rs). Invalid directories
  are refused rather than queued with an unknown outcome. The
  [CLI scheduling tests](../tests/cli_schedule.rs) also check absolute and
  relative directory resolution and the saved canonical path.
- S21 also relies on the [SQLite schema checks](../src/infrastructure/sqlite/mod.rs)
  and [recovery validation](../src/infrastructure/sqlite/reconcile.rs) for corrupt
  records. A schema-version refusal alone does not prove those cases.
- S24's CLI retention and log-removal cases are in
  [effective configuration tests](../tests/config_effective.rs).
- S25 is one input-bound example. Other bounds are tested with the
  [execution specification](../src/domain/execution.rs),
  [output capture](../src/run_monitor/capture.rs),
  [SQLite management queries](../src/infrastructure/sqlite/management.rs), and
  [supervisor](../src/supervisor/daemon.rs). These cover stored arguments,
  output, result sets, wake processing and bounded retries. Workflow tooling
  has separate [parser and allocation checks](../xtask/src/workflows.rs).
- [Color tests](../tests/cli_color.rs) use separate stdout and stderr PTYs,
  environment overrides, explicit flags, JSON and quiet output.
- [Process-lifecycle tests](../tests/process_lifecycle.rs) check fixture
  ownership, cleanup and reaping. The
  [foreign-fixture cancellation regression](../tests/acceptance_matrix.rs)
  keeps another job's matching shell out of a cancellation completion probe.

## Running the tests

The normal edit loop runs these suites and the other workspace tests:

```console
./scripts/check.sh quick
```

To run a single primary test, use its suite and full name from the table:

```console
cargo test --locked --test cli_schedule session_job_survives_a_controlling_terminal_hangup -- --exact
cargo test --locked --bin atx \
  run_monitor::lifecycle::tests::monitor_runs_direct_child_and_persists_output_summary \
  -- --exact
```

Run the [pre-merge checks](../scripts/README.md) from a clean checkout before
integration. Tests use private temporary roots. Process tests must keep owned,
bounded cleanup; do not run the commands against your real ATX state directory.
Permission-sensitive cases also need an ordinary user. A root run does not
replace that evidence.

## Release checks

These checks need evidence for the release candidate itself. This table is
not a record that they have passed.

| ID | Requirement | Where it is checked |
| --- | --- | --- |
| R1 | Current-toolchain and actual Rust 1.85 MSRV gates pass. | [Shared check stages](../scripts/check.sh) and [CI](../.github/workflows/ci.yml) |
| R2 | All Tier 1 targets build and their native/user variants pass. | [Platform workflow](../.github/workflows/platform.yml) and [supported targets](platform-support.md) |
| R3 | Dependency, license, source and security checks pass. | [Dependency policy](../deny.toml), [security workflow](../.github/workflows/security.yml), and [CodeQL](../.github/workflows/codeql.yml) |
| R4 | Clean package dry-run and fresh extracted-bin tests pass. | [Package stage](../scripts/check.sh) and [CI package job](../.github/workflows/ci.yml) |
| R5 | Archives, checksums, SBOMs and attestations verify for the intended source. | [Release workflow](../.github/workflows/release.yml) and [release runbook](releasing.md) |
| R6 | Publication stays disabled until explicitly authorized and gated. | [Publish workflow](../.github/workflows/publish-crates-io.yml) and [release runbook](releasing.md) |

Scheduled [fuzz](../.github/workflows/fuzz.yml),
[mutation](../.github/workflows/mutation.yml), and
[performance](../.github/workflows/performance.yml) checks have their own
evidence. Local tests do not replace them or the native target matrix. See the
[readiness checklist](readiness.md) before treating ATX as release-ready.
