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

base="https://github.com/RoversX/thinkterm/releases/download/$tag"

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

# The same header on every release, from main: the tag does not exist until
# the draft is published, so a link through it would show broken in the draft.
echo "## $version"
echo
echo "![ThinkTerm](https://raw.githubusercontent.com/RoversX/thinkterm/main/assets/release-header.jpg)"
echo
echo "🍺 Homebrew (macOS):"
echo
echo '```'
echo "brew install --cask roversx/tap/thinkterm"
echo '```'
echo
echo "🌐 Website: [closex.org/thinkterm](https://closex.org/thinkterm)"
echo
echo "📚 Documentation: [docs.closex.org/thinkterm](https://docs.closex.org/thinkterm)"
echo
echo "<!-- What changed in this release goes here. Then delete this line. -->"
echo
echo "## Downloads"
echo
echo "### macOS"
echo
echo "[Apple silicon]($base/ThinkTerm-macos-arm64-$version.zip) · [Intel]($base/ThinkTerm-macos-x86_64-$version.zip) — or \`curl -fsSL https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh | sh\`, which picks the right one, verifies it and puts the command-line tools on PATH"
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
# The tarballs are what install.sh fetches; the links are here for anyone who
# would rather unpack by hand. The desktop glob pins the digit after the
# name so it cannot also match thinkterm-server-*.
desktop=$(links_by_arch 'thinkterm-[0-9]*-linux-*.tar.gz') || desktop=''
server=$(links_by_arch 'thinkterm-server-*.tar.gz') || server=''
if [ -n "$desktop$server" ]; then
  linux="$linux
**Install script** — \`curl -fsSL https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh | sh\` puts ThinkTerm under \`~/.local\` without root and asks which variant you want; \`--desktop\` or \`--server\` (headless remote host, no GUI) skips the question
"
  [ -z "$desktop" ] || linux="$linux
Desktop tarball: $desktop
"
  [ -z "$server" ] || linux="$linux
Server tarball: $server
"
fi
if appimage=$(links_by_arch '*.AppImage'); then
  linux="$linux
**AppImage** — \`chmod +x\` and run it; nothing is installed

$appimage
"
fi
# The globs pin the digit after the name so thinkterm-* cannot also match
# thinkterm-server-*. Only one of the two packages may be installed at a time.
if deb=$(links_by_arch 'thinkterm-[0-9]*.deb'); then
  linux="$linux
**DEB (Debian / Ubuntu)** — \`sudo apt install ./<file>.deb\`

$deb
"
fi
if deb_server=$(links_by_arch 'thinkterm-server-[0-9]*.deb'); then
  linux="$linux
**DEB, server only** (headless remote host, no GUI and no graphics libraries) — \`sudo apt install ./<file>.deb\`; replaces the desktop package if that is installed

$deb_server
"
fi
if rpm_desktop=$(links_by_arch 'thinkterm-[0-9]*.rpm'); then
  linux="$linux
**RPM (Fedora / RHEL)** — \`sudo dnf install ./<file>.rpm\`

$rpm_desktop
"
fi
if rpm_server=$(links_by_arch 'thinkterm-server-[0-9]*.rpm'); then
  linux="$linux
**RPM, server only** (headless remote host, no GUI and no graphics libraries) — \`sudo dnf install ./<file>.rpm\`; replaces the desktop package if that is installed

$rpm_server
"
fi

if [ -n "$linux" ]; then
  echo "### Linux"
  echo "$linux"
fi
