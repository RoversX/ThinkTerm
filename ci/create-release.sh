#!/bin/bash
set -x
name="$1"

# Placeholder body. Whoever finishes the release replaces it in the web UI --
# these two lines are here so a half-finished draft says what it is still
# missing rather than sitting there blank.
notes=$(cat <<EOT
_Draft: replace these notes before publishing._

- [ ] Attach the macOS zip from \`ci/macos-package.sh\`
- [ ] Write the release notes
EOT
)

# A draft, not a prerelease: a prerelease is publicly visible, so the window
# between CI uploading the Windows and Linux packages and a human attaching the
# macOS zip would show everyone a release that is missing a platform. A draft
# is visible only to people who can write to the repo.
#
# The tag comes into existence when the draft is published, pointing at
# --target. Without it gh would tag the default branch -- a commit nobody
# built. Re-running the workflow finds the existing draft and only re-uploads.
gh release view "$name" || gh release create --draft \
  ${GITHUB_SHA:+--target "$GITHUB_SHA"} \
  --notes "$notes" --title "$name" "$name"
