#!/bin/bash

# Shared resolver for macOS signing identities.  Meant to be sourced, not run.
#
# Auto-detection replaces what used to be a hardcoded team id.  A hardcoded
# team only ever matches one developer's keychain, so a fresh clone would fail
# with a confusing "no certificate for team XXXX" even when the person running
# it has a perfectly good certificate of their own.

# resolve_signing_identity <kind>
#
# <kind> is the certificate prefix, e.g. "Developer ID Application" or
# "Apple Development".  Echoes the full identity name on stdout when exactly
# one matches.  Anything else is an error: zero means nothing to sign with,
# and more than one has no safe default, so both report to stderr and return 1
# rather than silently picking.
resolve_signing_identity() {
  local kind=$1
  local matches count

  if [[ -n "${MACOS_SIGNING_IDENTITY:-}" ]]; then
    printf '%s\n' "$MACOS_SIGNING_IDENTITY"
    return 0
  fi

  matches=$(
    security find-identity -v -p codesigning |
      sed -n "s/.*\"\(${kind}:[^\"]*\)\".*/\1/p"
  )
  count=$(grep -c . <<<"$matches" || true)

  case "$count" in
    1)
      printf '%s\n' "$matches"
      return 0
      ;;
    0)
      echo "No \"${kind}\" certificate found in your keychain." >&2
      echo "Create or download one at developer.apple.com > Certificates," >&2
      echo "or in Xcode > Settings > Accounts > Manage Certificates." >&2
      return 1
      ;;
    *)
      echo "More than one \"${kind}\" certificate in your keychain:" >&2
      sed 's/^/    /' <<<"$matches" >&2
      echo "There is no safe default, so pick one explicitly:" >&2
      echo "    export MACOS_SIGNING_IDENTITY='<one full line from above>'" >&2
      return 1
      ;;
  esac
}

# team_id_from_identity <identity>
#
# Apple puts the team id in parentheses at the end of a Developer ID common
# name, which is the only place it can be read from without a second keychain
# query.
team_id_from_identity() {
  sed -n 's/.*(\([A-Z0-9]\{6,\}\))[[:space:]]*$/\1/p' <<<"$1"
}
