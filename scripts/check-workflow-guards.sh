#!/bin/sh

set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

require_literal() {
    file=$1
    text=$2

    grep -Ev '^[[:space:]]*#' "$file" | grep -Fqx -- "$text" >/dev/null || {
        printf 'missing workflow guard in %s: %s\n' "$file" "$text" >&2
        exit 1
    }
}

require_pattern() {
    file=$1
    pattern=$2

    grep -Ev '^[[:space:]]*#' "$file" | grep -Eq -- "$pattern" || {
        printf 'missing workflow guard in %s: /%s/\n' "$file" "$pattern" >&2
        exit 1
    }
}

for workflow in .github/workflows/*.yml; do
    actionlint "$workflow"
done

require_literal .github/workflows/ci.yml '          scripts/check.sh markdown'
require_literal .github/workflows/ci.yml '          scripts/test-markdown-harness.sh --real'
require_pattern scripts/check-markdown.sh 'markdownlint-cli2@0\.23\.2'
require_pattern .github/workflows/ci.yml 'ACTIONLINT_VERSION:[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+'
require_pattern .github/workflows/ci.yml 'rhysd/actionlint/releases/download/'
require_pattern .github/workflows/ci.yml 'sha256sum --check'
require_literal .github/workflows/ci.yml '          scripts/check-workflow-guards.sh'
require_pattern .github/workflows/ci.yml 'toolchain:[[:space:]]+nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}'
require_pattern .github/workflows/ci.yml 'ubuntu-[0-9]'
require_pattern .github/workflows/ci.yml 'macos-[0-9]'
require_literal .github/workflows/ci.yml '      COVERAGE_LINE_FLOOR: 95'
require_literal .github/workflows/ci.yml '      COVERAGE_REGION_FLOOR: 85'

require_pattern .github/workflows/codeql.yml 'ubuntu-[0-9]'
require_pattern .github/workflows/codeql.yml 'macos-[0-9]'
require_literal .github/workflows/codeql.yml '      - "xtask/**"'

require_pattern .github/workflows/mutation.yml 'timeout-minutes:'
require_pattern .github/workflows/mutation.yml 'cargo mutants'
require_pattern .github/workflows/mutation.yml 'mutants\.out'
require_pattern .github/workflows/mutation.yml 'actions/upload-artifact@[0-9a-f]{40}'

require_literal .github/workflows/platform.yml 'name: platform'
require_pattern .github/workflows/platform.yml '^[[:space:]]+container:'
require_pattern .github/workflows/platform.yml 'cargo test --all-targets --all-features --locked'
if grep -Eq '^    paths:$' .github/workflows/platform.yml; then
    printf 'platform workflow must not path-filter a required check\n' >&2
    exit 1
fi

require_pattern .github/workflows/release.yml 'DRY_RUN:'
require_pattern .github/workflows/release.yml "if: env\.DRY_RUN == 'false'"
require_pattern .github/workflows/release.yml "if: github\.event_name == 'push'"
require_pattern .github/workflows/release.yml 'cargo install cargo-auditable --version 0\.7\.7 --locked'
require_pattern .github/workflows/release.yml 'cargo auditable build --release --locked --target'
require_pattern .github/workflows/release.yml 'syft-version: v1\.42\.3'
require_pattern .github/workflows/release.yml 'cargo xtask check-sbom'
require_pattern .github/workflows/release.yml 'upload-release-assets: false'
require_pattern .github/workflows/release.yml 'sha256sum .*\.tar\.gz .*\.cdx\.json .*SHA256SUMS'
require_pattern .github/workflows/release.yml 'dist/SHA256SUMS'
require_pattern .github/workflows/release.yml 'gh attestation verify'
require_pattern .github/workflows/release.yml 'path:[[:space:]]+dist/SHA256SUMS'
require_pattern .github/workflows/release.yml 'if-no-files-found:[[:space:]]+error'
require_pattern .github/workflows/release.yml 'fail_on_unmatched_files:[[:space:]]+true'

require_pattern .github/workflows/publish-crates-io.yml 'git rev-parse HEAD'
require_pattern .github/workflows/publish-crates-io.yml "git rev-parse \"\\\$TAG_REF\\^\\{commit\\}\""
if grep -Fq -- '--no-verify' .github/workflows/publish-crates-io.yml; then
    printf 'real publishing checks must not skip package verification\n' >&2
    exit 1
fi

require_literal .github/ISSUE_TEMPLATE/config.yml 'blank_issues_enabled: true'
require_literal .github/ISSUE_TEMPLATE/config.yml '    url: https://github.com/jsugg/atx/security/advisories/new'
require_literal .github/ISSUE_TEMPLATE/bug_report.md 'atx doctor --json'
require_literal .github/ISSUE_TEMPLATE/bug_report.md 'Remove credentials, environment values and private command output.'
require_literal .github/ISSUE_TEMPLATE/bug_report.md "Redact paths or user names you don't want in a public issue."
require_literal .github/ISSUE_TEMPLATE/bug_report.md "Include your OS version and \`atx version\` output too; doctor does not report them."

printf 'workflow-guards: PASS\n'
