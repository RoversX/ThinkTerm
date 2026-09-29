#!/bin/sh
# Plugins in a browser, end to end: a release server under a throwaway
# HOME with the example plugin installed beside a broken one, its plugin
# host started through it, the release bundle and headless Chrome. The page
# lists the plugins in Settings › Sidebar & Plugins and shows nothing of them outside
# the sidebar, turns them off and on -- Snippets' tab going with its switch
# -- reloads them, and says in a row why a plugin failed.
#
#   cargo build --release -p wezterm-mux-server -p wezterm \
#     -p thinkterm-plugin-server -p thinkterm-plugin-example
#   NODE_PATH=<dir with node_modules/ws> thinkterm-web/smoke/plugins.sh [out.png]
#
# Prints the JSON plugins-test.js produces; exit status is the test's.
set -eu
cd "$(dirname "$0")/../.."
REPO=$PWD
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release
PORT=18104
OUT=${1:-/tmp/thinkterm-plugins.png}
if lsof -nP -t -i :$PORT >/dev/null 2>&1; then echo "port $PORT busy" >&2; exit 1; fi
REAL_HOME=$HOME
# Short, under /tmp: the host's socket goes under this HOME, and a unix
# socket path is limited to 104 bytes.
T=$(mktemp -d /tmp/ttp.XXXXXX)
cat > "$T/web.lua" <<LUA
return { web_servers = { { bind_address = "127.0.0.1:$PORT" } } }
LUA
export HOME="$T/home"; mkdir -p "$HOME"
export THINKTERM_NO_PRIVACY_DISCLAIM=1
export THINKTERM_WEB_STATIC_DIR="$REPO/thinkterm-web/www"
DATA="$HOME/Library/Application Support/ThinkTerm"
[ "$(uname)" = Darwin ] || DATA="$HOME/.local/share/ThinkTerm"
# The example, installed as its docs say; a plugin whose manifest does not
# read; and a second copy of the example under another directory, which
# claims its id and so is listed as not used.
mkdir -p "$DATA/plugins/text-tools" "$DATA/plugins/broken" "$DATA/plugins/twin"
cp "$REPO/plugins/example/plugin.toml" "$BIN/thinkterm-plugin-example" "$DATA/plugins/text-tools/"
cp "$REPO/plugins/example/plugin.toml" "$DATA/plugins/twin/"
echo 'id = ' > "$DATA/plugins/broken/plugin.toml"
"$BIN/thinkterm-mux-server" --config-file "$T/web.lua" --daemonize
i=0; until curl -s -o /dev/null "http://127.0.0.1:$PORT/"; do i=$((i+1)); [ $i -lt 50 ] || { echo "server did not come up" >&2; exit 1; }; sleep 0.2; done
CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm --config-file $T/web.lua cli --prefer-mux --no-auto-start"
PLUGIN_CLI="env HOME=$HOME THINKTERM_NO_PRIVACY_DISCLAIM=1 $BIN/thinkterm plugin"
URL=$($CLI web-token mint --label plugins --ttl 1h --url-only 2>/dev/null | head -1)
status=0
HOME=$REAL_HOME TEST_HOME="$HOME" MUX_CLI="$CLI" PLUGIN_CLI="$PLUGIN_CLI" PLUGINS_DATA="$DATA" \
  EXAMPLE_DIR="$REPO/plugins/example" EXAMPLE_BIN="$BIN/thinkterm-plugin-example" \
  node "$REPO/thinkterm-web/smoke/plugins-test.js" "$URL" "$OUT" || status=$?
for p in $(pgrep -f "$T/" || true); do kill "$p" 2>/dev/null || true; done
sleep 0.5
rm -rf "$T" 2>/dev/null || true
exit $status
