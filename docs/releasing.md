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

## Publishing the crate

Publishing to crates.io is wired but switched off. Every pull request
rehearses the whole publish flow against a local registry mock with dummy
credentials — upload, checksum, duplicate rejection, failure handling — so
the machinery is exercised without anything ever reaching crates.io.

The real publish job only runs when a GitHub release is published *and* the
repository variable `CRATES_IO_PUBLISH_ENABLED` is set to `true`, inside
the protected `crates-io` environment. It re-checks that the release tag,
`Cargo.toml`, and `CHANGELOG.md` all agree, refuses to republish an
existing version, then asks crates.io for a short-lived trusted-publishing
credential (OIDC) instead of using a stored token. No crates.io token
lives in this repository.

Turning publishing on is an owner action, done once:

1. Publish the first version manually (see below) — crates.io needs an
   existing crate before its trusted publisher can be configured.
2. On crates.io, configure the trusted publisher: repository `jsugg/atx`,
   workflow `publish-crates-io.yml`, environment `crates-io`.
3. Set the repository variable `CRATES_IO_PUBLISH_ENABLED=true`.
4. The next tagged release publishes through OIDC automatically.

Until step 3 happens, the real job stays skipped and requests no
credential.

Crate publishing to crates.io also stays gated behind the separate
approval above.

The exact rollback and yank notes will be filled in before 1.0.
