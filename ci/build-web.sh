#!/bin/sh
# Build the browser client bundle into thinkterm-web/www:
#   pkg/     the wasm and its wasm-bindgen JavaScript glue
#   fonts/   the faces the page loads
# Usage: ci/build-web.sh [--debug]
# Needs the wasm32-unknown-unknown target and the wasm-bindgen CLI at the
# version the workspace pins (see Cargo.toml). wasm-opt is used if present.
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
wasm-bindgen --target web --out-dir "$OUT/pkg" --no-typescript "$WASM"
if command -v wasm-opt >/dev/null 2>&1 && [ "$PROFILE" = release ]; then
  wasm-opt -Oz -o "$OUT/pkg/thinkterm_web_bg.wasm" "$OUT/pkg/thinkterm_web_bg.wasm"
fi
mkdir -p "$OUT/fonts"
cp assets/fonts/JetBrainsMono-Regular.ttf assets/fonts/SymbolsNerdFontMono-Regular.ttf "$OUT/fonts/"
ls -la "$OUT/pkg"
echo "bundle at $OUT; serve it with web_servers.static_dir = \"$(pwd)/$OUT\" or THINKTERM_WEB_STATIC_DIR"
