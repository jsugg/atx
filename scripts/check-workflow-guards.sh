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

require_pattern .github/workflows/ci.yml 'markdownlint-cli2@[0-9]+\.[0-9]+\.[0-9]+'
require_pattern .github/workflows/ci.yml 'tool:[[:space:]]+actionlint@[0-9]+\.[0-9]+\.[0-9]+'
require_literal .github/workflows/ci.yml '          scripts/check-workflow-guards.sh'
require_pattern .github/workflows/ci.yml 'toolchain:[[:space:]]+nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}'
require_pattern .github/workflows/ci.yml 'ubuntu-[0-9]'
require_pattern .github/workflows/ci.yml 'macos-[0-9]'
require_literal .github/workflows/ci.yml '      COVERAGE_LINE_FLOOR: 95'
require_literal .github/workflows/ci.yml '      COVERAGE_REGION_FLOOR: 85'

require_pattern .github/workflows/codeql.yml 'ubuntu-[0-9]'
require_pattern .github/workflows/codeql.yml 'macos-[0-9]'

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
require_pattern .github/workflows/release.yml "if: github\.event_name != 'workflow_dispatch'"
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

printf 'workflow-guards: PASS\n'
