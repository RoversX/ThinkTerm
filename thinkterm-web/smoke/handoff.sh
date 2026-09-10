#!/bin/sh
# The tab strip, end to end: a release server with two tabs under a
# throwaway HOME, the release bundle, headless Chrome.
#
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/tabs.sh [out.png]
#
# Prints the JSON tabs-test.js produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18107
OUT=${1:-/tmp/thinkterm-tabs.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
T=$(mktemp -d "${TMPDIR:-/tmp}/tt-handoff.XXXXXX")
cat > "$T/web.lua" <<LUA
return { web_servers = { { bind_address = "127.0.0.1:$PORT" } } }
LUA
export HOME="$T/home"; mkdir -p "$HOME"
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
# Pinned to this server: without --prefer-mux and this HOME the CLI would
# reach a running GUI instead.
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
$CLI spawn --window-id 0 >/dev/null
URL=$($CLI web-token mint --label a --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME node "$REPO/thinkterm-web/smoke/handoff-test.js" "$URL" "$($CLI web-token mint --label b --ttl 1h --url-only 2>/dev/null | head -1)" "$CLI" "$OUT" || status=$?
for p in $(pgrep -f "$T/web.lua" || true); do kill "$p"; done
# The server may still be closing its files; a leftover directory is not
# worth a failure.
sleep 0.5
rm -rf "$T" 2>/dev/null || true
exit $status
