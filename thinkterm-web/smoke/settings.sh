#!/bin/sh
# The settings panel, end to end: a release server with an empty tree under
# a throwaway HOME, the release bundle, headless Chrome, pinning the font,
# following the desktop again, the light theme, and a reload that keeps
# what was chosen.
#
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/settings.sh [out.png]
#
# Prints the JSON settings-test.js produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18099
OUT=${1:-/tmp/thinkterm-settings.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
T=$(mktemp -d "${TMPDIR:-/tmp}/tt-settings.XXXXXX")
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
# Nothing is spawned here: the page makes its own tab when it attaches to
# an empty server. There is no desktop to follow, so the size the page
# lands on is what "Follow the desktop" has to come back to; the test reads
# it rather than assuming one.
URL=$($CLI web-token mint --label settings --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME node "$REPO/thinkterm-web/smoke/settings-test.js" "$URL" "$CLI" "$OUT" || status=$?
for p in $(pgrep -f "$T/web.lua" || true); do kill "$p"; done
# The server may still be closing its files; a leftover directory is not
# worth a failure.
sleep 0.5
rm -rf "$T" 2>/dev/null || true
exit $status
