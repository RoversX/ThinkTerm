#!/bin/sh
# Build the iOS app for the simulator and, optionally, run it.
#
#   ios/build.sh            build the Rust library, bindings, project and app
#   ios/build.sh run        ...and boot a simulator, install, launch
#   ios/build.sh shot FILE  ...and save a screenshot to FILE after launch
#   ios/build.sh xcode      build the Rust library for a real iPhone as well,
#                           generate the project, and open it in Xcode: pick
#                           your phone and your team there and press Run.
#
# A signing team for the phone goes in ios/Local.xcconfig (not tracked):
#   DEVELOPMENT_TEAM = ABCDE12345
# or is chosen in Xcode's Signing & Capabilities; the project is generated
# again by every build, so the file is the setting that lasts.
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD
SIM_TARGET=aarch64-apple-ios-sim
DEVICE_TARGET=aarch64-apple-ios
SIM_NAME=${SIM_NAME:-iPhone 17}
DERIVED=${DERIVED:-$ROOT/target/xcode}
BUNDLE=com.roversx.thinkterm.ios

# Only the static library: the crate's cdylib is for Android, and its
# link fails on the iPhone target. The deployment target keeps rustc and
# ring's C objects on the same iOS version.
export IPHONEOS_DEPLOYMENT_TARGET=17.0
# cargo rustc with a crate type does not always leave the library at
# target/<triple>/release/ (the path the project links); the copy in
# deps/ is put there when it is missing.
rust_lib() {
  echo "== rust ($1, release)"
  cargo rustc -p thinkterm-mobile --lib --release --target "$1" --crate-type staticlib
  out=target/$1/release/libthinkterm_mobile.a
  if [ ! -f "$out" ]; then
    newest=$(ls -t target/$1/release/deps/libthinkterm_mobile-*.a | head -1)
    cp "$newest" "$out"
  fi
  test -f "$out" || { echo "no static library at $out" >&2; exit 1; }
}
rust_lib $SIM_TARGET
if [ "${1:-}" = xcode ]; then
  rust_lib $DEVICE_TARGET
fi

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
# The colour schemes the theme picker offers: the web page's list.
cp thinkterm-web/www/schemes.json ios/Resources/

echo "== project"
touch ios/Local.xcconfig
(cd ios && xcodegen generate --quiet)
if [ "${1:-}" = xcode ]; then
  open ios/ThinkTerm.xcodeproj
  exit 0
fi

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
