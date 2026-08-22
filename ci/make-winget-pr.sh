#!/bin/bash
set -xe

winget_repo=$1
setup_exe=$2
TAG_NAME=$(ci/tag-name.sh)

cd "$winget_repo" || exit 1

# First sync repo with upstream
git remote add upstream https://github.com/microsoft/winget-pkgs.git || true
git fetch upstream master --quiet
git checkout -b "$TAG_NAME" upstream/master

exehash=$(sha256sum -b ../$setup_exe | cut -f1 -d' ' | tr a-f A-F)

release_date=$(git show -s "--format=%cd" "--date=format:%Y-%m-%d")

# Create the directory structure
mkdir -p manifests/r/RoversX/ThinkTerm/$TAG_NAME

cat > manifests/r/RoversX/ThinkTerm/$TAG_NAME/RoversX.ThinkTerm.installer.yaml <<-EOT
PackageIdentifier: RoversX.ThinkTerm
PackageVersion: $TAG_NAME
MinimumOSVersion: 10.0.17763.0
InstallerType: inno
UpgradeBehavior: install
ReleaseDate: $release_date
Installers:
- Architecture: x64
  InstallerUrl: https://github.com/RoversX/thinkterm/releases/download/$TAG_NAME/$setup_exe
  InstallerSha256: $exehash
  ProductCode: '{56CBA99B-8F65-4EC0-8CE4-F13BFCB70274}_is1'
ManifestType: installer
ManifestVersion: 1.1.0
EOT

cat > manifests/r/RoversX/ThinkTerm/$TAG_NAME/RoversX.ThinkTerm.locale.en-US.yaml <<-EOT
PackageIdentifier: RoversX.ThinkTerm
PackageVersion: $TAG_NAME
PackageLocale: en-US
Publisher: RoversX
PublisherUrl: https://github.com/RoversX/thinkterm
PublisherSupportUrl: https://github.com/RoversX/thinkterm/issues
Author: RoversX
PackageName: ThinkTerm
PackageUrl: https://github.com/RoversX/thinkterm
License: GPL-3.0-only
LicenseUrl: https://github.com/RoversX/thinkterm/blob/main/LICENSE.md
ShortDescription: A workspace-first terminal emulator and multiplexer implemented in Rust
ReleaseNotesUrl: https://github.com/RoversX/thinkterm/releases/tag/$TAG_NAME
ManifestType: defaultLocale
ManifestVersion: 1.1.0
EOT

cat > manifests/r/RoversX/ThinkTerm/$TAG_NAME/RoversX.ThinkTerm.yaml <<-EOT
PackageIdentifier: RoversX.ThinkTerm
PackageVersion: $TAG_NAME
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.1.0
EOT

git add --all
git diff --cached
git commit -m "New version: RoversX.ThinkTerm version $TAG_NAME"
git push --set-upstream origin "$TAG_NAME" --quiet
