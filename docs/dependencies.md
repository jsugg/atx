# Dependencies

ATX is small enough that every runtime dependency should earn its place.

Current choices:

- `serde` and `serde_json` keep persisted and command-line data formats typed.
- `uuid` creates sortable UUIDv7 identifiers.
- `jiff` handles checked date/time work and ships timezone data in the binary.
- `toml` reads the small config file without a home-grown parser.
- `rustix` gives the filesystem and process code safe Unix system calls.
- `libc` is limited to the small macOS clock and process shims that Rust's
  standard library does not expose.
- `rusqlite` ships SQLite with the binary and keeps transactions typed.
- `getrandom` creates run-claim tokens straight from the operating system.
- `thiserror` keeps error types readable without hiding their causes.
- `proptest` and `tempfile` are test-only. They cover generated invariants and
  filesystem behavior without touching a real ATX directory.

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
