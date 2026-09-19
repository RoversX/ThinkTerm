#!/bin/sh
# Build the iOS app for the simulator and, optionally, run it.
#
#   ios/build.sh            build the Rust library, bindings, project and app
#   ios/build.sh run        ...and boot a simulator, install, launch
#   ios/build.sh shot FILE  ...and save a screenshot to FILE after launch
#
# Device builds (aarch64-apple-ios) need a signing team; not wired here yet.
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD
SIM_TARGET=aarch64-apple-ios-sim
SIM_NAME=${SIM_NAME:-iPhone 17}
DERIVED=${DERIVED:-$ROOT/target/xcode}
BUNDLE=com.roversx.thinkterm.ios

echo "== rust ($SIM_TARGET, release)"
cargo build -p thinkterm-mobile --release --target $SIM_TARGET

echo "== bindings"
rm -rf ios/Generated && mkdir -p ios/Generated
cargo run -q -p thinkterm-mobile --bin uniffi-bindgen -- generate \
  --library target/$SIM_TARGET/release/libthinkterm_mobile.a \
  --language swift --out-dir ios/Generated
# Swift finds a module by a `module.modulemap` on its include path.
mv ios/Generated/thinkterm_mobileFFI.modulemap ios/Generated/module.modulemap

echo "== fonts"
mkdir -p ios/Resources
cp assets/fonts/JetBrainsMono-Regular.ttf assets/fonts/SymbolsNerdFontMono-Regular.ttf ios/Resources/

echo "== project"
(cd ios && xcodegen generate --quiet)

echo "== xcodebuild"
xcodebuild -project ios/ThinkTerm.xcodeproj -scheme ThinkTerm \
  -configuration Debug -sdk iphonesimulator \
  -destination "platform=iOS Simulator,name=$SIM_NAME" \
  -derivedDataPath "$DERIVED" build 2>&1 | grep -E "error:|warning: .*Sources/|BUILD (SUCCEEDED|FAILED)" || true
APP="$DERIVED/Build/Products/Debug-iphonesimulator/ThinkTerm.app"
test -d "$APP" || { echo "no app bundle at $APP" >&2; exit 1; }

[ "${1:-}" = run ] || [ "${1:-}" = shot ] || exit 0

echo "== simulator"
UDID=$(xcrun simctl list devices available | grep "$SIM_NAME (" | head -1 | sed -E 's/.*\(([0-9A-F-]+)\).*/\1/')
xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null
xcrun simctl install "$UDID" "$APP"
xcrun simctl terminate "$UDID" $BUNDLE 2>/dev/null || true
xcrun simctl launch "$UDID" $BUNDLE
if [ "${1:-}" = shot ]; then
  sleep "${SHOT_DELAY:-3}"
  xcrun simctl io "$UDID" screenshot "$2"
  echo "screenshot: $2"
fi
