#!/bin/bash

signing_mask() {
  if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
    local value=$1
    value=${value//%/%25}
    value=${value//$'\r'/%0D}
    value=${value//$'\n'/%0A}
    printf '::add-mask::%s\n' "$value"
  fi
}

# Only fixed descriptions reach the log. Tool output can contain identities,
# paths and credentials, including on success, so never echo excerpts of it.
signing_error_reason() {
  local matched=no
  while IFS='|' read -r pattern message; do
    if LC_ALL=C grep -Eiq -- "$pattern" "$@" 2>/dev/null; then
      printf '  %s\n' "$message" >&2
      matched=yes
    fi
  done <<'REASONS'
HTTP[^0-9]*401|Authentication was rejected; check the notarization credentials.
HTTP[^0-9]*403|Access was denied; check the API key permissions and team membership.
unauthorized|Authentication was rejected; check the notarization credentials.
invalid credentials|Authentication was rejected; check the notarization credentials.
keychain password item.*not found|The notarization keychain profile is missing; configure the requested profile.
invalid (private )?key|The key or key identifier was rejected; check its format and credential configuration.
password.*incorrect|The password was rejected; check the certificate password or keychain credentials.
MAC verification failed|The certificate could not be decrypted; check its password and PKCS12 format.
no identity found|No matching signing identity was found; check the certificate and its private key.
item could not be found in the keychain|A required keychain item is missing; check the certificate and its private key.
private key.*not found|The signing private key is missing from the keychain.
certificate.*expired|The signing certificate has expired.
certificate.*revoked|The signing certificate has been revoked.
interaction is not allowed|Keychain access was denied; check that it is unlocked and allows codesign.
unable to build chain|The certificate trust chain is incomplete; check the signing certificate and intermediates.
not signed|A bundled executable is unsigned; check nested code signing.
signature.*invalid|A code signature is invalid; re-sign the complete bundle.
sealed resource|The signed bundle was modified or has missing resources; rebuild and re-sign it.
hardened runtime|Hardened Runtime is missing or invalid; check the signing options.
secure timestamp|A secure signing timestamp is missing or invalid.
SDK older|A bundled binary was built with an unsupported SDK; rebuild it with a supported SDK.
entitlement|The signing entitlements need attention.
timed? out|The request timed out; check connectivity and retry.
could not resolve|Name resolution failed; check connectivity and retry.
network|A network request failed; check connectivity and retry.
connection|A connection failed; check connectivity and retry.
service unavailable|The notarization service is unavailable; retry later.
ticket.*not found|The notarization ticket is not available yet; retry stapling after acceptance.
file does not exist|A required input or notarization ticket is missing.
permission denied|File or keychain access was denied; check access permissions.
Unsupported signing mode|MACOS_SIGNING_MODE has to be adhoc, development or developerid.
App bundle not found|The app bundle is missing; check the packaging step.
No ".*" certificate found|No matching certificate is in the keychain; create one at developer.apple.com or in Xcode.
More than one ".*" certificate|More than one matching certificate is in the keychain; set MACOS_SIGNING_IDENTITY to the one to use.
REASONS
  if [[ "$matched" == no ]]; then
    echo "  No recognized diagnostic; check this step's inputs and tool availability. Raw output was withheld." >&2
  fi
}

# signing_rejected_files <notary log> <app bundle>
#
# Apple's notarization log names each rejected file by its path inside the
# submission. A path is shown only when it names a file shipped in the bundle,
# under Contents/ and without stepping out of it: those names are the app's
# own, public with it. Any other path is withheld.
signing_rejected_files() {
  local count i path seen=' ' withheld=0
  count=$(plutil -extract issues raw -o - "$1" 2>/dev/null) || return 0
  [[ "$count" =~ ^[0-9]+$ ]] || return 0
  for ((i = 0; i < count && i < 50; i++)); do
    path=$(plutil -extract "issues.$i.path" raw -o - "$1" 2>/dev/null) || continue
    path=${path#*.app/}
    if [[ "$path" =~ ^Contents(/[A-Za-z0-9_+-][A-Za-z0-9._+-]*)+$ && -e "$2/$path" ]]; then
      if [[ "$seen" != *" $path "* ]]; then
        seen+="$path "
        printf '  Rejected file: %s\n' "$path" >&2
      fi
    else
      withheld=$((withheld + 1))
    fi
  done
  if [[ "$withheld" -gt 0 ]]; then
    printf '  %s rejected file name(s) withheld.\n' "$withheld" >&2
  fi
}

# The optional output file is private scratch data for callers that need a
# plist, certificate or keychain path. It must never be uploaded or printed.
signing_capture() {
  local label=$1 output=$2 diagnostic result
  shift 2
  diagnostic=$(mktemp 2>/dev/null) || {
    echo "error: Could not create a private signing diagnostic file." >&2
    return 1
  }
  if [[ "$output" == /dev/null ]]; then
    if ("$@") >"$diagnostic" 2>&1; then result=0; else result=$?; fi
  else
    if (umask 077; "$@" >"$output") 2>"$diagnostic"; then result=0; else result=$?; fi
  fi
  if [[ "$result" -ne 0 ]]; then
    printf 'error: %s failed (exit %s).\n' "$label" "$result" >&2
    signing_error_reason "$diagnostic" "$output"
  fi
  rm -f "$diagnostic" 2>/dev/null ||
    echo "warning: Could not remove a private signing diagnostic file." >&2
  return "$result"
}

signing_run() {
  local label=$1
  shift
  signing_capture "$label" /dev/null "$@"
}
