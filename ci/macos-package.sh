#!/bin/bash

# Interactive front end for macOS packaging.  ci/deploy.sh does the real work;
# this picks a signing mode, verifies that mode's prerequisites before anything
# expensive runs, and reports what Gatekeeper makes of the result.

set -euo pipefail

# Resolved before the cd, because --arch both re-runs this script once per
# architecture and $0 is relative for most of the ways it gets invoked.
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

cd "$(dirname "$0")/.."

. ci/macos-identity.sh

NOTARY_PROFILE=${MACOS_NOTARY_PROFILE:-thinkterm}

usage() {
  cat <<EOT
usage: ci/macos-package.sh [--build] [--arch arm64|x86_64|both]
                           [--upload] [adhoc|developerid] [tag]

  --build       Compile the binaries before packaging.
  --arch        Which Mac the package is for.  Defaults to this one; the
                other is cross-compiled, which needs its standard library
                (\`rustup target add x86_64-apple-darwin\`).  \`both\` runs
                the whole thing twice, which is what a release wants.
  --upload      Attach the finished archives to the release named by the
                tag, which has to exist already -- the draft the release
                workflow leaves behind.
  adhoc         Self-signed.  Fast and offline, but Gatekeeper rejects the
                result everywhere except the machine that built it.
  developerid   Developer ID signature, notarization and stapling.  Needs
                network and an Apple Developer account; opens on any Mac.

Every argument is optional -- you are prompted for whatever is missing.
EOT
}

# --build is orthogonal to the signing mode, so it is a flag rather than a
# third mode.  ci/deploy.sh only copies whatever target/release already holds,
# which makes the package exactly as old as the last build -- and a partial
# \`cargo build -p wezterm-gui\` refreshes two of the five binaries and leaves a
# bundle whose parts come from different commits.
BUILD=no
ARCH=
UPLOAD=no
while [[ $# -gt 0 ]]; do
  case "$1" in
    --build)
      BUILD=yes
      shift
      ;;
    --arch)
      ARCH=${2:-}
      shift 2 || { usage >&2; exit 2; }
      ;;
    --upload)
      UPLOAD=yes
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      break
      ;;
  esac
done

MODE=${1:-}
TAG=${2:-${TAG_NAME:-}}

if [[ -z "$MODE" ]]; then
  cat <<'EOT'
ThinkTerm macOS packaging

  1) Ad-hoc self-signed
     Fast, no network, no Apple account needed.
     Runs on this Mac only -- Gatekeeper rejects it everywhere else.

  2) Developer ID + notarized
     Signs with your Developer ID certificate, submits to Apple, staples
     the ticket into the bundle.  Uploads ~60MB and takes a few minutes.
     Opens on any Mac with no warning, and needs no network to verify.
     Packages the binaries already in target/release, whatever their age.

  3) Build, then Developer ID + notarized
     Everything option 2 does, but compiles all five binaries first so the
     bundle cannot carry a mix of commits.  This is the one to use for a
     release.

EOT
  while [[ -z "$MODE" ]]; do
    printf "Select [1/2/3]: "
    read -r reply || { echo; exit 1; }
    case "$reply" in
      1) MODE=adhoc ;;
      2) MODE=developerid ;;
      3)
        MODE=developerid
        BUILD=yes
        ;;
      *) echo "Enter 1, 2 or 3." ;;
    esac
  done
  echo
fi

if [[ "$MODE" != adhoc && "$MODE" != developerid ]]; then
  echo "Unsupported mode: $MODE" >&2
  echo >&2
  usage >&2
  exit 2
fi

# One package holds one architecture.  A universal binary would be the other
# way to cover both Macs, but it doubles a 66MB download for everyone to spare
# the smaller half of the audience a choice, and the release page has to name
# the two anyway.
HOST_ARCH=$(uname -m)
arm_note=
intel_note=
if [[ "$HOST_ARCH" == arm64 ]]; then
  arm_note='  -- this Mac'
  default_choice=1
else
  intel_note='  -- this Mac'
  default_choice=2
fi

if [[ -z "$ARCH" ]]; then
  cat <<EOT
Architecture

  1) Apple silicon (arm64)$arm_note
  2) Intel (x86_64)$intel_note
  3) Both -- what a release needs

EOT
  while [[ -z "$ARCH" ]]; do
    printf "Select [1/2/3, default %s]: " "$default_choice"
    read -r reply || { echo; exit 1; }
    case "${reply:-$default_choice}" in
      1) ARCH=arm64 ;;
      2) ARCH=x86_64 ;;
      3) ARCH=both ;;
      *) echo "Enter 1, 2 or 3." ;;
    esac
  done
  echo
fi

case "$ARCH" in
  arm64 | x86_64 | both) ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    echo >&2
    usage >&2
    exit 2
    ;;
esac

INFO_PLIST=assets/macos/ThinkTerm.app/Contents/Info.plist
bundle_version=$(plutil -extract CFBundleShortVersionString raw "$INFO_PLIST")

if [[ -z "$TAG" ]]; then
  # The tag names the archive, and ci/wezterm-homebrew-macos.rb.template builds
  # its download URL out of it, so it has to match the GitHub release tag byte
  # for byte -- and no v anywhere, because deb and rpm version fields reject
  # one and Homebrew wants a bare version.
  # Deliberately not ci/tag-name.sh's build timestamp -- that identifies a build
  # rather than a release, and `thinkterm --version` already reports it.
  suggested="$bundle_version"
  printf "Version tag [%s]: " "$suggested"
  read -r TAG || { echo; exit 1; }
  TAG=${TAG:-$suggested}
  echo
fi

# The tag becomes a filename inside ci/deploy.sh, which builds paths from it and
# passes them to rm -rf.  Whitespace and glob characters are refused here rather
# than relied upon being quoted at every later use.
if [[ ! "$TAG" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "Refusing tag: $TAG" >&2
  echo "Use only letters, digits, dot, underscore and dash." >&2
  exit 2
fi

# The bundle's version is what the shipped app reports to the update check, and
# the tag is what it gets compared against.  A mismatch ships an app that
# announces an update to the version it already is, on every launch, because
# installing that update cannot change what the binary claims about itself.
if [[ "${TAG#v}" != "$bundle_version" ]]; then
  echo "Tag $TAG does not match the bundle version $bundle_version." >&2
  echo "Bump CFBundleShortVersionString in $INFO_PLIST first," >&2
  echo "or package the version the bundle already declares." >&2
  exit 2
fi

# Both is one run per architecture rather than one pass producing two, so that
# half a release and a single-architecture run go through the same code. The
# tag is resolved above first, so the second run does not prompt for it again.
if [[ "$ARCH" == both ]]; then
  extra=()
  if [[ "$BUILD" == yes ]]; then extra+=(--build); fi
  if [[ "$UPLOAD" == yes ]]; then extra+=(--upload); fi
  for one in arm64 x86_64; do
    echo
    echo "################  $one  ################"
    "$SELF" ${extra[@]+"${extra[@]}"} --arch "$one" "$MODE" "$TAG"
  done
  echo
  echo "==> Both archives"
  for one in arm64 x86_64; do
    echo "    $PWD/ThinkTerm-macos-$one-${TAG#v}.zip"
  done
  exit 0
fi

case "$ARCH" in
  arm64) RUST_TARGET=aarch64-apple-darwin ;;
  x86_64) RUST_TARGET=x86_64-apple-darwin ;;
esac

# ci/deploy.sh reads this for both the archive name and which target directory
# to take the binaries from.
export MACOS_ARCH="$ARCH"
BIN_DIR="target/$RUST_TARGET/release"

# `rustc` on PATH is often the one Homebrew installed, and a toolchain from a
# package manager carries only its own host's standard library: a cross build
# against it dies in the first dependency with E0463.  rustup's has both, so
# prefer it whenever there is one -- through rustup's proxies, not the
# toolchain's own bin directory: run bare, rust-lld and rust-objcopy cannot
# find libLLVM.dylib (the proxies set the dyld fallback path) and the wasm
# link dies with SIGABRT.
if rustup_bin=$(command -v rustup 2>/dev/null) &&
  [[ -x "$(dirname "$rustup_bin")/cargo" ]]; then
  PATH="$(dirname "$rustup_bin"):$PATH"
fi

echo "==> Checking prerequisites"

# The certificate and the notary round trip come first: they are cheap, and
# failing them after a fifteen minute --build would be the worst possible
# ordering.
if [[ "$MODE" == developerid ]]; then
  identity=$(resolve_signing_identity "Developer ID Application")
  team=$(team_id_from_identity "$identity")
  echo "    certificate: $identity"

  # One cheap round trip now beats discovering the credentials are missing
  # after a 60MB upload.
  if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
    cat >&2 <<EOT

No usable notarization credentials in keychain profile '$NOTARY_PROFILE'.

Create an app-specific password at https://account.apple.com
(Sign-In and Security > App-Specific Passwords), then store it once:

  xcrun notarytool store-credentials $NOTARY_PROFILE \\
    --apple-id YOUR_APPLE_ID \\
    --team-id $team \\
    --password xxxx-xxxx-xxxx-xxxx
EOT
    exit 1
  fi
  echo "    notarization credentials: profile '$NOTARY_PROFILE' works"
fi

BINARIES="wezterm thinkterm thinkterm-mux-server thinkterm-gui strip-ansi-escapes"

if [[ "$BUILD" == yes ]]; then
  if ! rustup target list --installed 2>/dev/null | grep -qx "$RUST_TARGET"; then
    echo "No standard library for $RUST_TARGET." >&2
    echo "Install it with: rustup target add $RUST_TARGET" >&2
    exit 1
  fi
  echo "    toolchain: $(cargo --version) at $(command -v cargo)"

  echo
  echo "==> Building for $ARCH"
  # The same four packages the release workflow builds, which between them
  # produce all five binaries the bundle carries.  Building the whole
  # workspace instead would compile crates no package ships.
  cargo build --release --target "$RUST_TARGET" \
    -p wezterm \
    -p wezterm-gui \
    -p wezterm-mux-server \
    -p strip-ansi-escapes
  echo
  echo "==> Building the browser client"
  # Into thinkterm-web/www, where deploy.sh picks it up. Needs the wasm32
  # target and the wasm-bindgen CLI at the pinned version; the script says
  # which if either is missing.
  bash ci/build-web.sh
  echo
  echo "==> Checking prerequisites (continued)"
fi

# A package without the browser client has a server that accepts browser
# connections and serves no page. Say so here rather than ship it quietly.
if [[ ! -f thinkterm-web/www/pkg/thinkterm_web.js ]]; then
  echo "No browser client in thinkterm-web/www." >&2
  echo "Build it first with 'ci/build-web.sh', or re-run with --build." >&2
  exit 1
fi

# A plain `cargo build --release` writes to target/release with no target
# directory of its own, and for this Mac's own architecture that is the same
# set of binaries. Keep accepting it, so packaging what is already built does
# not force a second full build into a target-specific directory.
if [[ ! -f "$BIN_DIR/thinkterm-gui" && "$ARCH" == "$HOST_ARCH" &&
  -f target/release/thinkterm-gui ]]; then
  BIN_DIR=target/release
fi
export MACOS_BIN_DIR="$BIN_DIR"

# Catch a missing build here rather than letting deploy.sh fail halfway through
# assembling a bundle it cannot populate.
missing=
for bin in $BINARIES; do
  [[ -f "$BIN_DIR/$bin" ]] || missing="$missing $bin"
done
if [[ -n "$missing" ]]; then
  echo "Missing $ARCH binaries in $BIN_DIR:$missing" >&2
  echo "Build them first with 'cargo build --release --target $RUST_TARGET'," >&2
  echo "or re-run with --build to have this script do it." >&2
  exit 1
fi

if [[ "$BUILD" == yes ]]; then
  echo "    release binaries: just built"
else
  # deploy.sh copies each binary independently, so a bundle can quietly carry
  # binaries from different commits: a partial `cargo build -p wezterm-gui`
  # refreshes two of these five and leaves the other three behind, and the
  # result is an app whose --version disagrees with its own terminal code.
  # Anything older than the current commit gets named here rather than
  # discovered later.
  head_time=$(git log -1 --format=%ct 2>/dev/null || true)
  stale=
  if [[ -n "$head_time" ]]; then
    for bin in $BINARIES; do
      [[ "$(stat -f %m "$BIN_DIR/$bin")" -lt "$head_time" ]] &&
        stale="$stale $bin"
    done
  fi
  if [[ -n "$stale" ]]; then
    echo "    release binaries: PRESENT BUT OLDER THAN HEAD --$stale"
    echo "    HEAD is $(git log -1 --format='%h %s')"
    echo "    Re-run with --build, or with option 3, unless you know those"
    echo "    binaries do not depend on anything that has changed since."
  else
    echo "    release binaries: present, none older than HEAD"
  fi
fi

# A leading v is tolerated here and dropped, the same as the Run workflow form
# does, so that typing one out of habit cannot produce a release page carrying
# both ThinkTerm-macos-arm64-v0.1.0.zip and thinkterm-0.1.0.Ubuntu22.04.deb.
version="${TAG#v}"

echo
echo "==> Packaging $ARCH as $TAG in $MODE mode"
TAG_NAME="$version" MACOS_SIGNING_MODE="$MODE" bash ci/deploy.sh

# deploy.sh derives both names the same way; keep them in sync with it.
zipdir="ThinkTerm-macos${MACOS_ARCH:+-$MACOS_ARCH}-$version"
zipname="$zipdir.zip"
app="$zipdir/ThinkTerm.app"

echo
echo "==> Result"
codesign -dv --verbose=2 "$app" 2>&1 |
  grep -E "^Authority=Developer ID Application|^TeamIdentifier=|flags=" || true

# spctl exits non-zero for anything Gatekeeper refuses, which is the expected
# outcome in adhoc mode, so its verdict is reported rather than enforced.
verdict=$(spctl --assess --verbose=4 --type exec "$app" 2>&1 || true)
echo "$verdict" | sed 's/^/    /'

echo
if [[ "$MODE" == developerid ]]; then
  echo "$zipname is signed, notarized and stapled."
  echo "It opens on any Mac, and verifies without network access."
else
  echo "$zipname is ad-hoc signed -- this Mac only."
  echo "Re-run with 'developerid' to produce something distributable."
fi

echo
if [[ "$UPLOAD" == yes ]]; then
  if ! command -v gh >/dev/null 2>&1; then
    echo "gh is not installed, so $zipname has to be attached by hand." >&2
    exit 1
  fi
  echo "==> Attaching to release $TAG"
  # --clobber so that re-packaging after a fix replaces the archive rather
  # than failing because the name is already taken.
  gh release upload --clobber "$TAG" "$zipname"
else
  echo "Attach to the release:"
  echo "    $PWD/$zipname"
fi
