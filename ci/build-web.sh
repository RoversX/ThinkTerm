#!/bin/sh
# Build the browser client bundle into thinkterm-web/www:
#   pkg/     the wasm and its wasm-bindgen JavaScript glue
#   fonts/   the faces the page loads
#   index.html, assets/   the page, built by Vite from thinkterm-web/ui
# Usage: ci/build-web.sh [--debug]
# Needs the wasm32-unknown-unknown target, the wasm-bindgen CLI at the
# version the workspace pins (see Cargo.toml), and Node 22+ with npm.
# wasm-opt is used if present.
set -eu
cd "$(dirname "$0")/.."

PROFILE=release
CARGO_FLAGS=--release
if [ "${1:-}" = "--debug" ]; then PROFILE=debug; CARGO_FLAGS=; fi

WANT=$(grep -E '^wasm-bindgen = "=' Cargo.toml | sed -E 's/.*"=([0-9.]+)".*/\1/')
HAVE=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')
if [ "$WANT" != "$HAVE" ]; then
  echo "wasm-bindgen CLI $HAVE does not match the pinned crate $WANT; run: cargo install wasm-bindgen-cli --version $WANT" >&2
  exit 1
fi

TARGET_DIR=${CARGO_TARGET_DIR:-target}
cargo build -p thinkterm-web --target wasm32-unknown-unknown $CARGO_FLAGS
WASM="$TARGET_DIR/wasm32-unknown-unknown/$PROFILE/thinkterm_web.wasm"
OUT=thinkterm-web/www
rm -rf "$OUT/pkg"
wasm-bindgen --target web --out-dir "$OUT/pkg" "$WASM"
if command -v wasm-opt >/dev/null 2>&1 && [ "$PROFILE" = release ]; then
  wasm-opt -Oz -o "$OUT/pkg/thinkterm_web_bg.wasm" "$OUT/pkg/thinkterm_web_bg.wasm"
fi
# The colour-scheme table the picker fetches. Compiled into the binary, so
# this is a build step and not a file in the tree.
cargo build -p wezterm --bin thinkterm $CARGO_FLAGS
"$TARGET_DIR/$PROFILE/thinkterm" cli color-schemes --json > "$OUT/schemes.json"
mkdir -p "$OUT/fonts"
cp assets/fonts/JetBrainsMono-Regular.ttf assets/fonts/SymbolsNerdFontMono-Regular.ttf "$OUT/fonts/"
# The page itself. Vite writes index.html and assets/ into $OUT and leaves
# pkg/ and fonts/ alone (emptyOutDir is off), so the stale hashed assets of
# an earlier build are cleared here. npm ci installs exactly the lockfile.
rm -rf "$OUT/assets" "$OUT/index.html" "$OUT/index.html.gz"
UI=thinkterm-web/ui
# The one step here that reaches the network, and the one worth retrying:
# a registry hiccup would otherwise fail a release build whose packaging
# jobs all wait on this bundle. The rest of this script is local work, so
# retrying the script as a whole would only multiply the cost of a genuine
# failure.
if [ ! -d "$UI/node_modules" ] || [ "$UI/package-lock.json" -nt "$UI/node_modules/.package-lock.json" ]; then
  bash ci/retry.sh npm ci --prefix "$UI" --no-audit --no-fund
fi
npm run --prefix "$UI" --silent build
# Precompress what a browser will take compressed. The mux server sends a
# .gz sibling as it is when the client accepts gzip, so this cost is paid
# once here instead of per request, and -9 beats what a server would spend
# in a request's time. -n leaves out the name and timestamp so the output
# is the same for the same input.
for f in "$OUT/index.html" "$OUT/schemes.json" "$OUT/assets"/*.js "$OUT/assets"/*.css "$OUT/pkg"/*.js "$OUT/pkg"/*.wasm "$OUT/fonts"/*.ttf; do
  [ -f "$f" ] || continue
  gzip -9 -n -c "$f" > "$f.gz"
done

ls -la "$OUT/pkg"
echo "bundle at $OUT; serve it with web_servers.static_dir = \"$(pwd)/$OUT\" or THINKTERM_WEB_STATIC_DIR"
