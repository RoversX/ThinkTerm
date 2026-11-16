#!/bin/sh
# Build the Android app and, optionally, run it on an emulator.
#
#   android/build.sh            build the Rust library, bindings, assets and APK
#   android/build.sh run        ...and boot the emulator, install, launch
#   android/build.sh shot FILE  ...and save a screenshot to FILE after launch
#   android/build.sh release [VERSION]
#                               a release APK instead, for desktop release
#                               VERSION (e.g. 0.5.0; "dev" without one)
#
# A release APK is signed when THINKTERM_ANDROID_KEYSTORE names a PKCS12
# keystore, with THINKTERM_ANDROID_KEYSTORE_PASSWORD and
# THINKTERM_ANDROID_KEY_ALIAS beside it; without one it stays unsigned.
#
# Needs: the Android SDK at ~/Library/Android/sdk with an NDK, the
# aarch64-linux-android Rust target, cargo-ndk, and a JDK (Android Studio's).
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD
export ANDROID_HOME=${ANDROID_HOME:-$HOME/Library/Android/sdk}
export ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}
# Gradle 8.14 does not run on JDK 25 (Android Studio's); a 17 does.
export JAVA_HOME=${JAVA_HOME:-$(/usr/libexec/java_home -v 17 2>/dev/null || echo "/Applications/Android Studio.app/Contents/jbr/Contents/Home")}
GRADLE=${GRADLE:-$(command -v gradle || echo "$ROOT/android/gradle-dist/bin/gradle")}
AVD=${AVD:-ThinkTerm36}
BUNDLE=com.roversx.thinkterm
TARGET=aarch64-linux-android
MODE=${1:-}
RUST_TARGET_DIR=${CARGO_TARGET_DIR:-target}
JNILIBS=android/app/src/main/jniLibs
if [ "$MODE" = release ]; then
  VERSION=${2:-dev}
  # It becomes a filename. Same character rule as the release workflow's.
  case "$VERSION" in
    *[!A-Za-z0-9._-]*) echo "refusing version '$VERSION'" >&2; exit 1 ;;
  esac
  # A release builds with flags of its own (below), so it gets its own
  # target directory and its own copy of the library: sharing either with
  # the debug build would rebuild the whole tree whenever the two alternate.
  RUST_TARGET_DIR=$RUST_TARGET_DIR/android-release
  JNILIBS=android/build/rust-release/jniLibs
fi

echo "== rust ($TARGET, release) via cargo ndk ($ANDROID_NDK_HOME)"
if [ "$MODE" = release ]; then
  # Panic messages carry source paths, which would name the build machine's
  # home directory; written relative to it and to the checkout instead.
  # Encoded, so a path with a space stays one flag.
  CARGO_TARGET_DIR=$RUST_TARGET_DIR \
    CARGO_ENCODED_RUSTFLAGS=$(printf '%s\037%s' "--remap-path-prefix=$HOME=~" "--remap-path-prefix=$ROOT=.") \
    cargo ndk -t arm64-v8a -o "$JNILIBS" build -p thinkterm-mobile --release --lib
else
  cargo ndk -t arm64-v8a -o "$JNILIBS" build -p thinkterm-mobile --release --lib
fi
# cargo ndk copies every cdylib it built; the web crate's is for wasm.
rm -f "$JNILIBS/arm64-v8a/libthinkterm_web.so"

echo "== bindings"
rm -rf android/app/src/main/java/com/roversx/thinkterm/core
cargo run -q -p thinkterm-mobile --bin uniffi-bindgen -- generate \
  --library "$RUST_TARGET_DIR/$TARGET/release/libthinkterm_mobile.so" \
  --language kotlin --out-dir android/app/src/main/java
if [ "$MODE" = release ]; then
  # The symbol table is half the library and only reads crashes; the
  # bindings above came from the unstripped build, whose symbols carry
  # UniFFI's metadata.
  "$(ls "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-strip | head -1)" \
    --strip-all "$JNILIBS/arm64-v8a/libthinkterm_mobile.so"
fi

echo "== assets"
mkdir -p android/app/src/main/assets
# The desktop's icons (third_party/lucide, as wezterm-gui's icons.rs names
# them), read as vectors by DesktopIcons.kt: copied here, not kept twice in
# the tree. The same list as ios/build.sh's.
rm -rf android/app/src/main/assets/lucide
mkdir -p android/app/src/main/assets/lucide
for name in app-window archive bot check hand laptop-minimal monitor-smartphone smartphone chevron-down circle-check clipboard copy lock shield-off triangle-alert chevron-left panel-left panel-right chevron-right chevrons-up-down circle-plus folder \
  folder-open globe grid-2x2 key-round keyboard layers link-2 paperclip plus rotate-ccw search server \
  settings square-pen x cloud laptop hard-drive monitor cpu package square-terminal power \
  palette panels-top-left wifi info stethoscope git-branch scale arrow-up-right; do
  [ -f "third_party/lucide/icons/$name.svg" ] && cp "third_party/lucide/icons/$name.svg" android/app/src/main/assets/lucide/
