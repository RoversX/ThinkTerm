#!/bin/bash

set -euo pipefail

APP_PATH=${1:?usage: macos-sign-local.sh path/to/ThinkTerm.app [adhoc|development]}
SIGNING_MODE=${2:-adhoc}

# This is the active Individual team used by the developer's current macOS
# applications.  It is public signing metadata, not a credential.  Override it
# when intentionally testing with another team.
DEVELOPMENT_TEAM=${THINKTERM_MACOS_DEVELOPMENT_TEAM:-D4KD8XCCL6}

case "$SIGNING_MODE" in
  adhoc)
    SIGNING_IDENTITY=-
    ;;
  development)
    SIGNING_IDENTITY=${MACOS_SIGNING_IDENTITY:-}
    if [[ -z "$SIGNING_IDENTITY" ]]; then
      SIGNING_IDENTITY=$(
        security find-identity -v -p codesigning |
          sed -n "s/.*\"\(Apple Development:.*(${DEVELOPMENT_TEAM})\)\".*/\1/p" |
          head -n 1
      )
    fi
    if [[ -z "$SIGNING_IDENTITY" ]]; then
      echo "No Apple Development identity found for team ${DEVELOPMENT_TEAM}." >&2
      echo "Create or download it in Xcode > Settings > Accounts > Manage Certificates." >&2
      exit 1
    fi
    ;;
  *)
    echo "Unsupported signing mode: ${SIGNING_MODE} (expected adhoc or development)" >&2
    exit 2
    ;;
esac

if [[ ! -d "$APP_PATH" ]]; then
  echo "App bundle not found: ${APP_PATH}" >&2
  exit 1
fi

# Sign nested executables first, then seal the outer bundle.  This avoids
# relying on codesign --deep to guess which bundled files are code.  Local
# builds can link Homebrew dylibs, so these development-only modes deliberately
# omit Hardened Runtime/library validation.  The Developer ID release path in
# deploy.sh continues to enable Hardened Runtime before notarization.
while IFS= read -r executable; do
  codesign --force --sign "$SIGNING_IDENTITY" "$executable"
done < <(find "$APP_PATH/Contents/MacOS" -type f -perm -111 -print | sort)

codesign --force \
  --entitlements ci/macos-entitlement.plist \
  --sign "$SIGNING_IDENTITY" \
  "$APP_PATH"

codesign --verify --deep --strict --verbose=2 "$APP_PATH"

if [[ "$SIGNING_MODE" == adhoc ]]; then
  echo "Ad-hoc signed ${APP_PATH}; TeamIdentifier is intentionally unset."
  echo "Configured development team for the next stage: ${DEVELOPMENT_TEAM}"
else
  echo "Development-signed ${APP_PATH} with ${SIGNING_IDENTITY}"
fi
