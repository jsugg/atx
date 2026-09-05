#!/bin/sh

set -eu

ROOT=$(CDPATH="" cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-harness.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

fixture="$tmp/project"
mkdir -p "$fixture/scripts"
cp scripts/check.sh scripts/check-markdown.sh scripts/check-package-list.sh "$fixture/scripts/"
cp scripts/check-links.sh "$fixture/scripts/"
CHECK="$fixture/scripts/check.sh"
for helper in hygiene man; do
    cat >"$fixture/scripts/check-$helper.sh" <<'SCRIPT'
#!/bin/sh
name=${0##*/}
stage=${name#check-}
stage=${stage%.sh}
printf '%s\n' "$stage" >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = "$stage" ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
SCRIPT
    chmod 0700 "$fixture/scripts/check-$helper.sh"
done

cat >"$tmp/cargo" <<'SCRIPT'
#!/bin/sh
case "$*" in
    'fmt --check') stage=cargo-fmt ;;
    'check --workspace --all-targets --all-features --locked') stage=cargo-check ;;
    'test --workspace --all-targets --all-features --locked') stage=cargo-test ;;
    'clippy --workspace --all-targets --all-features --locked -- -D warnings') stage=cargo-clippy ;;
    '+1.85.0 check --workspace --all-targets --all-features --locked') stage=cargo-msrv ;;
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
if [ "$stage" = cargo-msrv ]; then
    if [ "${RUSTC:-}" != "$ATX_FAKE_COMPILER_PATH" ] \
        || [ -n "${RUSTC_WRAPPER:-}" ] || [ -n "${RUSTC_WORKSPACE_WRAPPER:-}" ]; then
        printf 'msrv did not bind Cargo to the verified compiler\n' >&2
        exit 97
    fi
fi
if [ "$stage" = cargo-package ]; then
    cat "$ATX_PACKAGE_LIST"
fi
SCRIPT
chmod 0700 "$tmp/cargo"

cat >"$tmp/rustc" <<'SCRIPT'
#!/bin/sh
if [ "$*" != '--version' ]; then
    printf 'unexpected rustc command: %s\n' "$*" >&2
    exit 97
fi
printf '%s\n' rustc-msrv >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = rustc-msrv ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
printf '%s\n' "${ATX_FAKE_RUSTC_VERSION-rustc 1.85.0 (fixture)}"
SCRIPT
chmod 0700 "$tmp/rustc"

cat >"$tmp/rustup" <<'SCRIPT'
#!/bin/sh
if [ "$*" != 'which --toolchain 1.85.0 rustc' ]; then
    printf 'unexpected rustup command: %s\n' "$*" >&2
    exit 97
fi
printf '%s\n' rustup-msrv >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = rustup-msrv ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
printf '%s\n' "$ATX_FAKE_COMPILER_PATH"
SCRIPT
chmod 0700 "$tmp/rustup"

cat >"$tmp/versioned-tool" <<'SCRIPT'
#!/bin/sh
name=${0##*/}
case "$name" in
    cargo-audit) version=0.22.2 ;;
    cargo-deny) version=0.20.2 ;;
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
for tool in cargo-audit cargo-deny; do
    ln -s versioned-tool "$tmp/$tool"
done

cat >"$tmp/lychee" <<'SCRIPT'
#!/bin/sh
stage=lychee
case "$*" in
    --version) stage=lychee-version ;;
    '--offline --no-progress -- README.md scripts/README.md') stage=links-offline ;;
    '--no-progress --timeout 20 --max-retries 2 --max-concurrency 8 -- README.md scripts/README.md') stage=links-online ;;
    *) printf 'unexpected lychee command: %s\n' "$*" >&2; exit 97 ;;
esac
printf '%s\n' "$stage" >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = "$stage" ]; then exit "${ATX_FAKE_FAIL_STATUS:-41}"; fi
if [ "$stage" = lychee-version ]; then printf 'lychee 0.24.2\n'; fi
SCRIPT
chmod 0700 "$tmp/lychee"

cat >"$tmp/remote-command" <<'SCRIPT'
#!/bin/sh
printf 'quality harness attempted remote command: %s\n' "${0##*/}" >&2
exit 97
SCRIPT
chmod 0700 "$tmp/remote-command"
ln -s remote-command "$tmp/gh"

cat >"$tmp/git" <<'SCRIPT'
#!/bin/sh
if [ "$*" != 'ls-files -z -- *.md' ]; then
    printf 'quality harness attempted unexpected Git command: %s\n' "$*" >&2
    exit 97
