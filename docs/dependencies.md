# Dependencies

ATX is small enough that every runtime dependency should earn its place.

Current choices:

- `clap` owns the public option grammar and keeps help text in sync with it.
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
and duplicate major versions. Document any unavoidable duplicate in this page,
including why it cannot yet be removed.

`deny.toml` checks advisories, licenses and sources. Yanked versions, wildcard
requirements and unknown registries or Git sources fail the gate. The allowed
licenses are Apache-2.0, MIT, Unicode-3.0 and Unlicense. Multiple versions produce
a warning so transitive duplicates can be reviewed rather than hidden. Only
crates.io is an allowed registry; runtime code needs no registry connection.

Runtime code must not need a network service, an external date parser, or a
separately installed SQLite library.

The development-only `xtask` reuses `serde_json` to read Cargo's build messages
and run the executable Cargo actually produced, including custom target paths.
It also uses pinned `yaml-rust2` 0.13.0 to check workflow policy values without
depending on indentation or a global YAML interpreter. UTF-8 input, nesting and
alias expansion are bounded, including the copied string bytes. This parser is
not shipped with ATX.
