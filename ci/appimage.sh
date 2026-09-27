#!/bin/bash
set -x
rm -rf AppDir *.AppImage *.zsync
set -e

mkdir AppDir

# The cargo profile the binaries were built with, as ci/deploy.sh reads it.
PROFILE=${CARGO_PROFILE:-release}
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/thinkterm-mux-server
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/thinkterm
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/wezterm
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/thinkterm-gui
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/thinkterm-plugin-server
install -Dsm755 -t AppDir/usr/bin target/$PROFILE/strip-ansi-escapes
install -Dm644 assets/icon/terminal.png AppDir/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
install -Dm644 assets/wezterm.desktop AppDir/usr/share/applications/com.roversx.thinkterm.desktop
install -Dm644 assets/wezterm.appdata.xml AppDir/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
install -Dm644 assets/wezterm-nautilus.py AppDir/usr/share/nautilus-python/extensions/wezterm-nautilus.py
install -Dm644 NOTICE AppDir/usr/share/doc/thinkterm/NOTICE
# Both license texts, not just the NOTICE that points at them: AUR and
# linuxbrew unpack this AppImage for their own license dirs, so leaving
# them out here leaves them out of every downstream Linux package.
install -Dm644 LICENSE.md AppDir/usr/share/doc/thinkterm/LICENSE.md
install -Dm644 LICENSE-MIT AppDir/usr/share/doc/thinkterm/LICENSE-MIT

# linuxdeploy publishes per-machine names (x86_64, aarch64); the rest of the
# release labels the same machine arm64, so normalise for the output name.
MACHINE=$(uname -m)
case "$MACHINE" in
  aarch64) ARCH=arm64 ;;
  *)       ARCH=$MACHINE ;;
esac

# -f matters: without it curl writes the HTTP error body to the file, chmod
# makes it executable, and the [ -x ] guard then caches that corrupt download
# for the rest of the job.
[ -x /tmp/linuxdeploy ] || ( curl -fL --retry 3 "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${MACHINE}.AppImage" -o /tmp/linuxdeploy && chmod +x /tmp/linuxdeploy )

# GitHub's job containers have no /dev/fuse, so both linuxdeploy itself and the
# AppImage it produces have to self-extract instead of FUSE-mounting.
export APPIMAGE_EXTRACT_AND_RUN=1

TAG_NAME=${TAG_NAME:-$(git -c "core.abbrev=8" show -s "--format=%cd-%h" "--date=format:%Y%m%d-%H%M%S")}
distro=$(lsb_release -is 2>/dev/null || sh -c "source /etc/os-release && echo \$NAME")
distver=$(lsb_release -rs 2>/dev/null || sh -c "source /etc/os-release && echo \$VERSION_ID")

# Embed appropriate update info
# https://github.com/AppImage/AppImageSpec/blob/master/draft.md#github-releases
# The glob has to pin the architecture: a release now carries both an x86_64
# and an aarch64 AppImage, and an arch-blind pattern would let AppImageUpdate
# overwrite an arm64 binary with the x86_64 one.
if [[ "$BUILD_REASON" == "Schedule" ]] ; then
  UPDATE="gh-releases-zsync|RoversX|thinkterm|nightly|ThinkTerm-*-$ARCH.AppImage.zsync"
  OUTPUT=ThinkTerm-nightly-$distro$distver-$ARCH.AppImage
else
  UPDATE="gh-releases-zsync|RoversX|thinkterm|latest|ThinkTerm-*-$ARCH.AppImage.zsync"
  OUTPUT=ThinkTerm-$TAG_NAME-$distro$distver-$ARCH.AppImage
fi

# Munge the path so that it finds our appstreamcli wrapper
PATH="$PWD/ci:$PATH" \
VERSION="$TAG_NAME" \
UPDATE_INFORMATION="$UPDATE" \
OUTPUT="$OUTPUT" \
  /tmp/linuxdeploy \
  --exclude-library='libwayland-client.so.0' \
  --appdir AppDir \
  --output appimage \
  --desktop-file assets/wezterm.desktop

# Update the AUR build file.  We only really want to use this for tagged
# builds but it doesn't hurt to generate it always here.
SHA256=$(sha256sum $OUTPUT | cut -d' ' -f1)
sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$SHA256/g" < ci/PKGBUILD.template > PKGBUILD
sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$SHA256/g" < ci/wezterm-linuxbrew.rb.template > wezterm-linuxbrew.rb
