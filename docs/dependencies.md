# Dependencies

ATX is small enough that every runtime dependency should earn its place.

Current choices:

- `serde` and `serde_json` keep persisted and command-line data formats typed.
- `uuid` creates sortable UUIDv7 identifiers.
- `jiff` handles checked date/time work and ships timezone data in the binary.
- `thiserror` keeps error types readable without hiding their causes.
- `proptest` is test-only and checks parser and state-machine invariants.

Before adding a crate, check:

1. Does it remove a real correctness or portability risk?
2. Does it support the project's MSRV?
3. Is it maintained and narrowly scoped?
4. Are its license and source allowed by `deny.toml`?
5. Is the feature set smaller than writing and maintaining the same behavior
   here?

Avoid wildcard versions, Git dependencies, default features that are not used,
and duplicate major versions. A duplicate is acceptable only when the journal
records why it cannot yet be removed.

Runtime code must not need a network service, an external date parser, or a
separately installed SQLite library.
