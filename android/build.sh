#!/bin/sh
# Build the Android app and, optionally, run it on an emulator.
#
#   android/build.sh            build the Rust library, bindings, assets and APK
#   android/build.sh run        ...and boot the emulator, install, launch
#   android/build.sh shot FILE  ...and save a screenshot to FILE after launch
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

echo "== rust ($TARGET, release) via cargo ndk ($ANDROID_NDK_HOME)"
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build -p thinkterm-mobile --release --lib
# cargo ndk copies every cdylib it built; the web crate's is for wasm.
rm -f android/app/src/main/jniLibs/arm64-v8a/libthinkterm_web.so

echo "== bindings"
rm -rf android/app/src/main/java/com/roversx/thinkterm/core
cargo run -q -p thinkterm-mobile --bin uniffi-bindgen -- generate \
  --library target/$TARGET/release/libthinkterm_mobile.so \
  --language kotlin --out-dir android/app/src/main/java

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
(cd android && "$GRADLE" --quiet assembleDebug) >"$build_log" 2>&1 || build_status=$?
if [ "$build_status" -ne 0 ]; then
  cat "$build_log"
  exit "$build_status"
fi
grep -E "error|warning: unused|BUILD|FAIL" "$build_log" || true
rm -f "$build_log"
trap - EXIT
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
