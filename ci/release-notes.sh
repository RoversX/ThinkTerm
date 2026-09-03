#!/bin/bash

# Emits the body of a ThinkTerm release, as Markdown, on stdout.
#
# The download list is built from what is actually in the dist directory, so a
# release that skipped a platform links only to the packages it has. macOS is
# the exception: its zips are signed and notarized on a laptop and attached to
# the draft by hand, so that row is written from the names those uploads are
# expected to carry, and points at files that do not exist yet.
#
# Whoever finishes the release writes the "what changed" part above all this on
# the release page. Everything here is mechanical.

set -euo pipefail

tag="${1:?usage: release-notes.sh <tag> [dist-dir]}"
dist="${2:-dist}"
version="${tag#v}"

base="https://github.com/RoversX/ThinkTerm/releases/download/$tag"

# Every Linux package we build is one of these two. Anything unrecognized is
# labelled 64-bit rather than dropped, so a new architecture shows up wrong
# rather than silently going missing.
arch_label() {
  case "$1" in
    *arm64* | *aarch64*) echo "ARM64" ;;
    *) echo "64-bit" ;;
  esac
}

# "[64-bit](url) · [ARM64](url)" for every file matching a glob. Returns 1 when
# the release contains none of them, so the caller can drop the whole row.
#
# Two passes so that 64-bit leads, whatever the filenames sort as: it is what
# most people want, and a row reads as a recommendation followed by the
# alternatives.
links_by_arch() {
  local out='' f want
  for want in x86 arm; do
    for f in "$dist"/$1; do
      [ -f "$f" ] || continue
      f=$(basename "$f")
      case "$f" in
        *arm64* | *aarch64*) [ "$want" = arm ] || continue ;;
        *) [ "$want" = x86 ] || continue ;;
      esac
      [ -z "$out" ] || out="$out · "
      out="$out[$(arch_label "$f")]($base/$f)"
    done
  done
  [ -n "$out" ] || return 1
  printf '%s\n' "$out"
}

# One named file. Used where the architecture is not what distinguishes the
# download -- the Windows installer against the portable zip, say.
link_to() {
  local f
  for f in "$dist"/$2; do
    [ -f "$f" ] || continue
    printf '[%s](%s/%s)' "$1" "$base" "$(basename "$f")"
    return 0
  done
  return 1
}

# The rpms that make up a full install, labelled by package name. Which of them
# you want is not a question of architecture, and the split exists so that a
# headless machine can take the multiplexer alone -- so that one is listed
# separately and excluded here.
rpm_full_links() {
  local out='' f name
  for f in "$dist"/*.rpm; do
    [ -f "$f" ] || continue
    f=$(basename "$f")
    case "$f" in thinkterm-mux-server-*) continue ;; esac
    name=${f%%-[0-9]*}
    case "$f" in *arm64* | *aarch64*) name="$name (ARM64)" ;; esac
    [ -z "$out" ] || out="$out · "
    out="$out[$name]($base/$f)"
  done
  [ -n "$out" ] || return 1
  printf '%s\n' "$out"
}

echo "<!-- What changed in this release goes above this line. Then delete it. -->"
echo
echo "## Downloads"
echo
echo "### macOS"
echo
echo "[Apple silicon]($base/ThinkTerm-macos-arm64-$version.zip) · [Intel]($base/ThinkTerm-macos-x86_64-$version.zip)"
echo

win_setup=$(link_to "Installer" 'ThinkTerm-*-setup.exe') || win_setup=''
win_zip=$(link_to "Portable zip" 'ThinkTerm-windows-*.zip') || win_zip=''
if [ -n "$win_setup$win_zip" ]; then
  echo "### Windows"
  echo
  if [ -n "$win_setup" ] && [ -n "$win_zip" ]; then
    echo "$win_setup · $win_zip"
  else
    echo "$win_setup$win_zip"
  fi
  echo
fi

linux=''
if appimage=$(links_by_arch '*.AppImage'); then
  linux="$linux
**AppImage** — \`chmod +x\` and run it; nothing is installed

$appimage
"
fi
if deb=$(links_by_arch '*.deb'); then
  linux="$linux
**DEB (Debian / Ubuntu)** — \`sudo apt install ./<file>.deb\`

$deb
"
fi
if rpm_full=$(rpm_full_links); then
  linux="$linux
**RPM (Fedora / RHEL)** — put these in one directory, then \`sudo dnf install ./thinkterm-*.rpm\`

$rpm_full
"
fi
if rpm_mux=$(links_by_arch 'thinkterm-mux-server-*.rpm'); then
  linux="$linux
**Multiplexer server only** (headless remote host) — \`sudo dnf install ./<file>.rpm\`, and it pulls in no GUI libraries

$rpm_mux
"
fi

if [ -n "$linux" ]; then
  echo "### Linux"
  echo "$linux"
fi
