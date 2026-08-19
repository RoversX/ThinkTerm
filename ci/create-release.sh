#!/bin/bash
set -x
name="$1"

notes=$(cat <<EOT
See https://github.com/RoversX/thinkterm/releases/tag/$name for the release notes

If you're looking for nightly downloads or more detailed installation instructions:

[ThinkTerm releases](https://github.com/RoversX/thinkterm/releases)
EOT
)

# When $name is not already a git tag (a manual run, where the name comes from
# ci/tag-name.sh) gh creates it -- against the default branch unless told
# otherwise, which would tag a commit nobody built.
gh release view "$name" || gh release create --prerelease \
  ${GITHUB_SHA:+--target "$GITHUB_SHA"} \
  --notes "$notes" --title "$name" "$name"
