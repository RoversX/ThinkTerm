#!/bin/bash

set +x
set -euo pipefail

. "$(dirname "$0")/macos-signing-log.sh"

APP_PATH=${1:?usage: macos-notarize-local.sh path/to/ThinkTerm.app [keychain-profile]}
PROFILE=${2:-${MACOS_NOTARY_PROFILE:-thinkterm}}

if [[ ! -d "$APP_PATH" ]]; then
  echo "error: App bundle not found; check the supplied bundle path." >&2
  exit 1
fi

WORK_DIR=$(mktemp -d 2>/dev/null) || {
  echo "error: Could not create the private notarization directory." >&2
  exit 1
}
trap 'rm -rf "$WORK_DIR" 2>/dev/null || echo "warning: Could not remove the private notarization directory." >&2' EXIT

# Locally the credentials live in a keychain profile created once with
# `notarytool store-credentials`.  A CI runner has no such keychain, so prefer
# credentials passed in the environment: an App Store Connect API key (the
# .p8 file base64-encoded, its key ID and the issuer ID), which is tied to no
# person's Apple ID and can be revoked on its own; else an Apple ID with an
# app-specific password.
if [[ -n "${MACOS_NOTARY_KEY:-}" && -n "${MACOS_NOTARY_KEY_ID:-}" && -n "${MACOS_NOTARY_ISSUER:-}" ]]; then
  KEY_FILE="$WORK_DIR/notary-key.p8"
  if ! (umask 077 && printf '%s' "$MACOS_NOTARY_KEY" | base64 --decode >"$KEY_FILE") 2>/dev/null; then
    echo "error: Could not decode MACOS_NOTARY_KEY into the private key file." >&2
    exit 1
  fi
  NOTARY_AUTH=(--key "$KEY_FILE" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER")
  echo "==> Using an App Store Connect API key from the environment"
elif [[ -n "${MACOS_APPLEID:-}" && -n "${MACOS_APP_PW:-}" && -n "${MACOS_TEAM_ID:-}" ]]; then
  NOTARY_AUTH=(--apple-id "$MACOS_APPLEID" --password "$MACOS_APP_PW" --team-id "$MACOS_TEAM_ID")
  echo "==> Using notarization credentials from the environment"
else
  NOTARY_AUTH=(--keychain-profile "$PROFILE")
  echo "==> Using notarization credentials from a keychain profile"
fi

# Notarization only accepts a container, never a bare .app, and it has to be
# built with ditto: plain `zip` drops the symlinks and extended attributes that
# codesign relies on, which Apple rejects as a damaged bundle.
SUBMISSION_ZIP="$WORK_DIR/submission.zip"
RESULT_PLIST="$WORK_DIR/result.plist"

echo "==> Packing the app bundle for submission"
signing_run "Pack notarization submission" /usr/bin/ditto -c -k --keepParent "$APP_PATH" "$SUBMISSION_ZIP"

echo "==> Submitting to Apple (this usually takes 1-5 minutes)"
# A rejected submission may end notarytool with an error, yet the result still
# names the submission whose log says why: read the result first, and report
# the exit only when there is no result to read.
submit_exit=0
(umask 077; xcrun notarytool submit "$SUBMISSION_ZIP" \
  "${NOTARY_AUTH[@]}" \
  --wait \
  --output-format plist >"$RESULT_PLIST") 2>"$WORK_DIR/submit-diagnostic" || submit_exit=$?

if ! status=$(plutil -extract status raw -o - "$RESULT_PLIST" 2>/dev/null) ||
   ! submission_id=$(plutil -extract id raw -o - "$RESULT_PLIST" 2>/dev/null); then
  if [[ "$submit_exit" -ne 0 ]]; then
    echo "error: Submit for notarization failed (exit $submit_exit)." >&2
    signing_error_reason "$WORK_DIR/submit-diagnostic" "$RESULT_PLIST"
  else
    echo "error: Could not read the notarization result; the service returned an unexpected response." >&2
  fi
  exit 1
fi

if [[ "$status" != "Accepted" ]]; then
  # Read the diagnostic privately and emit only recognized, fixed reasons and
  # the rejected files' names inside the bundle: never print the log itself.
  echo "error: Notarization was not accepted; checking the service diagnostics." >&2
  if signing_capture "Fetch notarization diagnostics" "$WORK_DIR/notary-log" \
    xcrun notarytool log "$submission_id" "${NOTARY_AUTH[@]}"; then
    signing_error_reason "$WORK_DIR/notary-log"
    signing_rejected_files "$WORK_DIR/notary-log" "$APP_PATH"
  fi
  echo "  Apple's full report: find the submission with 'xcrun notarytool history' on your Mac, then run 'xcrun notarytool log <id>'." >&2
  exit 1
fi
echo "==> Notarization accepted"

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
  # Only the last attempt explains its failure: the ones before it are that
  # race, which a successful run should not log as errors.
  if [[ "$attempt" -lt "$staple_attempts" ]]; then
    if xcrun stapler staple "$APP_PATH" >/dev/null 2>&1; then
      break
    fi
  elif signing_run "Staple notarization ticket" xcrun stapler staple "$APP_PATH"; then
    break
  else
    echo "==> Ticket still not available after ${staple_attempts} attempts" >&2
    exit 1
  fi
  echo "==> Ticket not published yet, retrying in 15s (attempt ${attempt}/${staple_attempts})"
  sleep 15
  attempt=$((attempt + 1))
done

echo "==> Verifying"
signing_run "Validate notarization ticket" xcrun stapler validate "$APP_PATH"
signing_run "Verify bundle signature" codesign --verify --deep --strict --verbose=2 "$APP_PATH"
signing_run "Assess Gatekeeper acceptance" spctl --assess --verbose=4 --type exec "$APP_PATH"

echo
echo "Notarized and stapled successfully."
echo "Re-package it now -- the bundle gained a ticket, so any archive made"
echo "before this point is the unstapled version."
