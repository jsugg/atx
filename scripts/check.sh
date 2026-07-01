#!/bin/sh

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
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
        exit 127
    fi

    actual=$("$tool" --version 2>/dev/null)
    case "$actual" in
        *"$expected"*) ;;
        *)
            printf 'wrong %s version: expected %s, got %s\n' "$tool" "$expected" "$actual" >&2
            exit 2
            ;;
    esac
}

quick() {
    cargo fmt --check
    cargo check --all-targets --all-features --locked
    cargo test --all-targets --all-features --locked
    cargo clippy --all-targets --all-features --locked -- -D warnings
}

full() {
    quick
    cargo +1.85.0 check --all-targets --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

    require_version cargo-audit 0.22.2
    require_version cargo-deny 0.20.2
    require_version mdlint 0.3.22
    require_version lychee 0.24.2

    cargo audit
    cargo deny check
    # shellcheck disable=SC2046
    mdlint check $(markdown_files)
    # shellcheck disable=SC2046
    lychee --offline $(markdown_files)

    package_list=$(mktemp "${TMPDIR:-/tmp}/atx-package.XXXXXX")
    trap 'rm -f "$package_list"' EXIT HUP INT TERM
    cargo package --list >"$package_list"
    scripts/check-package-list.sh "$package_list"
    cargo publish --dry-run --locked
}

case "$mode" in
    quick) quick ;;
    full) full ;;
    *)
        printf 'usage: %s quick|full\n' "$0" >&2
        exit 2
        ;;
esac
