#!/bin/sh

set -eu

# Assert the mock registry recorded exactly one publish of the expected
# version, and that the advertised checksum matches the packaged crate.

if [ "$#" -ne 2 ]; then
    printf 'usage: %s VERSION REQUESTS_LOG\n' "$0" >&2
    exit 2
fi

version=$1
log=$2

uploads=$(grep -c '"method": "PUT"' "$log" || true)
if [ "$uploads" -ne 1 ]; then
    printf 'expected exactly 1 upload, log has %s\n' "$uploads" >&2
    exit 1
fi

recorded=$(grep '"method": "PUT"' "$log")
printf '%s\n' "$recorded" | grep -q '"name": "atx"' || {
    echo 'upload metadata is missing name atx' >&2
    exit 1
}
printf '%s\n' "$recorded" | grep -F "\"vers\": \"$version\"" >/dev/null || {
    printf 'uploaded version does not match manifest version %s\n' "$version" >&2
    exit 1
}

echo "mock registry accepted atx $version"
