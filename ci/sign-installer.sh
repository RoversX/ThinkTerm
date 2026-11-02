#!/bin/bash
# Use only the CI release-signing secret, without shell tracing.
set +x
set -euo pipefail
umask 077
dest=${1:?usage: sign-installer.sh OUTPUT_DIR}
: "${THINKTERM_RELEASE_SIGNING_KEY:?release signing secret is not configured}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
sh -n install.sh
[ "$(sed -n '2p' install.sh)" = '# thinkterm-installer-v1' ]
cp install.sh "$work/install.sh"
printf '%s\n' "$THINKTERM_RELEASE_SIGNING_KEY" > "$work/key"
unset THINKTERM_RELEASE_SIGNING_KEY
ssh-keygen -Y sign -f "$work/key" -n thinkterm-installer-v1 "$work/install.sh"
printf 'thinkterm-installer namespaces="thinkterm-installer-v1" %s\n' "$(cat assets/release-signing.pub)" > "$work/signers"
ssh-keygen -Y verify -f "$work/signers" -I thinkterm-installer -n thinkterm-installer-v1 \
  -s "$work/install.sh.sig" < "$work/install.sh"
mkdir -p "$dest"
mv "$work/install.sh" "$work/install.sh.sig" "$dest/"
