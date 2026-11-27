#!/bin/sh

set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

cargo test --locked --package xtask || exit $?
cargo clippy --locked --package xtask --all-targets -- -D warnings || exit $?
cargo xtask dist-man || exit $?
mandoc -T lint dist/man/*.1 || exit $?
