#!/bin/bash

set -euo pipefail

APP_PATH=${1:?usage: macos-sign-local.sh path/to/ThinkTerm.app [adhoc|development|developerid]}
SIGNING_MODE=${2:-adhoc}

# Extra codesign flags that only the distribution mode needs.  Kept as an array
# so the two codesign invocations below stay identical across modes.
CODESIGN_EXTRA=()

. "$(dirname "$0")/macos-identity.sh"

# Resolved against this script, not the caller's cwd: signing an already
# extracted bundle from some other directory is an obvious thing to want, and a
# cwd-relative path would fail only after every nested binary had been re-signed.
ENTITLEMENTS="$(dirname "$0")/macos-entitlement.plist"

case "$SIGNING_MODE" in
  adhoc)
    SIGNING_IDENTITY=-
    ;;
  development)
    SIGNING_IDENTITY=$(resolve_signing_identity "Apple Development")
    ;;
  developerid)
    SIGNING_IDENTITY=$(resolve_signing_identity "Developer ID Application")
    # Notarization rejects submissions that lack Hardened Runtime or a secure
    # timestamp, and neither is a codesign default.  Both have to be applied to
    # the nested executables as well as the outer bundle, so they live here
    # rather than only on the final seal.
    CODESIGN_EXTRA=(--options runtime --timestamp)
    ;;
  *)
    echo "Unsupported signing mode: ${SIGNING_MODE}" >&2
    echo "(expected adhoc, development or developerid)" >&2
    exit 2
    ;;
esac

if [[ ! -d "$APP_PATH" ]]; then
  echo "App bundle not found: ${APP_PATH}" >&2
  exit 1
fi

# Sign nested executables first, then seal the outer bundle.  This avoids
# relying on codesign --deep to guess which bundled files are code.  Local
# builds can link Homebrew dylibs, so adhoc and development deliberately omit
# Hardened Runtime/library validation; developerid turns it back on through
# CODESIGN_EXTRA above, because notarization refuses anything without it.
# The ${arr[@]+"${arr[@]}"} dance is not decoration: /bin/bash on macOS is 3.2,
# where `set -u` treats an empty array expansion as an unbound variable.
#
# -perm +111 matches any execute bit.  -perm -111 would demand all three, and
# ci/deploy.sh copies these binaries with plain cp, so a caller running under a
# restrictive umask gets 0700 files, matches nothing, and silently ships a
# bundle whose nested binaries were never signed.
#
# The entitlements are deliberately applied to every binary, not just the outer
# seal: only CFBundleExecutable inherits the bundle's entitlements, so the mux
# server and CLIs would otherwise run under Hardened Runtime with an empty set
# and be denied by TCC in developerid builds alone.
while IFS= read -r executable; do
  codesign --force ${CODESIGN_EXTRA[@]+"${CODESIGN_EXTRA[@]}"} \
    --entitlements "$ENTITLEMENTS" \
    --sign "$SIGNING_IDENTITY" "$executable"
done < <(find "$APP_PATH/Contents/MacOS" -type f -perm +111 -print | sort)

codesign --force ${CODESIGN_EXTRA[@]+"${CODESIGN_EXTRA[@]}"} \
  --entitlements "$ENTITLEMENTS" \
  --sign "$SIGNING_IDENTITY" \
  "$APP_PATH"

codesign --verify --deep --strict --verbose=2 "$APP_PATH"

case "$SIGNING_MODE" in
  adhoc)
    echo "Ad-hoc signed ${APP_PATH}; TeamIdentifier is intentionally unset."
    ;;
  development)
    echo "Development-signed ${APP_PATH} with ${SIGNING_IDENTITY}"
    ;;
  developerid)
    echo "Developer ID signed ${APP_PATH} with ${SIGNING_IDENTITY}"
    echo "Hardened Runtime is on, so Gatekeeper will still reject this until it"
    echo "has been notarized and stapled -- see ci/macos-notarize-local.sh."
    ;;
esac
