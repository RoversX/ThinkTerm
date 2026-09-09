#!/bin/sh
# Browser performance baseline: release server, release bundle, headless
# Chrome, all under a throwaway HOME so nothing of yours is touched.
#
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/bench.sh
#
# Wants `ci/build-web.sh` and `cargo build --release` done first, and port
# 18088 free. Prints the JSON line bench.js produces; docs/thinkterm/web-baseline.md explains it.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18088
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
T=$(mktemp -d "${TMPDIR:-/tmp}/tt-bench.XXXXXX")
cat > "$T/web.lua" <<LUA
return { web_servers = { { bind_address = "127.0.0.1:$PORT" } } }
LUA
export HOME="$T/home"; mkdir -p "$HOME"
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
CLI="$BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
URL=$($CLI web-token mint --label bench --ttl 1h --url-only 2>/dev/null | head -1)
# The send is pinned to this server: without --prefer-mux and this HOME the
# CLI would reach a running GUI instead.
SEND="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $CLI send-text --no-paste --pane-id 0"
status=0
HOME=$REAL_HOME node "$REPO/thinkterm-web/smoke/bench.js" "$URL" "$SEND" --timeout 60000 || status=$?
for p in $(pgrep -f "$T/web.lua" || true); do kill "$p"; done
# The server may still be closing its files; a leftover directory is not
# worth a failure.
sleep 0.5
rm -rf "$T" 2>/dev/null || true
exit $status
