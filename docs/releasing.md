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

```console
gh attestation verify atx-v0.1.0-x86_64-unknown-linux-musl.tar.gz -R jsugg/atx
```

And check the checksums against the manifest:

```console
sha256sum --check --ignore-missing SHA256SUMS
```

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

## Release process, step by step

1. Land changes on `main` (PRs only — the `protect-main` ruleset blocks
   direct pushes).
2. Move items from `## Unreleased` to a new `## [X.Y.Z] - YYYY-MM-DD`
   section in `CHANGELOG.md`, and set `version = "X.Y.Z"` in `Cargo.toml`.
3. Open that as a PR; once it is green and merged, tag the merge commit:
   `git tag -a vX.Y.Z <commit> && git push origin vX.Y.Z` (sign the tag
   too if you have a signing key set up).
4. The release workflow builds, attests, and verifies everything, then
   attaches it to a **draft** GitHub release. Review the artifacts,
   checksums, and attestations, then publish the release.
5. If crate publishing is switched on (`CRATES_IO_PUBLISH_ENABLED=true`),
   publishing the release also triggers the crates.io job: it re-checks
   tag/manifest/changelog agreement and refuses to republish.

Before the first public version there is one extra bootstrap, done once:

- Check `atx` is free on crates.io.
- After the exact publish approval, run the first
  `cargo publish --locked` by hand with a short-lived credential.
- On crates.io, configure the trusted publisher (repository `jsugg/atx`,
  workflow `publish-crates-io.yml`, environment `crates-io`), then revoke
  the bootstrap credential.
- Set the repository variable `CRATES_IO_PUBLISH_ENABLED=true`. From the
  next tagged release on, publishing goes through OIDC with no stored
  token.

Repository protection worth knowing about: `main` is covered by the
`protect-main` ruleset (PRs required, no force-push, no deletion), and
the `release` and `crates-io` GitHub environments gate anything that
touches published artifacts or the registry.

## Rolling back

Published binary releases are immutable: a tag points at a commit, and
artifacts are checksummed and attested. There is no "fixing" a bad
release — you cut a new one. If a release is broken:

1. Yank nothing yet; first decide how bad it is (see criteria below).
2. Fix forward: cut `vX.Y+1` with the fix, following the normal process.
3. If the draft release has not been published yet, just fix and re-run;
   drafts can be deleted freely.
4. Point any announcements at the fixed release.

A crate version on crates.io can be **yanked**, which stops new projects
from picking it up without breaking existing lockfiles:
`cargo yank --version X.Y.Z` (or undo with `--undo`). Yank when a version
builds fine but is wrong — a logic bug, a regression, a bad dependency
floor. Do not yank for a security problem alone; see below. Yank
sparingly: `cargo update` will not pick a yanked version back after a
project leaves it.

If a release contains something genuinely dangerous (leaked secret,
malicious code, data-loss bug):

1. Yank the affected crate versions immediately.
2. Replace leaked secrets everywhere they were used, not just where they
   were published.
3. Publish a fixed release and a changelog entry saying what happened.
4. Delete the broken binary assets from the GitHub release and note the
   reason on the release page, so nobody downloads them by accident.
5. Write down what went wrong in the issue tracker while it is fresh.

Tags are never moved or deleted once public; history stays honest even
when releases do not.