done
# The desktop's own "new project", which is not one of Lucide's.
cp assets/ui/project-add.svg android/app/src/main/assets/lucide/
# The makers' marks a tailnet's machines wear (third_party/simple-icons, as
# the desktop's icons.rs has them), filled, not stroked. ios/build.sh's list
# leaves out Apple's, which iOS has as a symbol of its own.
rm -rf android/app/src/main/assets/brands
mkdir -p android/app/src/main/assets/brands
for name in apple android linux ubuntu debian raspberrypi fedora archlinux alpinelinux redhat centos nixos proxmox; do
  cp "third_party/simple-icons/icons/$name.svg" android/app/src/main/assets/brands/
done
# About's own pictures, as the desktop's About page has them: the maker's
# mark (filled, like the marks above) and the app's icon.
cp assets/icon/CloseX.svg android/app/src/main/assets/brands/closex.svg
cp assets/icon/ThinkTerm.png android/app/src/main/assets/app_icon.png
cp assets/fonts/JetBrainsMono-Regular.ttf assets/fonts/FiraCode-Regular.ttf assets/fonts/SymbolsNerdFontMono-Regular.ttf android/app/src/main/assets/
cp thinkterm-web/www/schemes.json ios/Resources/strings.json android/app/src/main/assets/
# The probe sshd's throwaway key, for the emulator's "This Mac" entry:
# a debug asset, so no release build carries it.
mkdir -p android/app/src/debug/assets
[ -f /tmp/ttp-ssh/userkey ] && cp /tmp/ttp-ssh/userkey android/app/src/debug/assets/probe_key || true
# The app reads the probe's login at runtime. Override it when the SSH
# account differs from the local account; this asset is debug-only.
printf '%s\n' "${THINKTERM_PROBE_USER:-$(id -un)}" > android/app/src/debug/assets/probe_user

echo "== gradle"
build_log=$(mktemp "${TMPDIR:-/tmp}/thinkterm-android-build.XXXXXX")
trap 'rm -f "$build_log"' EXIT
build_status=0
if [ "$MODE" = release ]; then
  (cd android && "$GRADLE" --quiet assembleRelease \
    -PthinktermVersion="$VERSION" -PthinktermJniLibs="$ROOT/$JNILIBS") >"$build_log" 2>&1 || build_status=$?
else
  (cd android && "$GRADLE" --quiet assembleDebug) >"$build_log" 2>&1 || build_status=$?
fi
if [ "$build_status" -ne 0 ]; then
  cat "$build_log"
  exit "$build_status"
fi
grep -E "error|warning: unused|BUILD|FAIL" "$build_log" || true
rm -f "$build_log"
trap - EXIT

if [ "$MODE" = release ]; then
  UNSIGNED=android/app/build/outputs/apk/release/app-release-unsigned.apk
  test -f "$UNSIGNED" || { echo "no APK at $UNSIGNED" >&2; exit 1; }
  if [ -z "${THINKTERM_ANDROID_KEYSTORE:-}" ]; then
    echo "unsigned (THINKTERM_ANDROID_KEYSTORE is not set): $UNSIGNED"
    exit 0
  fi
  echo "== sign"
  BUILD_TOOLS=$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)
  APK=android/app/build/outputs/apk/release/ThinkTerm-android-arm64-$VERSION.apk
  # Aligned before signing, as apksigner needs: 16 KiB pages for the
  # uncompressed native libraries, which Android 15 and later may map so.
  "$BUILD_TOOLS/zipalign" -f -P 16 4 "$UNSIGNED" "$APK.aligned"
  # The password reaches apksigner through the environment, never argv.
  "$BUILD_TOOLS/apksigner" sign --ks "$THINKTERM_ANDROID_KEYSTORE" \
    --ks-key-alias "${THINKTERM_ANDROID_KEY_ALIAS:?THINKTERM_ANDROID_KEY_ALIAS is not set}" \
    --ks-pass env:THINKTERM_ANDROID_KEYSTORE_PASSWORD \
    --v4-signing-enabled false \
    --out "$APK" "$APK.aligned"
  rm -f "$APK.aligned"
  "$BUILD_TOOLS/apksigner" verify "$APK"
  echo "signed: $APK"
  exit 0
fi

APK="android/app/build/outputs/apk/debug/app-debug.apk"
test -f "$APK" || { echo "no APK at $APK" >&2; exit 1; }

[ "${1:-}" = run ] || [ "${1:-}" = shot ] || exit 0

echo "== emulator"
ADB="$ANDROID_HOME/platform-tools/adb"
if ! "$ADB" devices | grep -q "emulator-.*device"; then
  "$ANDROID_HOME/emulator/emulator" -avd "$AVD" -no-snapshot-load -no-boot-anim -no-audio >/dev/null 2>&1 &
  "$ADB" wait-for-device
  until [ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ]; do sleep 2; done
fi
"$ADB" install -r "$APK" >/dev/null
"$ADB" shell am force-stop $BUNDLE
"$ADB" shell am start -n $BUNDLE/.MainActivity ${LAUNCH_ARGS:-} >/dev/null
if [ "${1:-}" = shot ]; then
  sleep "${SHOT_DELAY:-6}"
  "$ADB" exec-out screencap -p > "$2"
  echo "screenshot: $2"
fi
