#!/bin/bash

# Checks, before anything is built, that the signing and notarization
# credentials a CI run was given work, so a wrong secret fails the run in a
# minute rather than after the build: the certificate opens with its password
# and signs a probe the way ci/deploy.sh signs the app, and the notary service
# accepts the API key (or the Apple ID). Prints no secret, nor anything
# derived from one.
#
# Reads the environment the package step gets. Without a certificate there is
# nothing to check: ci/deploy.sh then signs ad hoc.

set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

# ci/deploy.sh signs with the certificate once MACOS_TEAM_ID is set and ad hoc
# otherwise: anything in between is a secret forgotten, which it would only
# trip over after the build.
missing=()
for name in MACOS_CERT MACOS_CERT_PW MACOS_TEAM_ID; do
  if [[ -z "${!name:-}" ]]; then
    missing+=("$name")
  fi
done
notary=
for name in MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER MACOS_APPLEID MACOS_APP_PW; do
  if [[ -n "${!name:-}" ]]; then
    notary=yes
  fi
done
if [[ ${#missing[@]} -eq 3 ]]; then
  [[ -z "$notary" ]] ||
    fail "notarization credentials but no certificate: set MACOS_CERT, MACOS_CERT_PW and MACOS_TEAM_ID"
  echo "==> No signing certificate in this run; the package will be signed ad hoc"
  exit 0
fi
[[ ${#missing[@]} -eq 0 ]] ||
  fail "signing needs all of MACOS_CERT, MACOS_CERT_PW and MACOS_TEAM_ID; not set: ${missing[*]}"

WORK_DIR=$(mktemp -d)
KEYCHAIN="$WORK_DIR/signing-check.keychain-db"
ORIGINAL_KEYCHAIN=
cleanup() {
  if [[ -n "$ORIGINAL_KEYCHAIN" ]]; then
    security default-keychain -d user -s "$ORIGINAL_KEYCHAIN" || true
  fi
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

# --- the secrets' shapes, before anything is done with them ----------------
# MACOS_CERT, MACOS_CERT_PW and MACOS_NOTARY_KEY are base64, as ci/deploy.sh
# and ci/macos-notarize-local.sh decode them; the IDs are as Apple shows them.

[[ "$MACOS_TEAM_ID" =~ ^[A-Z0-9]{10}$ ]] ||
  fail "MACOS_TEAM_ID is not a team ID: ten letters and digits, from developer.apple.com > Membership"

CERT="$WORK_DIR/cert.p12"
if ! printf '%s' "$MACOS_CERT" | base64 --decode >"$CERT" 2>/dev/null || [[ ! -s "$CERT" ]]; then
  fail "MACOS_CERT is not base64: set it to the output of  base64 -i cert.p12"
fi
# A .p12 is DER, which opens with an ASN.1 sequence.
[[ "$(head -c 1 "$CERT" | od -An -tx1 | tr -d ' \n')" == "30" ]] ||
  fail "MACOS_CERT is not a .p12 file: export the certificate with its private key as .p12, then  base64 -i cert.p12"

if ! CERT_PW=$(printf '%s' "$MACOS_CERT_PW" | base64 --decode 2>/dev/null); then
  fail "MACOS_CERT_PW is not base64: set it to the output of  printf '%s' 'the .p12 password' | base64"
fi
# GitHub masks the secret as stored, not what it decodes to.
if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
  echo "::add-mask::$CERT_PW"
fi

NOTARY_AUTH=()
if [[ -n "${MACOS_NOTARY_KEY:-}" || -n "${MACOS_NOTARY_KEY_ID:-}" || -n "${MACOS_NOTARY_ISSUER:-}" ]]; then
  [[ -n "${MACOS_NOTARY_KEY:-}" && -n "${MACOS_NOTARY_KEY_ID:-}" && -n "${MACOS_NOTARY_ISSUER:-}" ]] ||
    fail "an API key needs all three of MACOS_NOTARY_KEY, MACOS_NOTARY_KEY_ID and MACOS_NOTARY_ISSUER"
  [[ "$MACOS_NOTARY_ISSUER" =~ ^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$ ]] ||
    fail "MACOS_NOTARY_ISSUER is not an issuer ID: the UUID above the keys in App Store Connect > Users and Access > Integrations"
  [[ "$MACOS_NOTARY_KEY_ID" =~ ^[A-Z0-9]{8,12}$ ]] ||
    fail "MACOS_NOTARY_KEY_ID is not a key ID: the short ID on the key's own row, not the issuer ID"
  KEY_FILE="$WORK_DIR/AuthKey_${MACOS_NOTARY_KEY_ID}.p8"
  if ! (umask 077 && printf '%s' "$MACOS_NOTARY_KEY" | base64 --decode >"$KEY_FILE" 2>/dev/null) ||
    [[ "$(head -n 1 "$KEY_FILE" | tr -d '\r')" != "-----BEGIN PRIVATE KEY-----" ]]; then
    fail "MACOS_NOTARY_KEY is not a .p8 key: set it to the output of  base64 -i AuthKey_<key id>.p8"
  fi
  NOTARY_AUTH=(--key "$KEY_FILE" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER")
  NOTARY_WHAT="the API key (MACOS_NOTARY_KEY, MACOS_NOTARY_KEY_ID, MACOS_NOTARY_ISSUER)"
elif [[ -n "${MACOS_APPLEID:-}" && -n "${MACOS_APP_PW:-}" ]]; then
  NOTARY_AUTH=(--apple-id "$MACOS_APPLEID" --password "$MACOS_APP_PW" --team-id "$MACOS_TEAM_ID")
  NOTARY_WHAT="the Apple ID (MACOS_APPLEID, MACOS_APP_PW)"
else
  # Signed but not notarized, the app would still be refused on other Macs.
  fail "a certificate but nothing to notarize with: set MACOS_NOTARY_KEY, MACOS_NOTARY_KEY_ID and MACOS_NOTARY_ISSUER"
fi

# --- the certificate signs, as ci/deploy.sh will sign the app --------------
KEYCHAIN_PW=$(uuidgen)
security create-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security import "$CERT" -k "$KEYCHAIN" -P "$CERT_PW" -T /usr/bin/codesign >/dev/null 2>&1 ||
  fail "MACOS_CERT does not open with MACOS_CERT_PW: check the .p12 password, base64-encoded as above"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PW" "$KEYCHAIN" >/dev/null 2>&1
ORIGINAL_KEYCHAIN=$(security default-keychain -d user | sed -e 's/^ *"//' -e 's/" *$//')
security default-keychain -d user -s "$KEYCHAIN"

# ci/deploy.sh names the identity by the team ID, which codesign matches
# against the certificate's name.
PROBE="$WORK_DIR/probe"
cp /usr/bin/true "$PROBE"
if ! codesign --keychain "$KEYCHAIN" --force --options runtime --sign "$MACOS_TEAM_ID" "$PROBE" \
  >/dev/null 2>"$WORK_DIR/codesign.err"; then
  echo "codesign: $(tail -n 1 "$WORK_DIR/codesign.err")" >&2
  fail "the certificate does not sign for MACOS_TEAM_ID: it has to be a Developer ID Application certificate of that team, exported with its private key"
fi
codesign -dv "$PROBE" 2>&1 | grep -q '^Authority=Developer ID Application:' ||
  fail "the certificate is not a Developer ID Application certificate, the only kind Apple notarizes"
if ! security find-certificate -c "Developer ID Application" -p "$KEYCHAIN" |
  openssl x509 -noout -checkend $((60 * 24 * 3600)) >/dev/null 2>&1; then
  echo "::warning::The Developer ID certificate expires within 60 days: make a new one at developer.apple.com and replace MACOS_CERT and MACOS_CERT_PW"
fi
echo "==> The certificate signs as a Developer ID Application of the team"

# --- the notary service takes the credentials -----------------------------
# Lists past submissions, which needs the same sign-in as a submission and
# submits nothing.
if ! xcrun notarytool history "${NOTARY_AUTH[@]}" >/dev/null 2>"$WORK_DIR/notary.err"; then
  echo "notarytool: $(tail -n 1 "$WORK_DIR/notary.err")" >&2
  fail "the notary service does not accept $NOTARY_WHAT"
fi
echo "==> The notary service accepts $NOTARY_WHAT"
