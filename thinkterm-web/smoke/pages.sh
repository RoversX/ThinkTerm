#!/bin/sh
# A plugin's page in a browser, end to end: a server under a throwaway HOME
# with the example plugin installed, a terminal, the plugin's page opened in
# a tab of its own with something to decode, the bundle and headless Chrome.
# The page draws the plugin's page in its pane's place and names its tab
# after the plugin; the palette lists the plugin's actions, one of which
# types in the terminal and one of which opens another page; and a page's
# view is let go with its tab.
#
#   cargo build --release -p wezterm-mux-server -p wezterm \
#     -p thinkterm-plugin-server -p thinkterm-plugin-example
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/pages.sh [out.png]
#
# PROFILE=debug runs the debug build. Prints the JSON pages-test.js
# produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/${PROFILE:-release}
PORT=18113
OUT=${1:-/tmp/thinkterm-pages.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
# Short, under /tmp: the host's socket goes under this HOME, and a unix
# socket path is limited to 104 bytes.
T=$(mktemp -d /tmp/ttg.XXXXXX)
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
# Run from a ThinkTerm pane, these name the server that pane is in: the
# command line would go there, not to the throwaway one.
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
DATA="$HOME/Library/Application Support/ThinkTerm"
[ "$(uname)" = Darwin ] || DATA="$HOME/.local/share/ThinkTerm"
mkdir -p "$DATA/plugins/text-tools"
cp "$REPO/plugins/example/plugin.toml" "$BIN/thinkterm-plugin-example" "$DATA/plugins/text-tools/"
cat > "$T/web.lua" <<LUA
return {
  web_servers = { { bind_address = "127.0.0.1:$PORT" } },
  default_cwd = "$T",
}
LUA
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
# A plugin runs only once it is let: as the user turning it on does.
"$BIN/thinkterm" plugin enable text-tools >/dev/null
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
TERMINAL=$($CLI spawn --window-id 0)
# "aGVsbG8gcGFnZQ==" is "hello page".
PAGE=$($CLI open-page text-tools --pane-id "$TERMINAL" --title "Text Tools" --args '{"text":"aGVsbG8gcGFnZQ=="}')
URL=$($CLI web-token mint --label pages --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME node "$REPO/thinkterm-web/smoke/pages-test.js" "$URL" "$CLI" "$TERMINAL" "$PAGE" "$OUT" || status=$?
exit $status
