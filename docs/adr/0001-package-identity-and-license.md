# ADR-0001: Package Identity and License

- Status: Accepted
- Date: 2026-07-30

## Context

ATX needs a stable package identity before its manifest and public interfaces
are created. The binary name must remain `atx`. The repository owner selected
the MIT license and a public repository at `https://github.com/jsugg/atx`.

The crates.io index was queried on 2026-07-30. No package with the exact name
`atx` existed at that time. Availability remains non-binding until publication
and must be checked again immediately before release.

## Decision

- Use `atx` as both the Cargo package and executable name.
- Use Rust edition 2024.
- License the project under MIT using the SPDX identifier `MIT`.
- Publish source from `https://github.com/jsugg/atx` only after the documented
  repository publication approval.
- Select a distinct package name while retaining binary name `atx` if the
  registry name becomes unavailable before publication.

## Consequences

The manifest, package metadata, documentation, and dependency policy must
remain MIT-compatible. Package-name availability is a release preflight rather
than a reservation.
