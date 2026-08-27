#!/bin/sh

set -eu

ROOT=$(CDPATH="" cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-harness.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/cargo" <<'SCRIPT'
#!/bin/sh
printf '%s\n' "$*" >>"$ATX_FAKE_LOG"
saw_check=0
saw_package=0
saw_list=0
saw_publish=0
saw_dry_run=0
for argument in "$@"; do
    case "$argument" in
        check) saw_check=1 ;;
        package) saw_package=1 ;;
        --list) saw_list=1 ;;
        publish) saw_publish=1 ;;
        --dry-run) saw_dry_run=1 ;;
    esac
done
if [ "${ATX_FAKE_FAIL_CHECK:-0}" -eq 1 ] && [ "$saw_check" -eq 1 ]; then
    exit 23
fi
if [ "$saw_package" -eq 1 ] && [ "$saw_list" -eq 1 ]; then
    cat "$ATX_PACKAGE_LIST"
fi
if [ "$saw_publish" -eq 1 ] && [ "$saw_dry_run" -ne 1 ]; then
    printf 'live cargo publish attempted\n' >&2
    exit 97
fi
exit 0
SCRIPT
chmod 0700 "$tmp/cargo"

export ATX_FAKE_LOG="$tmp/calls"
export ATX_PACKAGE_LIST="$tmp/package-good"
cat >"$ATX_PACKAGE_LIST" <<'LIST'
.cargo_vcs_info.json
CHANGELOG.md
Cargo.lock
Cargo.toml
Cargo.toml.orig
LICENSE-MIT
README.md
src/main.rs
LIST

status=0
ATX_FAKE_FAIL_CHECK=1 PATH="$tmp:$PATH" scripts/check.sh quick || status=$?

if [ "$status" -ne 23 ]; then
    printf 'quick check did not preserve the failing command status: %s\n' "$status" >&2
    exit 1
fi

if ! tail -n 1 "$ATX_FAKE_LOG" | grep -Eq '(^|[[:space:]])check($|[[:space:]])'; then
    printf 'quick check continued after failure\n' >&2
    exit 1
fi

cat >"$tmp/versioned-tool" <<'SCRIPT'
#!/bin/sh
name=${0##*/}
case "$name" in
    cargo-audit) version=0.22.2 ;;
    cargo-deny) version=0.20.2 ;;
    mdlint) version=0.3.22 ;;
    lychee) version=0.24.2 ;;
    *) exit 97 ;;
esac
if [ "${1:-}" = --version ]; then
    printf '%s %s\n' "$name" "$version"
fi
SCRIPT
chmod 0700 "$tmp/versioned-tool"
for tool in cargo-audit cargo-deny mdlint lychee; do
    ln -s versioned-tool "$tmp/$tool"
done

cat >"$tmp/remote-command" <<'SCRIPT'
#!/bin/sh
printf 'quality harness attempted remote command: %s\n' "${0##*/}" >&2
exit 97
SCRIPT
chmod 0700 "$tmp/remote-command"
ln -s remote-command "$tmp/git"
ln -s remote-command "$tmp/gh"

: >"$ATX_FAKE_LOG"
PATH="$tmp:$PATH" scripts/check.sh static
scripts/check-package-list.sh "$ATX_PACKAGE_LIST"

cp "$ATX_PACKAGE_LIST" "$tmp/package-bad"
printf '%s\n' '.local/plan.md' >>"$tmp/package-bad"
if scripts/check-package-list.sh "$tmp/package-bad" >/dev/null 2>&1; then
    printf 'package policy accepted an internal file\n' >&2
    exit 1
fi

printf 'quality-harness-tests: PASS\n'
