#!/bin/sh
# Build the iOS app for the simulator and, optionally, run it.
#
#   ios/build.sh            build the Rust library, bindings, project and app
#   ios/build.sh run        ...and boot a simulator, install, launch
#   ios/build.sh shot FILE  ...and save a screenshot to FILE after launch
#   ios/build.sh xcode      build the Rust library for a real iPhone as well,
#                           generate the project, and open it in Xcode: pick
#                           your phone and your team there and press Run.
#   ios/build.sh archive VERSION
#                           build for a real iPhone as that ThinkTerm release
#                           (0.5.1, or 0.5.1.2 for a fix to the app alone),
#                           archive it, and export it signed for App Store
#                           Connect; UPLOAD=1 also sends it there, for
#                           TestFlight. Signing is Xcode's own: the account
#                           signed in to Xcode, or an App Store Connect API
#                           key in ASC_KEY_PATH, ASC_KEY_ID and ASC_ISSUER_ID.
#
# A signing team for the phone goes in ios/Local.xcconfig (not tracked):
#   DEVELOPMENT_TEAM = ABCDE12345
# or is chosen in Xcode's Signing & Capabilities; the project is generated
# again by every build, so the file is the setting that lasts.
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD
MODE=${1:-}
SIM_TARGET=aarch64-apple-ios-sim
DEVICE_TARGET=aarch64-apple-ios
SIM_NAME=${SIM_NAME:-iPhone 17}
DERIVED=${DERIVED:-$ROOT/target/xcode}
BUNDLE=com.roversx.thinkterm.ios

# The release an archive is, and its build number: the Android app's
# version code (major, minor and patch, two digits each but the first, and
# a last two for a fix to the app alone), so a fix sorts after its release
# and before the next. App Store Connect takes three numbers as the
# version, so a fix keeps its release's and only the build number moves.
if [ "$MODE" = archive ]; then
  VERSION=${2:?usage: ios/build.sh archive VERSION}
  echo "$VERSION" | grep -Eq '^[0-9]+\.[0-9]{1,2}\.[0-9]{1,2}(\.[0-9]{1,2})?$' \
    || { echo "VERSION is major.minor.patch, or .fix after it: $VERSION" >&2; exit 1; }
  MARKETING=$(echo "$VERSION" | cut -d. -f1-3)
  BUILD=$(echo "$VERSION" | awk -F. '{ print $1 * 1000000 + $2 * 10000 + $3 * 100 + ($4 == "" ? 0 : $4) }')
fi

# Only the static library: the crate's cdylib is for Android, and its
# link fails on the iPhone target. The deployment target keeps rustc and
# ring's C objects on the same iOS version as the app.
export IPHONEOS_DEPLOYMENT_TARGET=26.0
# The paths rustc writes into the library (panic messages, debug info) as
# ~ and . rather than this machine's: the app goes to other people. Only
# for the iPhone targets (with --target, RUSTFLAGS leave build scripts and
# proc macros alone), so the workspace's own builds keep their cache.
remap=$(printf '%s\037%s' "--remap-path-prefix=$HOME=~" "--remap-path-prefix=$ROOT=.")
# cargo rustc with a crate type does not always leave the library at
# target/<triple>/release/ (the path the project links); the copy in
# deps/ is put there when it is missing.
rust_lib() {
  echo "== rust ($1, release)"
  CARGO_ENCODED_RUSTFLAGS=$remap \
    cargo rustc -p thinkterm-mobile --lib --release --target "$1" --crate-type staticlib
  out=target/$1/release/libthinkterm_mobile.a
  if [ ! -f "$out" ]; then
    newest=$(ls -t target/$1/release/deps/libthinkterm_mobile-*.a | head -1)
    cp "$newest" "$out"
  fi
  test -f "$out" || { echo "no static library at $out" >&2; exit 1; }
}
# An archive is for the phone alone; the bindings come from whichever
# library was built.
BINDINGS_TARGET=$SIM_TARGET
if [ "$MODE" = archive ]; then
  rust_lib $DEVICE_TARGET
  BINDINGS_TARGET=$DEVICE_TARGET
