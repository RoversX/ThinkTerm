#!/bin/sh
# A plugin's panel in a browser, end to end: a release server under a
# throwaway HOME with the diff plugin installed, a terminal in a git
# repository made for the test, the release bundle and headless Chrome. The
# page offers the plugin's panel in the right panel's selector, paints the
# repository's changes, tints what the pointer is over with rounded
# corners, sends a click on a file to the plugin and opens the extended
# view it asks for, showing the file there, scrolls ten thousand changed
# lines by the wheel and by a finger, asking for the rows that come into
# view, scrolls a wide line sideways under its line numbers, sizes the
# extended view by its edge and closes it by its button, and lets the panel
# go with its tab; then, shaped as a phone, a tap picks a file in the
# drawer and a finger scrolls its lines there.
#
#   cargo build --release -p wezterm-mux-server -p wezterm \
#     -p thinkterm-plugin-server -p thinkterm-plugin-diff
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/panels.sh [out.png]
#
# Prints the JSON panels-test.js produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18105
OUT=${1:-/tmp/thinkterm-panels.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
# Short, under /tmp: the host's socket goes under this HOME, and a unix
# socket path is limited to 104 bytes.
T=$(mktemp -d /tmp/ttq.XXXXXX)
# However it ends, what runs under the throwaway HOME is stopped and the
# HOME removed: a server left behind holds the port for the next run.
cleanup() {
  for p in $(pgrep -f "$T/" || true); do kill "$p" 2>/dev/null || true; done
  sleep 0.5
  rm -rf "$T" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 1' INT TERM
export HOME="$T/home"; mkdir -p "$HOME"
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
DATA="$HOME/Library/Application Support/ThinkTerm"
[ "$(uname)" = Darwin ] || DATA="$HOME/.local/share/ThinkTerm"
mkdir -p "$DATA/plugins/diff"
cp "$REPO/plugins/diff/plugin.toml" "$BIN/thinkterm-plugin-diff" "$DATA/plugins/diff/"
# A repository with a commit and changes since: a file edited in two places,
# one of five thousand lines all rewritten -- ten thousand changed lines --
# one with a line far wider than the panel, and a new one.
FIX="$T/repo"
mkdir -p "$FIX/src"
g() {
  git -C "$FIX" -c user.name=example -c user.email=user@example.com \
    -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"
}
g init -q -b main
printf 'one\ntwo\nthree\nfour\nfive\n' > "$FIX/README.md"
seq 1 5000 | sed 's/^/line /' > "$FIX/src/big.txt"
printf 'short\n' > "$FIX/wide.txt"
g add .
g commit -q -m first
printf 'one\n2\nthree\nfour\nfive\nsix\n' > "$FIX/README.md"
seq 1 5000 | sed 's/^/row /' > "$FIX/src/big.txt"
{ printf 'short\n'; awk 'BEGIN { for (i = 0; i < 300; i++) printf "wide%03d ", i; print "" }'; } > "$FIX/wide.txt"
printf 'hello\n' > "$FIX/new.txt"
# The terminals start in it: the page shows the server's first.
cat > "$T/web.lua" <<LUA
return {
  web_servers = { { bind_address = "127.0.0.1:$PORT" } },
  default_cwd = "$FIX",
}
LUA
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
$CLI spawn --window-id 0 >/dev/null
URL=$($CLI web-token mint --label panels --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME PLUGINS_DATA="$DATA" node "$REPO/thinkterm-web/smoke/panels-test.js" "$URL" "$OUT" || status=$?
exit $status
