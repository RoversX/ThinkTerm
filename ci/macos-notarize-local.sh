#!/bin/bash

set -euo pipefail

APP_PATH=${1:?usage: macos-notarize-local.sh path/to/ThinkTerm.app [keychain-profile]}
PROFILE=${2:-${MACOS_NOTARY_PROFILE:-thinkterm}}

# Locally the credentials live in a keychain profile created once with
# `notarytool store-credentials`.  A CI runner has no such keychain, so prefer
# the credentials passed in the environment whenever all three are present.
if [[ -n "${MACOS_APPLEID:-}" && -n "${MACOS_APP_PW:-}" && -n "${MACOS_TEAM_ID:-}" ]]; then
  NOTARY_AUTH=(--apple-id "$MACOS_APPLEID" --password "$MACOS_APP_PW" --team-id "$MACOS_TEAM_ID")
  echo "==> Using notarization credentials from the environment"
else
  NOTARY_AUTH=(--keychain-profile "$PROFILE")
  echo "==> Using notarization credentials from keychain profile '$PROFILE'"
fi

if [[ ! -d "$APP_PATH" ]]; then
  echo "App bundle not found: ${APP_PATH}" >&2
  exit 1
fi

# Notarization only accepts a container, never a bare .app, and it has to be
# built with ditto: plain `zip` drops the symlinks and extended attributes that
# codesign relies on, which Apple rejects as a damaged bundle.
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
SUBMISSION_ZIP="$WORK_DIR/submission.zip"
RESULT_PLIST="$WORK_DIR/result.plist"

echo "==> Packing $APP_PATH for submission"
/usr/bin/ditto -c -k --keepParent "$APP_PATH" "$SUBMISSION_ZIP"

echo "==> Submitting to Apple (this usually takes 1-5 minutes)"
xcrun notarytool submit "$SUBMISSION_ZIP" \
  "${NOTARY_AUTH[@]}" \
  --wait \
  --output-format plist >"$RESULT_PLIST"

status=$(plutil -extract status raw -o - "$RESULT_PLIST")
submission_id=$(plutil -extract id raw -o - "$RESULT_PLIST")
echo "==> Submission $submission_id finished with status: $status"

if [[ "$status" != "Accepted" ]]; then
  # The submit output only says Invalid; the per-issue reasons live in the log,
  # which is the only thing that makes a rejection actionable.
  echo "==> Notarization failed, fetching the log" >&2
  xcrun notarytool log "$submission_id" "${NOTARY_AUTH[@]}" >&2 || true
  exit 1
fi

# Apple recorded the result server-side; stapling copies that ticket into the
# bundle so first launch does not depend on reaching Apple over the network.
echo "==> Stapling the ticket into the bundle"
# notarytool reports Accepted the moment the verdict is recorded, but the
# ticket needs a few more seconds to become downloadable.  stapler surfaces
# that window as "file does not exist" naming the .app -- it means the ticket,
# not the bundle -- so retry instead of failing the build on a race.
staple_attempts=6
attempt=1
while true; do
  if xcrun stapler staple "$APP_PATH"; then
    break
  fi
  if [[ "$attempt" -ge "$staple_attempts" ]]; then
    echo "==> Ticket still not available after ${staple_attempts} attempts" >&2
    exit 1
  fi
  echo "==> Ticket not published yet, retrying in 15s (attempt ${attempt}/${staple_attempts})"
  sleep 15
  attempt=$((attempt + 1))
done

echo "==> Verifying"
xcrun stapler validate "$APP_PATH"
codesign --verify --deep --strict --verbose=2 "$APP_PATH"
spctl --assess --verbose=4 --type exec "$APP_PATH"

echo
echo "Notarized and stapled: $APP_PATH"
echo "Re-package it now -- the bundle gained a ticket, so any archive made"
echo "before this point is the unstapled version."
