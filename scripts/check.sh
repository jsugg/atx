#!/bin/sh

set -eu

ROOT=$(CDPATH="" cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

mode=${1:-quick}

markdown_files() {
    find docs -type f -name '*.md' -print
    printf '%s\n' README.md CHANGELOG.md CONTRIBUTING.md SECURITY.md CODE_OF_CONDUCT.md
}

require_version() {
    tool=$1
    expected=$2

    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'missing required tool: %s %s\n' "$tool" "$expected" >&2
        return 127
    fi

    actual=$("$tool" --version 2>/dev/null) || return $?
    case "$actual" in
        *"$expected"*) ;;
        *)
            printf 'wrong %s version: expected %s, got %s\n' "$tool" "$expected" "$actual" >&2
            return 2
            ;;
    esac
}

# Deterministic pre-merge gate: build, tests, lint. This is what CI runs per
# push; it is deliberately NOT the release gate.
quick() {
    cargo fmt --check || return $?
    cargo check --all-targets --all-features --locked || return $?
    cargo test --all-targets --all-features --locked || return $?
    cargo clippy --all-targets --all-features --locked -- -D warnings || return $?
}

# Extended static evidence on top of quick. Still not the release gate:
# coverage floors, mutation, fuzz budgets, and performance evidence run as
# dedicated scheduled/release workflows (see .github/workflows).
static() {
    cargo +1.85.0 check --all-targets --all-features --locked || return $?
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features || return $?

    require_version cargo-audit 0.22.2 || return $?
    require_version cargo-deny 0.20.2 || return $?
    require_version mdlint 0.3.22 || return $?
    require_version lychee 0.24.2 || return $?

    cargo audit || return $?
    cargo deny check || return $?
    markdown_paths=$(markdown_files) || return $?
    # shellcheck disable=SC2086
    mdlint check $markdown_paths || return $?
    # shellcheck disable=SC2086
    lychee --offline $markdown_paths || return $?

    package_list=$(mktemp "${TMPDIR:-/tmp}/atx-package.XXXXXX") || return $?
    trap 'rm -f "$package_list"' EXIT HUP INT TERM
    cargo package --list >"$package_list" || return $?
    scripts/check-package-list.sh "$package_list" || return $?
    cargo publish --dry-run --locked || return $?
}

case "$mode" in
    quick) quick || exit $? ;;
    # Historical aggregate retained for CI compatibility.
    full)
        quick || exit $?
        static || exit $?
        ;;
    static) static || exit $? ;;
    *)
        printf 'usage: %s quick|static|full\n' "$0" >&2
        printf '  quick  fmt+check+test+clippy (pre-merge)\n' >&2
        printf '  static MSRV+doc+audit+deny+mdlint+lychee+package (extended)\n' >&2
        printf '  full   quick+static (kept as an alias)\n' >&2
        exit 2
        ;;
esac
