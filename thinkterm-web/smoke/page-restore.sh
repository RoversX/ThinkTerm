#!/bin/sh
# A thread's pages come back with it after the server is gone and started
# again: a server under a throwaway HOME with the example plugin installed,
# the bundle and headless Chrome. The page makes a thread; three pages are
# opened in it -- beside its terminal, over it, and in a tab of their own;
# the server is killed outright and a new one started; the page attaches
# again, and the thread is restored with its pages where they were, each the
# plugin's and opened with what it was opened with.
#
#   cargo build --release -p wezterm-mux-server -p wezterm \
#     -p thinkterm-plugin-server -p thinkterm-plugin-example
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/page-restore.sh
#
# PROFILE=debug runs the debug build. Prints the JSON page-restore-test.js
# produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/${PROFILE:-release}
PORT=18117
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
# Short, under /tmp: the host's socket goes under this HOME, and a unix
# socket path is limited to 104 bytes.
T=$(mktemp -d /tmp/ttv.XXXXXX)
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
# Both carry the throwaway HOME with them: the test runs them again from a
# process that has the real one, and a server started there would be a
# server in the real home.
SERVER="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 THINKTERM_WEB_STATIC_DIR=$THINKTERM_WEB_STATIC_DIR $BIN/thinkterm-mux-server --config-file $T/web.lua --daemonize"
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
up() {
  $SERVER
  i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
}
up
# A plugin runs only once it is let: as the user turning it on does.
"$BIN/thinkterm" plugin enable text-tools >/dev/null
status=0
HOME=$REAL_HOME node "$REPO/thinkterm-web/smoke/page-restore-test.js" "$CLI" "$T" "$SERVER" "$PORT" || status=$?
exit $status
