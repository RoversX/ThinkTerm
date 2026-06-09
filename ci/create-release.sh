#!/bin/bash
set -x
name="$1"

notes=$(cat <<EOT
See https://github.com/RoversX/thinkterm/releases/tag/$name for the release notes

If you're looking for nightly downloads or more detailed installation instructions:

[ThinkTerm releases](https://github.com/RoversX/thinkterm/releases)
EOT
)

gh release view "$name" || gh release create --prerelease --notes "$notes" --title "$name" "$name"
