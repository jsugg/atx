#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

paths=$(mktemp "${TMPDIR:-/tmp}/atx-markdown.XXXXXX")
trap 'rm -f "$paths"' EXIT
git ls-files -z -- '*.md' > "$paths" || exit $?

files=()
while IFS= read -r -d '' name; do
    files+=(":$name")
done < "$paths"
if [ "${#files[@]}" -eq 0 ]; then
    printf 'no tracked Markdown files to lint\n' >&2
    exit 1
fi

npx --yes markdownlint-cli2@0.23.2 --config .markdownlint-cli2.jsonc \
    --no-globs "${files[@]}" || exit $?