else
  rust_lib $SIM_TARGET
  if [ "$MODE" = xcode ]; then
    rust_lib $DEVICE_TARGET
  fi
fi

echo "== bindings"
rm -rf ios/Generated && mkdir -p ios/Generated
cargo run -q -p thinkterm-mobile --bin uniffi-bindgen -- generate \
  --library target/$BINDINGS_TARGET/release/libthinkterm_mobile.a \
  --language swift --out-dir ios/Generated
# Swift finds a module by a `module.modulemap` on its include path.
mv ios/Generated/thinkterm_mobileFFI.modulemap ios/Generated/module.modulemap

echo "== icons"
# The desktop's icons (third_party/lucide, as wezterm-gui/src/termwindow/ui/
# icons.rs names them), as template images the app tints: made here, not
# kept twice in the tree. Xcode's catalogs do not read `currentColor`.
icons=ios/Generated/Lucide.xcassets
mkdir -p "$icons"
printf '{"info":{"author":"xcode","version":1}}\n' > "$icons/Contents.json"
for name in app-window archive chevron-left chevron-right chevrons-up-down circle-plus folder \
  folder-open globe grid-2x2 key-round layers link-2 rotate-ccw search server \
  settings square-pen x hand monitor-smartphone triangle-alert eye; do
  set="$icons/lucide-$name.imageset"
  mkdir -p "$set"
  sed 's/stroke="currentColor"/stroke="#000000"/' "third_party/lucide/icons/$name.svg" > "$set/$name.svg"
  printf '{"images":[{"filename":"%s.svg","idiom":"universal"}],"info":{"author":"xcode","version":1},"properties":{"preserves-vector-representation":true,"template-rendering-intent":"template"}}\n' "$name" > "$set/Contents.json"
done
# The desktop's own "new project", which is not one of Lucide's.
set="$icons/lucide-project-add.imageset"
mkdir -p "$set"
sed 's/stroke="currentColor"/stroke="#000000"/' assets/ui/project-add.svg > "$set/project-add.svg"
printf '{"images":[{"filename":"project-add.svg","idiom":"universal"}],"info":{"author":"xcode","version":1},"properties":{"preserves-vector-representation":true,"template-rendering-intent":"template"}}\n' > "$set/Contents.json"
# The makers' marks a tailnet's machines wear that the system's symbols do
# not have (third_party/simple-icons, as the desktop's icons.rs has them):
# one filled shape each, tinted the same. Apple's is the system's own.
for name in android linux ubuntu debian raspberrypi fedora archlinux alpinelinux redhat centos nixos proxmox; do
  set="$icons/brand-$name.imageset"
  mkdir -p "$set"
  cp "third_party/simple-icons/icons/$name.svg" "$set/$name.svg"
  printf '{"images":[{"filename":"%s.svg","idiom":"universal"}],"info":{"author":"xcode","version":1},"properties":{"preserves-vector-representation":true,"template-rendering-intent":"template"}}\n' "$name" > "$set/Contents.json"
done

# About's own pictures, as the desktop's About page has them: the app's
# icon at its head, and the maker's mark (white on clear, so a template the
# page tints) at its foot.
set="$icons/app-icon.imageset"
mkdir -p "$set"
cp assets/icon/ThinkTerm.png "$set/ThinkTerm.png"
printf '{"images":[{"filename":"ThinkTerm.png","idiom":"universal","scale":"3x"}],"info":{"author":"xcode","version":1}}\n' > "$set/Contents.json"
set="$icons/closex-logo.imageset"
mkdir -p "$set"
cp assets/icon/CloseX.svg "$set/CloseX.svg"
printf '{"images":[{"filename":"CloseX.svg","idiom":"universal"}],"info":{"author":"xcode","version":1},"properties":{"preserves-vector-representation":true,"template-rendering-intent":"template"}}\n' > "$set/Contents.json"

