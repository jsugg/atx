#!/bin/sh

set -eu

ROOT=$(CDPATH="" cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-harness.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/cargo" <<'SCRIPT'
#!/bin/sh
case "$*" in
    'fmt --check') stage=cargo-fmt ;;
    'check --all-targets --all-features --locked') stage=cargo-check ;;
    'test --all-targets --all-features --locked') stage=cargo-test ;;
    'clippy --all-targets --all-features --locked -- -D warnings') stage=cargo-clippy ;;
    '+1.85.0 check --all-targets --all-features --locked') stage=cargo-msrv ;;
    'doc --no-deps --all-features') stage=cargo-doc ;;
    audit) stage=cargo-audit ;;
    'deny check') stage=cargo-deny ;;
    'package --list') stage=cargo-package ;;
    'publish --dry-run --locked') stage=cargo-publish ;;
    *)
        printf 'unexpected cargo command: %s\n' "$*" >&2
        exit 97
        ;;
esac

printf '%s\n' "$stage" >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = "$stage" ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
if [ "$stage" = cargo-package ]; then
    cat "$ATX_PACKAGE_LIST"
fi
SCRIPT
chmod 0700 "$tmp/cargo"

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

stage=$name
if [ "${1:-}" = --version ]; then
    stage=$name-version
fi
printf '%s\n' "$stage" >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = "$stage" ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
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

assert_failure() {
    mode=$1
    stage=$2
    expected_status=$3

    : >"$ATX_FAKE_LOG"
    status=0
    ATX_FAKE_FAIL_STAGE="$stage" ATX_FAKE_FAIL_STATUS="$expected_status" \
        PATH="$tmp:$PATH" scripts/check.sh "$mode" >/dev/null 2>&1 || status=$?

    if [ "$status" -ne "$expected_status" ]; then
        printf '%s did not preserve %s failure status: expected %s, got %s\n' \
            "$mode" "$stage" "$expected_status" "$status" >&2
        exit 1
    fi

    last_stage=$(tail -n 1 "$ATX_FAKE_LOG")
    if [ "$last_stage" != "$stage" ]; then
        printf '%s continued after %s failure; last stage was %s\n' \
            "$mode" "$stage" "$last_stage" >&2
        exit 1
    fi
}

assert_package_policy_failure() {
    mode=$1
    bad_package_list="$tmp/package-bad-$mode"
    cp "$ATX_PACKAGE_LIST" "$bad_package_list"
    printf '%s\n' '.local/plan.md' >>"$bad_package_list"

    : >"$ATX_FAKE_LOG"
    status=0
    ATX_PACKAGE_LIST="$bad_package_list" PATH="$tmp:$PATH" \
        scripts/check.sh "$mode" >/dev/null 2>&1 || status=$?

    if [ "$status" -ne 1 ]; then
        printf '%s did not preserve package-policy failure status: %s\n' \
            "$mode" "$status" >&2
        exit 1
    fi
    if grep -qx cargo-publish "$ATX_FAKE_LOG"; then
        printf '%s continued after package-policy failure\n' "$mode" >&2
        exit 1
    fi
}

for stage in cargo-fmt cargo-check cargo-test cargo-clippy; do
    assert_failure quick "$stage" 41
    assert_failure full "$stage" 41
done

for stage in \
    cargo-msrv cargo-doc \
    cargo-audit-version cargo-deny-version mdlint-version lychee-version \
    cargo-audit cargo-deny mdlint lychee cargo-package cargo-publish
do
    assert_failure static "$stage" 41
    assert_failure full "$stage" 41
done

assert_package_policy_failure static
assert_package_policy_failure full

for mode in quick static full; do
    : >"$ATX_FAKE_LOG"
    PATH="$tmp:$PATH" scripts/check.sh "$mode"
done
scripts/check-package-list.sh "$ATX_PACKAGE_LIST"

printf 'quality-harness-tests: PASS\n'
