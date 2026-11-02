#!/bin/bash
# This script updates platform icon files from the default ThinkTerm icon.
# It assumes that the source image is square.
set -x
cd $(git rev-parse --show-toplevel)/assets/icon

src=ThinkTerm_simple.png
windows_src=ThinkTerm_windows.png
macos_icns=ThinkTerm_simple.icns
classic_macos_icns=ThinkTerm.icns
red_macos_icns=ThinkTerm_red.icns
otter_icon=ThinkTerm-otter.icon
otter_macos_icns=ThinkTerm_otter.icns
ictool="/Applications/Icon Composer.app/Contents/Executables/ictool"

conv_opts="-colors 256 -background none -density 300"

# the linux icon
convert $conv_opts -resize "!128x128" "$src" ../icon/terminal.png

for dim in 16 32 128 256 512 1024 ; do
  # convert is the imagemagick convert utility
  convert $conv_opts -border '10%' -bordercolor 'rgba(0,0,0,0)' -resize "!${dim}x${dim}" "$src" "icon_${dim}px.png"
done
# Prefer the hand-authored ThinkTerm macOS icons when present.
# png2icns is part of the libicns-utils on Fedora systems and is kept as a fallback.
mkdir -p ../macos/ThinkTerm.app/Contents/Resources
if [ -f "$macos_icns" ]; then
  cp "$macos_icns" ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm_simple.icns
  if [ -f "$classic_macos_icns" ]; then
    cp "$classic_macos_icns" ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm.icns
  fi
  if command -v sips >/dev/null 2>&1 ; then
    sips -z 512 512 -s format png "$macos_icns" --out ThinkTerm.png
    # The Appearance page's app icon choices, small so Settings never
    # decodes the full-size art to show them.
    sips -z 128 128 -s format png "$macos_icns" --out ThinkTerm_simple_128.png
    if [ -f "$classic_macos_icns" ]; then
      sips -z 128 128 -s format png "$classic_macos_icns" --out ThinkTerm_128.png
    fi
  fi
  # The red icon is the default one with red and blue swapped.
  if command -v iconutil >/dev/null 2>&1 ; then
    rm -rf red.iconset && iconutil -c iconset "$macos_icns" -o red.iconset
    for png in red.iconset/*.png ; do
      convert "$png" -channel-fx 'red<=>blue' "$png"
    done
    iconutil -c icns red.iconset -o "$red_macos_icns"
    rm -rf red.iconset
    cp "$red_macos_icns" ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm_red.icns
    sips -z 128 128 -s format png "$red_macos_icns" --out ThinkTerm_red_128.png
  fi
else
  png2icns ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm_simple.icns icon_*px.png
fi

# The otter is an Icon Composer document. ictool renders it edge to edge, so
# it goes on the macOS icon grid (824 of 1024, centred) with the grid's soft
# shadow, like the icons above, before it becomes an .icns.
if [ -x "$ictool" ] && command -v iconutil >/dev/null 2>&1 ; then
  rm -rf otter.iconset && mkdir otter.iconset
  "$ictool" "$otter_icon" --export-image --output-file otter-body.png \
    --platform macOS --rendition Default --width 824 --height 824 --scale 1
  convert otter-body.png -background none -gravity center -extent 1024x1024 \
    \( +clone -channel A -blur 0x7 -evaluate multiply 0.32 +channel -fill black -colorize 100 -roll +0+10 \) \
    +swap -composite -depth 8 otter-1024.png
  for dim in 16 32 128 256 512 ; do
    convert otter-1024.png -resize "${dim}x${dim}" "otter.iconset/icon_${dim}x${dim}.png"
    convert otter-1024.png -resize "$((dim * 2))x$((dim * 2))" "otter.iconset/icon_${dim}x${dim}@2x.png"
  done
  iconutil -c icns otter.iconset -o "$otter_macos_icns"
  rm -rf otter.iconset otter-body.png otter-1024.png
  cp "$otter_macos_icns" ../macos/ThinkTerm.app/Contents/Resources/ThinkTerm_otter.icns
  sips -z 128 128 -s format png "$otter_macos_icns" --out ThinkTerm_otter_128.png
fi

# Clean up
rm -f icon_*px.png

# The Windows icon
convert "$windows_src" $conv_opts -define icon:auto-resize=256,128,96,64,48,32,16 ../windows/terminal.ico
