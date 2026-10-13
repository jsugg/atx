#!/usr/bin/env bash

set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/atx-links-harness.XXXXXX")
server_pid=''
cleanup() {
    if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
    rm -rf "$tmp"
}
trap cleanup EXIT
fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/docs" "$tmp/bin"
cp "$ROOT/scripts/check-links.sh" "$fixture/scripts/"
git init -q "$fixture"
printf '# Links\n' > "$fixture/docs/a [literal]* file.md"
git -C "$fixture" add 'docs/a [literal]* file.md'
mkdir -p "$fixture/.local"
printf '[untracked](http://127.0.0.1:1/missing)\n' > "$fixture/.local/private.md"
printf '[untracked](http://127.0.0.1:1/missing)\n' > "$fixture/docs/untracked.md"

cat > "$tmp/bin/lychee" <<'SCRIPT'
#!/usr/bin/env bash
if [ "$*" = --version ]; then printf '%s\n' "${ATX_LINK_VERSION:-lychee 0.24.2}"; exit 0; fi
printf '%s\0' "$@" > "$ATX_LINK_ARGS"
exit "${ATX_LINK_STATUS:-0}"
SCRIPT
chmod 0700 "$tmp/bin/lychee"
export ATX_LINK_ARGS="$tmp/arguments"
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" online
printf '%s\0' --no-progress --timeout 20 --max-retries 2 --max-concurrency 8 -- 'docs/a [[]literal[]][*] file.md' > "$tmp/expected"
cmp "$tmp/expected" "$ATX_LINK_ARGS"
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" offline
printf '%s\0' --offline --no-progress -- 'docs/a [[]literal[]][*] file.md' > "$tmp/expected"
cmp "$tmp/expected" "$ATX_LINK_ARGS"
status=0
ATX_LINK_STATUS=41 PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" online || status=$?
test "$status" -eq 41
status=0
ATX_LINK_VERSION='lychee 0.25.0' PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" online || status=$?
test "$status" -eq 2

cat > "$tmp/bin/git" <<'SCRIPT'
#!/bin/sh
exit 73
SCRIPT
chmod 0700 "$tmp/bin/git"
status=0
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" offline || status=$?
test "$status" -eq 73
rm "$tmp/bin/git"
git -C "$fixture" rm --cached -q 'docs/a [literal]* file.md'
status=0
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-links.sh" offline || status=$?
test "$status" -eq 1
git -C "$fixture" add 'docs/a [literal]* file.md'

if [ "${1:-}" = --real ]; then
    cat > "$tmp/server.mjs" <<'SCRIPT'
import http from "node:http";
import fs from "node:fs";
const server = http.createServer((request, response) => {
  response.writeHead(request.url === "/ok" ? 200 : 404);
  response.end("fixture");
});
server.listen(0, "127.0.0.1", () => {
  fs.writeFileSync(process.argv[2], String(server.address().port));
});
SCRIPT
    node "$tmp/server.mjs" "$tmp/port" &
    server_pid=$!
    attempts=0
    while [ ! -s "$tmp/port" ]; do
        kill -0 "$server_pid"
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 50 ]; then printf 'fixture server did not start\n' >&2; exit 1; fi
        sleep 0.1
    done
    port=$(cat "$tmp/port")
    printf '# Links\n\n[valid](http://127.0.0.1:%s/ok)\n' "$port" > "$fixture/docs/a [literal]* file.md"
    "$fixture/scripts/check-links.sh" online
    printf '# Links\n\n[broken](http://127.0.0.1:%s/missing)\n' "$port" > "$fixture/docs/a [literal]* file.md"
    status=0
    "$fixture/scripts/check-links.sh" online > "$tmp/broken.log" 2>&1 || status=$?
    test "$status" -eq 2
    grep -q '404' "$tmp/broken.log"
fi
printf 'link-harness: PASS\n'