fi
printf '%s\n' git-markdown >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = git-markdown ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
printf 'README.md\000scripts/README.md\000'
SCRIPT
chmod 0700 "$tmp/git"

cat >"$tmp/npx" <<'SCRIPT'
#!/bin/sh
if [ "$#" -lt 6 ] || [ "$1" != --yes ] || [ "$2" != markdownlint-cli2@0.23.2 ] \
    || [ "$3" != --config ] || [ "$4" != .markdownlint-cli2.jsonc ] || [ "$5" != --no-globs ]; then
    printf 'unexpected Markdown command: %s\n' "$*" >&2
    exit 97
fi
printf '%s\n' npx-markdown >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = npx-markdown ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
SCRIPT
chmod 0700 "$tmp/npx"

export ATX_FAKE_LOG="$tmp/calls"
export ATX_FAKE_COMPILER_PATH="$tmp/rustc"
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
tests/fixtures/exit-codes-v1.json
tests/fixtures/json-error-v1.json
tests/fixtures/json-outcomes-v1.json
tests/fixtures/migrations/v0.sql
LIST

assert_failure() {
    mode=$1
    stage=$2
    expected_status=$3

    : >"$ATX_FAKE_LOG"
    status=0
    ATX_FAKE_FAIL_STAGE="$stage" ATX_FAKE_FAIL_STATUS="$expected_status" \
        PATH="$tmp:$PATH" "$CHECK" "$mode" >/dev/null 2>&1 || status=$?

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
    leaked_path=${2:-.local/plan.md}
    bad_package_list="$tmp/package-bad-$mode"
    cp "$ATX_PACKAGE_LIST" "$bad_package_list"
    printf '%s\n' "$leaked_path" >>"$bad_package_list"

    : >"$ATX_FAKE_LOG"
    status=0
    ATX_PACKAGE_LIST="$bad_package_list" PATH="$tmp:$PATH" \
        "$CHECK" "$mode" >/dev/null 2>&1 || status=$?

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

assert_failure markdown git-markdown 41
assert_failure markdown npx-markdown 41

assert_failure fmt cargo-fmt 41
assert_failure check cargo-check 41
assert_failure test cargo-test 41
assert_failure clippy cargo-clippy 41
assert_failure docs cargo-doc 41
assert_failure hygiene hygiene 41
assert_failure man man 41

for stage in cargo-fmt cargo-check cargo-test cargo-clippy; do
    assert_failure quick "$stage" 41
    assert_failure full "$stage" 41
    assert_failure pre-merge "$stage" 41
done

for stage in \
    rustup-msrv rustc-msrv cargo-msrv cargo-doc \
    cargo-audit-version cargo-deny-version lychee-version \
    cargo-audit cargo-deny git-markdown npx-markdown links-offline cargo-package cargo-publish
do
    assert_failure static "$stage" 41
    assert_failure full "$stage" 41
    assert_failure pre-merge "$stage" 41
done

for stage in hygiene man; do
    assert_failure full "$stage" 41
    assert_failure pre-merge "$stage" 41
done
for mode in links-offline links-online; do
    assert_failure "$mode" git-markdown 41
    assert_failure "$mode" lychee-version 41
    assert_failure "$mode" "$mode" 41
done

assert_failure msrv rustup-msrv 41
assert_failure msrv rustc-msrv 41
assert_failure msrv cargo-msrv 41

for mode in msrv static full pre-merge; do
    for version in 'rustc 1.97.0 (fixture)' 'rustc 1.85.1 (fixture)' ''; do
        : >"$ATX_FAKE_LOG"
        status=0
        ATX_FAKE_RUSTC_VERSION="$version" PATH="$tmp:$PATH" \
            "$CHECK" "$mode" >/dev/null 2>&1 || status=$?
        if [ "$status" -ne 2 ] || [ "$(tail -n 1 "$ATX_FAKE_LOG")" != rustc-msrv ]; then
            printf '%s accepted an invalid MSRV compiler: %s\n' "$mode" "$version" >&2
            exit 1
        fi
    done
done

for mode in package static full pre-merge; do
    for leaked_path in \
        .local/plan.md tests/cli_schedule.rs tests/support/mod.rs \
        tests/fixtures-other/leak.json tests/fixtures \
        tests/fixtures/../cli_schedule.rs tests/fixtures/../../.local/plan.md \
        tests/fixtures/./exit-codes-v1.json tests/fixtures/..
    do
        assert_package_policy_failure "$mode" "$leaked_path"
    done
done

for embedded_fixture in tests/fixtures/exit-codes-v1.json tests/fixtures/json-outcomes-v1.json; do
    missing_fixture="$tmp/package-missing-fixture"
    grep -Fxv "$embedded_fixture" "$ATX_PACKAGE_LIST" >"$missing_fixture"
    if scripts/check-package-list.sh "$missing_fixture" >/dev/null 2>&1; then
        printf 'package policy accepted missing embedded fixture: %s\n' "$embedded_fixture" >&2
        exit 1
    fi
done

for mode in quick static full pre-merge fmt check test clippy docs dependencies package \
    hygiene man markdown links-offline links-online msrv; do
    : >"$ATX_FAKE_LOG"
    PATH="$tmp:$PATH" "$CHECK" "$mode"
done
expected_calls=$(printf 'rustup-msrv\nrustc-msrv\ncargo-msrv')
if [ "$(cat "$ATX_FAKE_LOG")" != "$expected_calls" ]; then
    printf 'msrv did not use the explicit compiler and Cargo toolchain\n' >&2
    exit 1
fi

: >"$ATX_FAKE_LOG"
RUSTC=wrong-compiler RUSTC_WRAPPER=wrong-wrapper RUSTC_WORKSPACE_WRAPPER=wrong-wrapper \
    PATH="$tmp:$PATH" "$CHECK" msrv >/dev/null
if [ "$(cat "$ATX_FAKE_LOG")" != "$expected_calls" ]; then
    printf 'msrv did not isolate inherited compiler overrides\n' >&2
    exit 1
fi
scripts/check-package-list.sh "$ATX_PACKAGE_LIST"

# Exercise the real helper composition separately from the aggregate gate doubles.
cp "$ROOT/scripts/check-hygiene.sh" "$ROOT/scripts/check-man.sh" "$fixture/scripts/"
cat >"$tmp/helper-command" <<'SCRIPT'
#!/bin/sh
stage=${0##*/}
case "$stage" in
    cargo)
        case "$*" in
            'test --locked --package xtask') stage=xtask-test ;;
            'clippy --locked --package xtask --all-targets -- -D warnings') stage=xtask-clippy ;;
            'xtask dist-man') stage=dist-man ;;
            *) exit 97 ;;
        esac ;;
    mandoc) [ "$*" = '-T lint dist/man/atx.1' ] || exit 97 ;;
    shellcheck)
        for script in scripts/*.sh; do
            [ "${1:-}" = "$script" ] || exit 97
            shift
        done
        [ "$#" -eq 0 ] || exit 97 ;;
    test-markdown-harness.sh) [ "$*" = --real ] || exit 97 ;;
    *) [ "$#" -eq 0 ] || exit 97 ;;
esac
printf '%s\n' "$stage" >>"$ATX_FAKE_LOG"
if [ "${ATX_FAKE_FAIL_STAGE:-}" = "$stage" ]; then
    exit "${ATX_FAKE_FAIL_STATUS:-41}"
fi
SCRIPT
chmod 0700 "$tmp/helper-command"
for command in cargo shellcheck mandoc; do
    cp "$tmp/helper-command" "$tmp/$command"
done
for helper in check-workflow-guards test-check-harness test-markdown-harness test-links-harness; do
    cp "$tmp/helper-command" "$fixture/scripts/$helper.sh"
done
mkdir -p "$fixture/dist/man"
: >"$fixture/dist/man/atx.1"

for helper in hygiene man; do
    CHECK="$fixture/scripts/check-$helper.sh"
    case "$helper" in
        hygiene) stages='shellcheck check-workflow-guards.sh test-check-harness.sh test-markdown-harness.sh test-links-harness.sh' ;;
        man) stages='xtask-test xtask-clippy dist-man mandoc' ;;
    esac
    for stage in $stages; do
        assert_failure "$helper" "$stage" 53
    done
    : >"$ATX_FAKE_LOG"
    PATH="$tmp:$PATH" "$CHECK"
    expected_calls=$(for stage in $stages; do printf '%s\n' "$stage"; done)
    if [ "$(cat "$ATX_FAKE_LOG")" != "$expected_calls" ]; then
        printf '%s skipped or reordered a helper stage\n' "$helper" >&2
        exit 1
    fi
done

printf 'quality-harness-tests: PASS\n'
