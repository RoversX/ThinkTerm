#!/bin/sh
# The Snippets tab, end to end: a release server under a throwaway HOME,
# its plugin host started through it, the release bundle and headless
# Chrome. The page lists, adds, searches, runs and deletes snippets; a
# change another client makes reaches it; a Run lands in the pane focused
# when it was pressed, or nowhere once too late; the host is killed and
# comes back; and through a link that adds latency, a search is timed and
# the requests typing makes are counted.
#
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/snippets.sh [out.png]
#
# Prints the JSON snippets-test.js produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18102
OUT=${1:-/tmp/thinkterm-snippets.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
# Short, under /tmp: the host's socket goes under this HOME, and a unix
# socket path is limited to 104 bytes.
T=$(mktemp -d /tmp/tts.XXXXXX)
# SLOW is where the test's latency-adding link listens; the page it serves
# is another origin, so it is allowed alongside the server's own.
SLOW=18103
if lsof -nP -t -i :$SLOW >/dev/null 2>&1; then echo "port $SLOW busy" >&2; exit 1; fi
cat > "$T/web.lua" <<LUA
return { web_servers = { {
  bind_address = "127.0.0.1:$PORT",
  allowed_origins = { "http://127.0.0.1:$PORT", "http://127.0.0.1:$SLOW" },
} } }
LUA
export HOME="$T/home"; mkdir -p "$HOME"
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
# One snippet already there, as the desktop would have left it.
DATA="$HOME/Library/Application Support/ThinkTerm"
[ "$(uname)" = Darwin ] || DATA="$HOME/.local/share/ThinkTerm"
mkdir -p "$DATA"
cat > "$DATA/snippets.json" <<JSON
{"version":1,"snippets":[{"id":"snippet-1-seeded","title":"List files","body":"ls -la","created_at_ms":1,"updated_at_ms":1}]}
JSON
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
# Two panes side by side in the tab on show: Run must land in the one that
# was focused when it was pressed.
FIRST=$($CLI list --format json | python3 -c 'import json, sys; print(json.load(sys.stdin)[0]["pane_id"])')
$CLI split-pane --pane-id "$FIRST" --right >/dev/null
URL=$($CLI web-token mint --label snippets --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME SNIPPETS_FILE="$DATA/snippets.json" TEST_HOME="$HOME" MUX_CLI="$CLI" SLOW_PORT=$SLOW \
  node "$REPO/thinkterm-web/smoke/snippets-test.js" "$URL" "$OUT" || status=$?
for p in $(pgrep -f "$T/" || true); do kill "$p" 2>/dev/null || true; done
sleep 0.5
rm -rf "$T" 2>/dev/null || true
exit $status
