#!/bin/sh

set -eu

ROOT=$(CDPATH="" cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

mode=${1:-quick}

markdown() {
    scripts/check-markdown.sh || return $?
}

fmt() { cargo fmt --check || return $?; }
check() { cargo check --workspace --all-targets --all-features --locked || return $?; }
test_stage() { cargo test --workspace --all-targets --all-features --locked || return $?; }
clippy() { cargo clippy --workspace --all-targets --all-features --locked -- -D warnings || return $?; }
docs() { RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --all-features || return $?; }
hygiene() { scripts/check-hygiene.sh || return $?; }
man() { scripts/check-man.sh || return $?; }
links_offline() { scripts/check-links.sh offline || return $?; }
links_online() { scripts/check-links.sh online || return $?; }

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

# Fast edit loop; CI calls these same stages separately on its runners.
quick() {
    fmt || return $?
    check || return $?
    test_stage || return $?
    clippy || return $?
}

msrv() {
    compiler=$(rustup which --toolchain 1.85.0 rustc) || return $?
    version=$("$compiler" --version) || return $?
    case "$version" in
        'rustc 1.85.0 '*) ;;
        *)
            printf 'wrong MSRV compiler: expected Rust 1.85.0, got %s\n' "$version" >&2
            return 2
            ;;
    esac
    printf '%s\n' "$version"
    RUSTC="$compiler" RUSTC_WRAPPER='' RUSTC_WORKSPACE_WRAPPER='' \
        cargo +1.85.0 check --workspace --all-targets --all-features --locked || return $?
}

dependencies() {
    require_version cargo-audit 0.22.2 || return $?
    require_version cargo-deny 0.20.2 || return $?
    cargo audit || return $?
    cargo deny check || return $?
}

package() {
    package_list=$(mktemp "${TMPDIR:-/tmp}/atx-package.XXXXXX") || return $?
    trap 'rm -f "$package_list"' EXIT HUP INT TERM
    cargo package --list >"$package_list" || return $?
    scripts/check-package-list.sh "$package_list" || return $?
    cargo publish --dry-run --locked || return $?
}

# Static checks are part of pre-merge; remote-only checks are documented separately.
static() {
    msrv || return $?
    docs || return $?
    dependencies || return $?
    markdown || return $?
    links_offline || return $?
    package || return $?
}

pre_merge() {
    quick || return $?
    static || return $?
    hygiene || return $?
    man || return $?
}

case "$mode" in
    quick) quick || exit $? ;;
    pre-merge|full) pre_merge || exit $? ;;
    fmt) fmt || exit $? ;;
    check) check || exit $? ;;
    test) test_stage || exit $? ;;
    clippy) clippy || exit $? ;;
    docs) docs || exit $? ;;
    dependencies) dependencies || exit $? ;;
    package) package || exit $? ;;
    hygiene) hygiene || exit $? ;;
    man) man || exit $? ;;
    links-offline) links_offline || exit $? ;;
    links-online) links_online || exit $? ;;
    static) static || exit $? ;;
    msrv) msrv || exit $? ;;
    markdown) markdown || exit $? ;;
    *)
        printf 'usage: %s pre-merge|full|quick|static|fmt|check|test|clippy|msrv|docs|dependencies|package|hygiene|man|markdown|links-offline|links-online\n' "$0" >&2
        printf '  quick  fmt+check+test+clippy (pre-merge)\n' >&2
        printf '  msrv   verify and check with Rust 1.85.0 (no compiler wrappers)\n' >&2
        printf '  markdown pinned CLI2+canonical config over tracked Markdown\n' >&2
        printf '  static MSRV+doc+audit+deny+markdown+lychee+package (extended)\n' >&2
        printf '  pre-merge/full quick+static+hygiene+man (local contributor gate)\n' >&2
        exit 2
        ;;
esac
