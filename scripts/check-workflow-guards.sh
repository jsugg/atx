#!/bin/sh

set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

require_literal() {
    file=$1
    text=$2

    grep -Fqx -- "$text" "$file" >/dev/null || {
        printf 'missing workflow guard in %s: %s\n' "$file" "$text" >&2
        exit 1
    }
}

require_pattern() {
    file=$1
    pattern=$2

    grep -Eq -- "$pattern" "$file" || {
        printf 'missing workflow guard in %s: /%s/\n' "$file" "$pattern" >&2
        exit 1
    }
}

for workflow in .github/workflows/*.yml; do
    actionlint "$workflow"
done

require_pattern .github/workflows/ci.yml 'markdownlint-cli2@[0-9]+\.[0-9]+\.[0-9]+'
require_literal .github/workflows/ci.yml '          tool: actionlint@1.7.12'
require_literal .github/workflows/ci.yml '          scripts/check-workflow-guards.sh'
require_literal .github/workflows/ci.yml '          toolchain: nightly-2026-08-01'
require_pattern .github/workflows/ci.yml '^[[:space:]]+os: \[ubuntu-24\.04, macos-15\]$'
require_literal .github/workflows/ci.yml '      LLVM_PROFILE_FILE_NAME: atx-%p-%m.profraw'
require_literal .github/workflows/ci.yml '      COVERAGE_LINE_FLOOR: 95'
require_literal .github/workflows/ci.yml '      COVERAGE_REGION_FLOOR: 85'
require_literal .github/workflows/ci.yml '        run: rm -rf target/llvm-cov-target'
require_literal .github/workflows/ci.yml '      - name: Generate and lint man pages'
require_literal .github/workflows/ci.yml '        run: sudo apt-get update && sudo apt-get install -y mandoc shellcheck'
require_literal .github/workflows/ci.yml '          cargo run --locked -p xtask -- dist-man'
require_literal .github/workflows/ci.yml '          mandoc -T lint dist/man/*.1'
require_pattern .github/workflows/ci.yml 'base="\$\{\{ github\.event\.pull_request\.base\.sha \|\| github\.event\.before \}\}"'
require_pattern .github/workflows/ci.yml '0000000000000000000000000000000000000000'

require_pattern .github/workflows/codeql.yml '^[[:space:]]+os: \[ubuntu-24\.04, macos-15\]$'

require_literal .github/workflows/mutation.yml '    timeout-minutes: 55'
require_pattern .github/workflows/mutation.yml 'timeout 45m cargo mutants --no-shuffle --timeout-multiplier 4'
require_literal .github/workflows/mutation.yml "            --file src/domain/duration.rs \\"
require_literal .github/workflows/mutation.yml "            --file src/domain/calendar_syntax.rs \\"
require_literal .github/workflows/mutation.yml "            --file src/domain/recurrence.rs \\"
require_literal .github/workflows/mutation.yml "            --file src/domain/transition.rs \\"
require_pattern .github/workflows/mutation.yml "--[[:space:]]+--bin atx 'domain::'\$"
require_literal .github/workflows/mutation.yml "          if [ \"\$status\" -eq 124 ]; then"
require_pattern .github/workflows/mutation.yml 'cargo mutants --version'
require_pattern .github/workflows/mutation.yml 'tar czf mutants-report\.tgz mutants\.out'

require_literal .github/workflows/platform.yml 'name: platform'
require_pattern .github/workflows/platform.yml '^  linux-root:$'
require_literal .github/workflows/platform.yml '    container: rust:1.97-bookworm'
if grep -Eq '^    paths:$' .github/workflows/platform.yml; then
    printf 'platform workflow must not path-filter a required check\n' >&2
    exit 1
fi

require_pattern .github/workflows/release.yml 'DRY_RUN: \$\{\{ github\.event_name == '\''workflow_dispatch'\'' && '\''true'\'' \|\| '\''false'\'' \}\}'
require_pattern .github/workflows/release.yml "if: env\.DRY_RUN == 'false'"
require_pattern .github/workflows/release.yml 'sha256sum -- \*\.tar\.gz \*\.cdx\.json > SHA256SUMS'
require_pattern .github/workflows/release.yml 'dist/SHA256SUMS'
require_pattern .github/workflows/release.yml 'for f in dist/\*\.tar\.gz dist/\*\.cdx\.json dist/SHA256SUMS; do'
require_pattern .github/workflows/release.yml 'name:[[:space:]]+release-checksums'
require_pattern .github/workflows/release.yml 'path:[[:space:]]+dist/SHA256SUMS'
require_pattern .github/workflows/release.yml 'if-no-files-found:[[:space:]]+error'
require_pattern .github/workflows/release.yml 'fail_on_unmatched_files:[[:space:]]+true'
require_pattern .github/workflows/release.yml 'touch -t 197001010000 LICENSE-MIT README\.md CHANGELOG\.md'
require_pattern .github/workflows/release.yml '-c atx LICENSE-MIT README\.md CHANGELOG\.md man-pages'

require_literal .github/workflows/publish-crates-io.yml "          head=\"\$(git rev-parse HEAD)\""
require_literal .github/workflows/publish-crates-io.yml "          tagged=\"\$(git rev-parse \"\$TAG_REF^{commit}\")\""
if grep -Fq -- '--no-verify' .github/workflows/publish-crates-io.yml; then
    printf 'real publishing checks must not skip package verification\n' >&2
    exit 1
fi

require_literal .gitignore '.smoke-*'
require_literal .gitignore '.cargo/'
if grep -Fqx '    "Zlib",' deny.toml; then
    printf 'deny.toml retains an unused Zlib license allowance\n' >&2
    exit 1
fi

printf 'workflow-guards: PASS\n'
