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

## Binary archives

Tagging a commit with a `v*` tag runs the release workflow: it checks the tag
against the crate version, builds macOS arm64 and Linux x86_64/aarch64 musl
binaries, packs each into a reproducible `.tar.gz` (fixed mtime, root
ownership, so rebuilding the same commit gives identical bytes), and attaches
them to a draft GitHub release together with a `SHA256SUMS` file and a
CycloneDX SBOM for every archive. Publishing the draft is a manual click.

The workflow also signs build-provenance attestations for each archive and
SBOM, so anyone can check that a given file really came from this
repository's release workflow:

    gh attestation verify atx-v0.1.0-x86_64-unknown-linux-musl.tar.gz -R jsugg/atx

And check the checksums against the manifest:

    sha256sum --check --ignore-missing SHA256SUMS

You can rehearse without cutting a tag: run the workflow by hand from the
Actions tab and give it the tag name you have in mind. It builds everything,
attests it, verifies the attestations, but publishes nothing.

Crate publishing to crates.io stays gated behind the separate approval above.

The exact commands and rollback notes will be filled in before 1.0.
