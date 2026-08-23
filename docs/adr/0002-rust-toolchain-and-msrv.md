# ADR-0002: Rust Toolchain and MSRV

- Status: Accepted
- Date: 2026-07-30

## Context

ATX uses Rust edition 2024 and must publish reproducible builds while avoiding
unnecessary compiler churn for downstream package maintainers.

## Decision

- Pin the development and release toolchain to Rust 1.97.0.
- Declare Rust 1.85 as the minimum supported Rust version (MSRV), the first
  stable release supporting edition 2024.
- Test both pinned stable and MSRV locally and in the future platform matrix.
- Permit dependencies only when their resolved versions support the declared
  MSRV.

## Upgrade Policy

The pinned stable toolchain may advance after formatting, lint, test, package,
and performance gates pass. A 1.x MSRV increase requires a changelog notice and
advance release notice unless required by a security correction.

## Consequences

Newer language features may be used only when available on Rust 1.85. Dependency
updates that raise MSRV are rejected or deferred.
