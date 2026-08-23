#!/bin/sh

set -eu

if [ "$#" -ne 1 ]; then
    printf 'usage: %s PACKAGE_LIST\n' "$0" >&2
    exit 2
fi

list=$1

for required in Cargo.toml Cargo.lock README.md CHANGELOG.md LICENSE-MIT src/main.rs; do
    if ! grep -Fx "$required" "$list" >/dev/null; then
        printf 'package is missing %s\n' "$required" >&2
        exit 1
    fi
done

while IFS= read -r path; do
    case "$path" in
        .github/* | .local/* | benches/* | docs/* | fuzz/* | scripts/* | target/* | tests/*)
            printf 'internal path leaked into package: %s\n' "$path" >&2
            exit 1
            ;;
    esac
done <"$list"
