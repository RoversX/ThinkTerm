#!/bin/bash

# Interactive front end for macOS packaging.  ci/deploy.sh does the real work;
# this picks a signing mode, verifies that mode's prerequisites before anything
# expensive runs, and reports what Gatekeeper makes of the result.

set -euo pipefail

cd "$(dirname "$0")/.."

. ci/macos-identity.sh

NOTARY_PROFILE=${MACOS_NOTARY_PROFILE:-thinkterm}

usage() {
  cat <<EOT
usage: ci/macos-package.sh [adhoc|developerid] [tag]

  adhoc         Self-signed.  Fast and offline, but Gatekeeper rejects the
                result everywhere except the machine that built it.
  developerid   Developer ID signature, notarization and stapling.  Needs
                network and an Apple Developer account; opens on any Mac.

Both arguments are optional -- you are prompted for whatever is missing.
EOT
}

MODE=${1:-}
TAG=${2:-${TAG_NAME:-}}

case "$MODE" in
  -h | --help)
    usage
    exit 0
    ;;
esac

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

EOT
  while [[ -z "$MODE" ]]; do
    printf "Select [1/2]: "
    read -r reply || { echo; exit 1; }
    case "$reply" in
      1) MODE=adhoc ;;
      2) MODE=developerid ;;
      *) echo "Enter 1 or 2." ;;
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

INFO_PLIST=assets/macos/ThinkTerm.app/Contents/Info.plist
bundle_version=$(plutil -extract CFBundleShortVersionString raw "$INFO_PLIST")

if [[ -z "$TAG" ]]; then
  # The tag names the archive, and ci/wezterm-homebrew-macos.rb.template builds
  # its download URL out of it, so it has to match the GitHub release tag byte
  # for byte.  The v prefix is not decoration: .github/workflows/release.yml
  # only triggers on 'v*', so every release this has to line up with has one.
  # Deliberately not ci/tag-name.sh's build timestamp -- that identifies a build
  # rather than a release, and `thinkterm --version` already reports it.
  suggested="v$bundle_version"
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

echo "==> Checking prerequisites"

# Catch a missing build here rather than letting deploy.sh fail halfway through
# assembling a bundle it cannot populate.
missing=
for bin in wezterm thinkterm thinkterm-mux-server thinkterm-gui strip-ansi-escapes; do
  [[ -f "target/release/$bin" ]] || missing="$missing $bin"
done
if [[ -n "$missing" ]]; then
  echo "Missing release binaries:$missing" >&2
  echo "Build them first:  cargo build --release" >&2
  exit 1
fi
echo "    release binaries: present"

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

# The tag keeps its v so it matches the GitHub release, but the filename drops
# it. That is the same split the workflow makes -- release.yml derives
# version="${GITHUB_REF_NAME#v}" and names every Linux and Windows artifact
# from it -- and matching here is what keeps one release page from carrying
# both ThinkTerm-macos-v0.1.0.zip and thinkterm-0.1.0.Ubuntu22.04.deb. The v
# has to go for deb and rpm regardless: their version fields reject it.
version="${TAG#v}"

echo
echo "==> Packaging as $TAG in $MODE mode"
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
