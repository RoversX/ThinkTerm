#!/bin/bash
# Fail immediately when a required submodule checked out empty.
#
# Every one of these is needed at compile time -- the freetype and harfbuzz
# trees are built from source by deps/freetype/build.rs, and the two
# third_party icon sets are pulled in with include_bytes! from
# wezterm-gui/src/termwindow/ui/icons.rs. Without this check an empty checkout
# surfaces 20+ minutes into the build as an unreadable-file error, on a runner
# that bills at up to 10x.

set -eu

status=0

for dir in \
  deps/freetype/zlib \
  deps/freetype/libpng \
  deps/freetype/freetype2 \
  deps/harfbuzz/harfbuzz \
  third_party/lucide \
  third_party/simple-icons
do
  if [ -n "$(ls -A "$dir" 2>/dev/null)" ] ; then
    echo "ok    $dir"
  else
    echo "::error::submodule $dir is empty; checkout needs submodules: recursive"
    status=1
  fi
done

exit $status