echo "== licenses"
# Settings, Open Source Licenses: every crate the phone's library links, and
# the bundle's fonts and icons.
python3 ios/tools/gen-licenses.py ios/Generated/licenses.json $DEVICE_TARGET

echo "== fonts"
mkdir -p ios/Resources
cp assets/fonts/JetBrainsMono-Regular.ttf assets/fonts/FiraCode-Regular.ttf assets/fonts/SymbolsNerdFontMono-Regular.ttf ios/Resources/
# The colour schemes the theme picker offers: the web page's list.
cp thinkterm-web/www/schemes.json ios/Resources/

echo "== project"
touch ios/Local.xcconfig
(cd ios && xcodegen generate --quiet)
if [ "$MODE" = xcode ]; then
  open ios/ThinkTerm.xcodeproj
  exit 0
fi

if [ "$MODE" = archive ]; then
  echo "== archive ($MARKETING, build $BUILD)"
  ARCHIVE=$DERIVED/ThinkTerm-$VERSION.xcarchive
  EXPORT=$DERIVED/export-$VERSION
  rm -rf "$ARCHIVE" "$EXPORT"
  set --
  if [ -n "${ASC_KEY_PATH:-}" ]; then
    set -- -authenticationKeyPath "$ASC_KEY_PATH" \
      -authenticationKeyID "${ASC_KEY_ID:?ASC_KEY_ID goes with ASC_KEY_PATH}" \
      -authenticationKeyIssuerID "${ASC_ISSUER_ID:?ASC_ISSUER_ID goes with ASC_KEY_PATH}"
  fi
  xcodebuild -project ios/ThinkTerm.xcodeproj -scheme ThinkTerm \
    -configuration Release -destination generic/platform=iOS \
    -derivedDataPath "$DERIVED" -archivePath "$ARCHIVE" \
    -allowProvisioningUpdates "$@" \
    MARKETING_VERSION="$MARKETING" CURRENT_PROJECT_VERSION="$BUILD" \
    archive -quiet
  # App Store Connect's distribution, signed by Xcode; the build number is
  # the one above, not one Xcode picks.
  destination=export
  [ "${UPLOAD:-}" = 1 ] && destination=upload
  options=$DERIVED/ExportOptions.plist
  cat > "$options" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>$destination</string>
  <key>signingStyle</key><string>automatic</string>
  <key>manageAppVersionAndBuildNumber</key><false/>
  <key>uploadSymbols</key><true/>
</dict>
</plist>
PLIST
  xcodebuild -exportArchive -archivePath "$ARCHIVE" -exportPath "$EXPORT" \
    -exportOptionsPlist "$options" -allowProvisioningUpdates "$@"
  echo "archive: $ARCHIVE"
  if [ "$destination" = upload ]; then
    echo "uploaded $MARKETING ($BUILD) to App Store Connect"
  else
    echo "app: $(ls "$EXPORT"/*.ipa)"
  fi
  exit 0
fi

echo "== xcodebuild"
build_log=$(mktemp "${TMPDIR:-/tmp}/thinkterm-ios-build.XXXXXX")
trap 'rm -f "$build_log"' EXIT
build_status=0
xcodebuild -project ios/ThinkTerm.xcodeproj -scheme ThinkTerm \
  -configuration Debug -sdk iphonesimulator \
  -destination "platform=iOS Simulator,name=$SIM_NAME" \
  -derivedDataPath "$DERIVED" build >"$build_log" 2>&1 || build_status=$?
if [ "$build_status" -ne 0 ]; then
  cat "$build_log"
  exit "$build_status"
fi
grep -E "error:|warning: .*Sources/|BUILD (SUCCEEDED|FAILED)" "$build_log" || true
rm -f "$build_log"
trap - EXIT
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
