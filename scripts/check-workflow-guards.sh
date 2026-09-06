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

cargo xtask check-workflows || exit $?
require_pattern scripts/check-markdown.sh 'markdownlint-cli2@0\.23\.2'
require_literal scripts/check-hygiene.sh 'shellcheck scripts/*.sh || exit $?'
require_literal scripts/check-hygiene.sh 'scripts/test-check-harness.sh || exit $?'
require_literal .github/ISSUE_TEMPLATE/bug_report.md 'atx doctor --json'
require_literal .github/ISSUE_TEMPLATE/bug_report.md 'Remove credentials, environment values and private command output.'
require_literal .github/ISSUE_TEMPLATE/bug_report.md "Redact paths or user names you don't want in a public issue."
require_literal .github/ISSUE_TEMPLATE/bug_report.md "Include your OS version and \`atx version\` output too; doctor does not report them."

printf 'workflow-guards: PASS\n'
