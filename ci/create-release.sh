#!/bin/bash
set -ex
name="$1"
dist="${2:-dist}"

# The body is generated from what is actually in $dist -- see
# ci/release-notes.sh -- so the draft already carries a download list that
# matches the packages attached to it. Only whoever finishes the release adds
# what changed.
notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
bash "$(dirname "$0")/release-notes.sh" "$name" "$dist" > "$notes"

# A draft, not a prerelease: a prerelease is publicly visible, so the window
# between CI uploading the Windows and Linux packages and a human attaching the
# macOS zip would show everyone a release that is missing a platform. A draft
# is visible only to people who can write to the repo.
#
# The tag comes into existence when the draft is published, pointing at
# --target. Without it gh would tag the default branch -- a commit nobody
# built. Re-running the workflow finds the existing draft and only re-uploads:
# the notes are written once, so an edit made on the release page survives.
gh release view "$name" || gh release create --draft \
  ${GITHUB_SHA:+--target "$GITHUB_SHA"} \
  --notes-file "$notes" --title "$name" "$name"
