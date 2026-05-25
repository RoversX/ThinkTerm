#!/bin/bash
# This script updates the icon files from the svg file.
# It assumes that the svg file is square.
set -x
cd $(git rev-parse --show-toplevel)/assets/icon

src=wezterm-icon.svg
macos_icns=ThinkTerm.icns

conv_opts="-colors 256 -background none -density 300"

# the linux icon
convert $conv_opts -resize "!128x128" "$src" ../icon/terminal.png

for dim in 16 32 128 256 512 1024 ; do
  # convert is the imagemagick convert utility
  convert $conv_opts -border '10%' -bordercolor 'rgba(0,0,0,0)' -resize "!${dim}x${dim}" "$src" "icon_${dim}px.png"
done
# Prefer the hand-authored ThinkTerm macOS icon when present.
# png2icns is part of the libicns-utils on Fedora systems and is kept as a fallback.
mkdir -p ../macos/ThinkTerm.app/Contents/Resources
if [ -f "$macos_icns" ]; then
  cp "$macos_icns" ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm.icns
  if command -v sips >/dev/null 2>&1 ; then
    sips -z 512 512 -s format png "$macos_icns" --out ThinkTerm.png
  fi
else
  png2icns ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm.icns icon_*px.png
fi

# Clean up
rm -f icon_*px.png

# The Windows icon
convert $conv_opts -define icon:auto-resize=256,128,96,64,48,32,16 $src ../windows/terminal.ico
