#!/bin/sh

set -eu

# Assert the mock registry recorded exactly one publish of the expected
# version, and that the advertised checksum matches the packaged crate.

if [ "$#" -ne 3 ]; then
    printf 'usage: %s VERSION REQUESTS_LOG CRATE_FILE\n' "$0" >&2
    exit 2
fi

version=$1
log=$2
crate_file=$3

python3 - "$version" "$log" "$crate_file" <<'PY'
import hashlib
import json
import pathlib
import sys

version, log_name, crate_name = sys.argv[1:]
events = [json.loads(line) for line in pathlib.Path(log_name).read_text().splitlines()]
uploads = [
    event
    for event in events
    if event.get("method") == "PUT" and event.get("path") == "/api/v1/crates/new"
]
if len(uploads) != 1:
    raise SystemExit(f"expected exactly 1 crate upload, log has {len(uploads)}")

upload = uploads[0]
if upload.get("name") != "atx":
    raise SystemExit("upload metadata has the wrong crate name")
if upload.get("vers") != version:
    raise SystemExit(f"uploaded version does not match manifest version {version}")

expected = hashlib.sha256(pathlib.Path(crate_name).read_bytes()).hexdigest()
if upload.get("cksum") != expected:
    raise SystemExit("uploaded checksum does not match the packaged crate")
PY

echo "mock registry accepted atx $version"
