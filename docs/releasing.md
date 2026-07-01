# Releasing

There is no public release yet.

Release work is deliberately split:

1. Finish and verify the crate locally.
2. Publish the repository only after the owner's exact repository approval.
3. Wire and test release jobs without publishing a crate.
4. Publish to crates.io only after the separate crate approval.

Never use `--allow-dirty` or `--no-verify` for a release. The full local check,
package file-list check, unpacked-package build, and
`cargo publish --dry-run --locked` must pass first.

The exact commands and rollback notes will be filled in before 1.0.
