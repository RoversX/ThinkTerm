#!/bin/bash
# Called only after all packages are assembled. Never enable shell tracing:
# the signing material comes from a GitHub Actions secret.
set +x
set -euo pipefail
umask 077
tag=${1:?usage: sign-release.sh TAG DIST [EXISTING_PACKAGES]}
dist=${2:?missing package directory}
previous=${3:-}
: "${THINKTERM_RELEASE_SIGNING_KEY:?release signing secret is not configured}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '%s\n' "$THINKTERM_RELEASE_SIGNING_KEY" > "$work/key"
unset THINKTERM_RELEASE_SIGNING_KEY
set -- "$dist"
[ -z "$previous" ] || set -- "$previous" "$dist"
python3 ci/release-manifest.py "$tag" "$work/thinkterm-release.txt" "$@"
ssh-keygen -Y sign -f "$work/key" -n thinkterm-release "$work/thinkterm-release.txt"
printf 'thinkterm-release namespaces="thinkterm-release" %s\n' "$(cat assets/release-signing.pub)" > "$work/signers"
ssh-keygen -Y verify -f "$work/signers" -I thinkterm-release -n thinkterm-release \
  -s "$work/thinkterm-release.txt.sig" < "$work/thinkterm-release.txt"
mv "$work/thinkterm-release.txt" "$work/thinkterm-release.txt.sig" "$dist/"
