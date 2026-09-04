#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-markdown-harness.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/docs" "$fixture/.local" "$fixture/.github/ISSUE_TEMPLATE" "$tmp/bin"
cp "$ROOT/scripts/check-markdown.sh" "$fixture/scripts/"
cp "$ROOT/.markdownlint-cli2.jsonc" "$fixture/"
git init -q "$fixture"
printf '# Root\n' > "$fixture/README.md"
printf '# Spaces and glob characters\n' > "$fixture/docs/a [literal]* file.md"
printf 'untracked invalid markdown\n' > "$fixture/orphan.md"
printf 'ignored invalid markdown\n' > "$fixture/.local/private.md"
printf 'excluded template without a heading' > "$fixture/.github/ISSUE_TEMPLATE/bug_report.md"
git -C "$fixture" add README.md 'docs/a [literal]* file.md' .github/ISSUE_TEMPLATE/bug_report.md

cat > "$tmp/bin/npx" <<'SCRIPT'
#!/usr/bin/env bash
printf '%s\0' "$@" > "$ATX_MARKDOWN_ARGS"
exit "${ATX_MARKDOWN_STATUS:-0}"
SCRIPT
chmod 0700 "$tmp/bin/npx"
export ATX_MARKDOWN_ARGS="$tmp/arguments"

PATH="$tmp/bin:$PATH" "$fixture/scripts/check-markdown.sh"
expected=(--yes markdownlint-cli2@0.23.2 --config .markdownlint-cli2.jsonc --no-globs ':.github/ISSUE_TEMPLATE/bug_report.md' ':README.md' ':docs/a [literal]* file.md')
printf '%s\0' "${expected[@]}" > "$tmp/expected"
cmp "$tmp/expected" "$ATX_MARKDOWN_ARGS"

status=0
ATX_MARKDOWN_STATUS=41 PATH="$tmp/bin:$PATH" "$fixture/scripts/check-markdown.sh" || status=$?
test "$status" -eq 41

cat > "$tmp/bin/git" <<'SCRIPT'
#!/usr/bin/env bash
exit 73
SCRIPT
chmod 0700 "$tmp/bin/git"
rm "$ATX_MARKDOWN_ARGS"
status=0
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-markdown.sh" || status=$?
test "$status" -eq 73
test ! -e "$ATX_MARKDOWN_ARGS"
rm "$tmp/bin/git"

git -C "$fixture" rm --cached -q README.md 'docs/a [literal]* file.md' .github/ISSUE_TEMPLATE/bug_report.md
status=0
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-markdown.sh" || status=$?
test "$status" -eq 1
test ! -e "$ATX_MARKDOWN_ARGS"

if [ "${1:-}" = --real ]; then
    git -C "$fixture" add README.md 'docs/a [literal]* file.md' .github/ISSUE_TEMPLATE/bug_report.md
    "$fixture/scripts/check-markdown.sh"
    printf 'no heading and no final newline' > "$fixture/docs/a [literal]* file.md"
    status=0
    "$fixture/scripts/check-markdown.sh" > "$tmp/invalid.log" 2>&1 || status=$?
    test "$status" -eq 1
    grep -q 'a \[literal\]\* file.md' "$tmp/invalid.log"
fi

printf 'markdown-harness: PASS\n'
