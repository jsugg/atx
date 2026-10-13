#!/usr/bin/env bash

set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

mode=${1:-offline}
case "$mode" in
    offline|online) ;;
    *) printf 'usage: %s offline|online\n' "$0" >&2; exit 2 ;;
esac

paths=$(mktemp "${TMPDIR:-/tmp}/atx-links.XXXXXX")
trap 'rm -f "$paths"' EXIT
git ls-files -z -- '*.md' > "$paths" || exit $?
files=()
while IFS= read -r -d '' name; do
    pattern=$(printf '%s' "$name" | sed 's/[][?*]/[&]/g') || exit $?
    files+=("$pattern")
done < "$paths"
if [ "${#files[@]}" -eq 0 ]; then printf 'no tracked Markdown files to check\n' >&2; exit 1; fi
version=$(lychee --version) || exit $?
case "$version" in
    'lychee 0.24.2') ;;
    *) printf 'expected lychee 0.24.2, got %s\n' "$version" >&2; exit 2 ;;
esac
if [ "$mode" = offline ]; then
    lychee --offline --no-progress -- "${files[@]}" || exit $?
else
    lychee --no-progress --timeout 20 --max-retries 2 --max-concurrency 8 \
        -- "${files[@]}" || exit $?
fi
