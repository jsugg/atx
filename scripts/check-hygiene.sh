#!/bin/sh

set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

shellcheck scripts/*.sh || exit $?
scripts/check-workflow-guards.sh || exit $?
scripts/test-check-harness.sh || exit $?
scripts/test-markdown-harness.sh --real || exit $?
scripts/test-links-harness.sh || exit $?
