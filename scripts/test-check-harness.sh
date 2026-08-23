#!/bin/sh

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-harness.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/cargo" <<'SCRIPT'
#!/bin/sh
printf '%s\n' "$*" >>"$ATX_FAKE_LOG"
case "$1" in
    check) exit 23 ;;
esac
exit 0
SCRIPT
chmod 0700 "$tmp/cargo"

export ATX_FAKE_LOG="$tmp/calls"
status=0
PATH="$tmp:$PATH" scripts/check.sh quick || status=$?

if [ "$status" -ne 23 ]; then
    printf 'quick check did not preserve the failing command status: %s\n' "$status" >&2
    exit 1
fi

if [ "$(wc -l <"$ATX_FAKE_LOG" | tr -d ' ')" -ne 2 ]; then
    printf 'quick check continued after failure\n' >&2
    exit 1
fi

if grep -En 'git[[:space:]]+push|gh[[:space:]]+(api|pr|release|repo|workflow)' \
    scripts/check.sh >/dev/null; then
    printf 'quality harness contains a remote mutation command\n' >&2
    exit 1
fi

if grep -E 'cargo[[:space:]]+publish' scripts/check.sh |
    grep -Ev -- '--dry-run' >/dev/null; then
    printf 'quality harness contains a live publish command\n' >&2
    exit 1
fi

cat >"$tmp/package-good" <<'LIST'
.cargo_vcs_info.json
CHANGELOG.md
Cargo.lock
Cargo.toml
Cargo.toml.orig
LICENSE-MIT
README.md
src/main.rs
LIST
scripts/check-package-list.sh "$tmp/package-good"

cp "$tmp/package-good" "$tmp/package-bad"
printf '%s\n' '.local/plan.md' >>"$tmp/package-bad"
if scripts/check-package-list.sh "$tmp/package-bad" >/dev/null 2>&1; then
    printf 'package policy accepted an internal file\n' >&2
    exit 1
fi

printf 'quality-harness-tests: PASS\n'
