#!/bin/sh
# ThinkTerm installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh | sh
#
# Downloads the release for this machine, verifies it against the digest
# GitHub recorded at upload, and installs it for the current user -- no root,
# no package manager. Linux gets a tarball unpacked under ~/.local; macOS gets
# the signed ThinkTerm.app plus command-line links under ~/.local/bin. The two
# variants are mutually exclusive: installing one removes the other's files.
#
#   --desktop         GUI + CLI + TUI + mux server.
#   --server          CLI + TUI + mux server, no GUI (on Linux, no graphics
#                     libraries either). With neither, the script asks -- on a
#                     terminal. It never guesses from the machine.
#   --version X       Install release X instead of the latest one.
#   --prefix DIR      Install under DIR instead of ~/.local (binaries go in
#                     DIR/bin).
#   --app-dir DIR     macOS: put ThinkTerm.app in DIR instead of /Applications
#                     (or ~/Applications when that is not writable).
#   --from FILE       Use an already-downloaded tarball or zip; nothing is
#                     fetched.
#   --dry-run         Say what would be done and stop before touching the disk.
#
# Windows has an installer on the releases page; this script stops there.
#
# Deliberately not here: version comparison, uninstall. Re-running the script
# is the upgrade: it overwrites whatever is installed with the release it
# fetched, and going from --server to --desktop is the same run plus the GUI.
# Removing ThinkTerm is deleting the files it lists at the end. A manifest
# under share/thinkterm/ records what was installed for a future
# `thinkterm update`.

set -eu

repo="RoversX/thinkterm"
min_glibc="2.35"   # the Ubuntu 22.04 build base; see .github/workflows/release.yml

variant=""
tag=""
version=""
prefix="${HOME}/.local"
app_dir_opt=""
from=""
dry_run=""

usage() {
  cat <<'EOF'
ThinkTerm installer for Linux and macOS.

  curl -fsSL https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh | sh

  --desktop         GUI + CLI + TUI + mux server.
  --server          CLI + TUI + mux server, no GUI. With neither, the script
                    asks -- on a terminal.
  --version X       Install release X instead of the latest one.
  --prefix DIR      Install under DIR instead of ~/.local (binaries go in
                    DIR/bin).
  --app-dir DIR     macOS: put ThinkTerm.app in DIR instead of /Applications
                    (or ~/Applications when that is not writable).
  --from FILE       Use an already-downloaded tarball or zip; nothing is
                    fetched.
  --dry-run         Say what would be done and stop before touching the disk.
EOF
}

say()  { printf '%s\n' "$*"; }
warn() { printf 'install.sh: %s\n' "$*" >&2; }
die()  { warn "$@"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --desktop)  variant=desktop ;;
    --server)   variant=server ;;
    --version)  [ $# -ge 2 ] || die "--version needs a value"; tag="$2"; shift ;;
    --version=*) tag="${1#--version=}" ;;
    --prefix)   [ $# -ge 2 ] || die "--prefix needs a value"; prefix="$2"; shift ;;
    --prefix=*) prefix="${1#--prefix=}" ;;
    --app-dir)  [ $# -ge 2 ] || die "--app-dir needs a value"; app_dir_opt="$2"; shift ;;
    --app-dir=*) app_dir_opt="${1#--app-dir=}" ;;
    --from)     [ $# -ge 2 ] || die "--from needs a value"; from="$2"; shift ;;
    --from=*)   from="${1#--from=}" ;;
    --dry-run)  dry_run=1 ;;
    -h|--help)  usage; exit 0 ;;
    *) die "unknown option '$1' (try --help)" ;;
  esac
  shift
done

[ -n "$prefix" ] || die "--prefix needs a directory"
bindir="$prefix/bin"
sharedir="$prefix/share"

# ---- platform --------------------------------------------------------------

case "$(uname -s)" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*)
    die "on Windows, use the installer: https://github.com/$repo/releases" ;;
  *) die "unsupported OS '$(uname -s)'" ;;
esac

arch=$(uname -m)
case "$os/$arch" in
  linux/x86_64 | linux/aarch64) ;;
  linux/arm64) arch=aarch64 ;;
  macos/arm64 | macos/x86_64) ;;
  macos/aarch64) arch=arm64 ;;
  *) die "no build for $os on '$arch'" ;;
esac
[ "$os" = macos ] || [ -z "$app_dir_opt" ] || warn "--app-dir only applies on macOS; ignored"

# ---- what is already here --------------------------------------------------

# What a previous run of this script left behind, if anything. This is what a
# future `thinkterm update` reads to know that it may overwrite these files
# itself -- a deb, rpm, AppImage or Homebrew install writes no such file and
# belongs to its own updater. One key=value per line, readable from a shell
# or from Rust without a parser.
manifest="$sharedir/thinkterm/install-manifest"
prev_variant=""
prev_version=""
prev_app=""
if [ -f "$manifest" ]; then
  prev_variant=$(sed -n 's/^variant=//p' "$manifest" | head -n1)
  prev_version=$(sed -n 's/^version=//p' "$manifest" | head -n1)
  prev_app=$(sed -n 's/^app=//p' "$manifest" | head -n1)
  # The value goes to rm -rf below, so anything that is not a bundle path
  # -- a hand-edited or truncated line -- is treated as unknown.
  case "$prev_app" in */ThinkTerm.app) ;; *) prev_app="" ;; esac
fi

# Never guessed from the machine: a headless box may want the TUI, and a
# desktop may be somebody's SSH target. Ask when there is someone to ask.
# Under `curl | sh` stdin is the script itself, so the question goes through
# /dev/tty; with no terminal at all (a provisioning script) the flag is
# mandatory.
if [ -z "$variant" ]; then
  if [ -t 2 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
    if [ -n "$prev_variant" ]; then
      printf '%s\n' "ThinkTerm $prev_variant${prev_version:+ $prev_version} is installed under $prefix." > /dev/tty
    fi
    printf '%s\n' "Which ThinkTerm do you want on this machine?" \
      "  1) desktop  GUI + CLI + TUI + mux server" \
      "  2) server   CLI + TUI + mux server, no GUI (headless hosts, SSH targets)" > /dev/tty
    printf '%s' "Choice [1/2]: " > /dev/tty
    read -r choice < /dev/tty || choice=""
    case "$choice" in
      1 | d | desktop) variant=desktop ;;
      2 | s | server)  variant=server ;;
      *) die "no choice made; pass --desktop or --server" ;;
    esac
  else
    die "pass --desktop (GUI + CLI + TUI + mux server) or --server (the same without the GUI)"
  fi
fi

# ---- preflight -------------------------------------------------------------

if [ "$os" = linux ]; then
  # The binaries link against glibc; musl (Alpine) cannot run them at all,
  # and an older glibc fails at load time with a message that blames a
  # symbol version rather than the real cause. Check here, in words.
  glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || glibc=""
  if [ -z "$glibc" ]; then
    if ldd --version 2>&1 | grep -qi musl; then
      die "this host uses musl libc; the release tarballs are built against glibc. Build from source instead."
    fi
    warn "could not determine the glibc version; continuing, but the binaries need glibc >= $min_glibc"
  elif lowest=$(printf '%s\n%s\n' "$min_glibc" "$glibc" | sort -V 2>/dev/null | head -n1) \
      && [ -n "$lowest" ] && [ "$lowest" != "$min_glibc" ]; then
    die "glibc $glibc is too old: the release binaries need >= $min_glibc (Ubuntu 22.04, Debian 12, RHEL 10 or newer). Build from source instead."
  fi
  have tar || die "tar is required"
else
  have ditto || die "ditto is required (it ships with macOS)"
fi

if [ -z "$from" ]; then
  have curl || die "curl is required"
fi

if have sha256sum; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif have shasum; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  sha256() { return 1; }
fi

fetch() {
  curl -fsSL --retry 3 "$@"
}

# Where the app bundle goes on macOS. /Applications is writable by any admin
# user without sudo; a standard user gets ~/Applications, which Launchpad and
# Spotlight index just the same. The server variant keeps the bundle out of
# both -- the binaries inside it are what is wanted, not a Dock icon.
app_dir=""
if [ "$os" = macos ]; then
  if [ -n "$app_dir_opt" ]; then
    app_dir="$app_dir_opt"
  else
    case "$variant" in
      server) app_dir="$prefix/libexec" ;;
      *)
        if [ -w /Applications ]; then app_dir=/Applications; else app_dir="$HOME/Applications"; fi
        ;;
    esac
  fi
fi
app="$app_dir/ThinkTerm.app"

# ---- resolve the release ---------------------------------------------------

api="https://api.github.com/repos/$repo/releases"
asset=""
digest=""
url=""

asset_name() {
  case "$os/$variant" in
    linux/desktop) echo "thinkterm-$1-linux-$arch.tar.gz" ;;
    linux/server)  echo "thinkterm-server-$1-linux-$arch.tar.gz" ;;
    # One bundle serves both variants: it already holds every binary.
    macos/*)       echo "ThinkTerm-macos-$arch-$1.zip" ;;
  esac
}

if [ -z "$from" ]; then
  if [ -n "$tag" ]; then
    release_json=$(fetch "$api/tags/$tag") \
      || die "release '$tag' not found at https://github.com/$repo/releases"
  else
    release_json=$(fetch "$api/latest") \
      || die "could not look up the latest release (is the network up? does https://github.com/$repo have a release yet?)"
    tag=$(printf '%s' "$release_json" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
    [ -n "$tag" ] || die "could not read the release tag from the GitHub API response"
  fi

  # The tag as GitHub has it goes in the URL path; the filename is built from
  # the version ci/deploy.sh stamped, which never carries a leading v.
  version="${tag#v}"
  asset=$(asset_name "$version")
  url="https://github.com/$repo/releases/download/$tag/$asset"

  # GitHub stores a sha256 for every asset as it is uploaded and reports it
  # on the API as "sha256:<hex>". That is the only checksum the release
  # carries -- there is no checksums file -- so read it from the same JSON.
  # The asset object is the block between the "name" line naming our file and
  # the next "name" line, and "digest" sits inside it.
  digest=$(printf '%s' "$release_json" \
    | awk -v want="\"name\": \"$asset\"" '
        index($0, want)      { inside = 1; next }
        inside && /"name":/  { exit }
        inside && /"digest": *"sha256:/ {
          sub(/.*"digest": *"sha256:/, ""); sub(/".*/, ""); print; exit
        }')
  # Anything but a hex string means the field was absent or null: install
  # unverified with a warning rather than fail a good download against junk.
  case "$digest" in *[!0-9a-f]* | "") digest="" ;; esac
  if [ -z "$digest" ]; then
    printf '%s' "$release_json" | grep -qF "\"name\": \"$asset\"" \
      || die "release $version has no $asset; see https://github.com/$repo/releases/tag/$tag"
  fi
fi

say "ThinkTerm $variant${version:+ $version} for $os-$arch"
if [ -n "$prev_variant" ]; then
  case "$prev_variant/$variant" in
    server/desktop)  say "  was:    server${prev_version:+ $prev_version}; adding the GUI, everything else is kept" ;;
    desktop/server)  say "  was:    desktop${prev_version:+ $prev_version}; the GUI and its launcher will be removed" ;;
    *)               say "  was:    $prev_variant${prev_version:+ $prev_version}; reinstalling over it" ;;
  esac
fi
if [ -n "$from" ]; then
  say "  from:   $from"
else
  say "  from:   $url"
fi
say "  into:   $bindir"
[ -z "$app_dir" ] || say "  app:    $app"
if [ -z "$from" ] && [ -z "$digest" ]; then
  say "  verify: no digest available for this asset; the download will not be checked"
fi

if [ -n "$dry_run" ]; then
  say "dry run: stopping here"
  exit 0
fi

# ---- download and verify ---------------------------------------------------

tmp=$(mktemp -d "${TMPDIR:-/tmp}/thinkterm-install.XXXXXX")
# Staged files all live next to their destination as .<name>.tmp, so the trap
# can find them without a list that a prefix containing spaces would break.
cleanup() {
  rm -rf "$tmp"
  rm -f "$bindir"/.*.tmp 2>/dev/null || true
  [ -z "$app_dir" ] || rm -rf "$app_dir/.ThinkTerm.app.tmp" 2>/dev/null || true
  [ -z "$app_dir" ] || rm -rf "$app_dir/.ThinkTerm.app.old" 2>/dev/null || true
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

if [ -n "$from" ]; then
  [ -f "$from" ] || die "no such file: $from"
  archive="$from"
else
  archive="$tmp/$asset"
  say "downloading..."
  fetch -o "$archive" "$url" || die "download failed: $url"
  if [ -n "$digest" ]; then
    if got=$(sha256 "$archive"); then
      [ "$got" = "$digest" ] || die "checksum mismatch for $asset
  expected $digest
  got      $got
The download is corrupt or has been tampered with; nothing was installed."
    else
      warn "neither sha256sum nor shasum is available; the download was not verified"
    fi
  fi
fi

mkdir "$tmp/x"
case "$os" in
  linux)
    tar xzf "$archive" -C "$tmp/x" || die "could not unpack $archive"
    # The tarball holds exactly one top-level directory named after itself.
    root=$(find "$tmp/x" -mindepth 1 -maxdepth 1 -type d | head -n1)
    [ -n "$root" ] && [ -d "$root/bin" ] || die "unexpected tarball layout: no bin/ directory inside"
    if [ "$variant" = desktop ] && [ ! -f "$root/share/applications/com.roversx.thinkterm.desktop" ]; then
      die "that tarball is the server variant (no GUI inside); pass --server"
    fi
    if [ "$variant" = server ] && [ -f "$root/share/applications/com.roversx.thinkterm.desktop" ]; then
      die "that tarball is the desktop variant; pass --desktop"
    fi
    # With --from there was no release lookup, so the version comes from the
    # directory name ci/deploy.sh gave the tarball:
    # thinkterm[-server]-<ver>-linux-<arch>.
    if [ -z "$version" ]; then
      version=$(basename "$root" | sed -n 's/^thinkterm-\(server-\)\{0,1\}\(.*\)-linux-[^-]*$/\2/p')
    fi
    ;;
  macos)
    # ditto, not unzip: it is what packed the archive, and it restores the
    # resource forks and extended attributes the code signature covers.
    ditto -x -k "$archive" "$tmp/x" || die "could not unpack $archive"
    root=$(find "$tmp/x" -maxdepth 2 -name ThinkTerm.app -type d | head -n1)
    [ -n "$root" ] && [ -x "$root/Contents/MacOS/thinkterm" ] || die "unexpected zip layout: no ThinkTerm.app inside"
    if [ -z "$version" ]; then
      version=$(basename "$(dirname "$root")" | sed -n 's/^ThinkTerm-macos-\(arm64\|x86_64\)-\(.*\)$/\2/p')
    fi
    ;;
esac

# ---- GUI library preflight (Linux) -----------------------------------------

# A package manager would pull these in; a tarball cannot. Ask the dynamic
# loader what the GUI binary is missing on this exact machine, and name the
# package for it. Warn rather than stop: the CLI and mux server in the same
# tarball work regardless, and the user may be about to install the library.
missing_libs=""
if [ "$os" = linux ] && [ "$variant" = desktop ] && [ -x "$root/bin/thinkterm-gui" ] && have ldd; then
  missing_libs=$(ldd "$root/bin/thinkterm-gui" 2>/dev/null | awk '/not found/ {print $1}')
fi
# Debian/Ubuntu package first, Fedora/RHEL second, openSUSE third.
lib_package() {
  case "$1" in
    libxcb-keysyms*)      echo "libxcb-keysyms1 / xcb-util-keysyms / libxcb-keysyms1" ;;
    libxcb-ewmh*)         echo "libxcb-ewmh2 / xcb-util-wm / libxcb-ewmh2" ;;
    libxcb-icccm*)        echo "libxcb-icccm4 / xcb-util-wm / libxcb-icccm4" ;;
    libxcb-image*)        echo "libxcb-image0 / xcb-util-image / libxcb-image0" ;;
    libxcb-render-util*)  echo "libxcb-render-util0 / xcb-util-renderutil / libxcb-render-util0" ;;
    libxcb-util*)         echo "libxcb-util1 / xcb-util / libxcb-util1" ;;
    libxcb-randr*)        echo "libxcb-randr0 / libxcb / libxcb-randr0" ;;
    libxcb-render*)       echo "libxcb-render0 / libxcb / libxcb-render0" ;;
    libxcb-xkb*)          echo "libxcb-xkb1 / libxcb / libxcb-xkb1" ;;
    libX11-xcb*)          echo "libx11-xcb1 / libX11-xcb / libX11-xcb1" ;;
    libxcb.so*)           echo "libxcb1 / libxcb / libxcb1" ;;
    libxcb-*)             echo "libxcb1 (or the matching libxcb-*0 package) / libxcb / libxcb1" ;;
    libxkbcommon-x11*)    echo "libxkbcommon-x11-0 / libxkbcommon-x11 / libxkbcommon-x11-0" ;;
    libxkbcommon*)        echo "libxkbcommon0 / libxkbcommon / libxkbcommon0" ;;
    libwayland-client*)   echo "libwayland-client0 / libwayland-client / libwayland-client0" ;;
    libwayland-egl*)      echo "libwayland-egl1 / libwayland-egl / libwayland-egl1" ;;
    libwayland-cursor*)   echo "libwayland-cursor0 / libwayland-cursor / libwayland-cursor0" ;;
    libEGL*)              echo "libegl1 / mesa-libEGL / Mesa-libEGL1" ;;
    libfontconfig*)       echo "libfontconfig1 / fontconfig / fontconfig" ;;
    libfreetype*)         echo "libfreetype6 / freetype / libfreetype6" ;;
    libdbus*)             echo "libdbus-1-3 / dbus-libs / libdbus-1-3" ;;
    libssl* | libcrypto*) echo "libssl3 (libssl3t64 on Ubuntu 24.04+) / openssl-libs / libopenssl3" ;;
    *)                    echo "(no package name known; search your distro for $1)" ;;
  esac
}

# ---- install ---------------------------------------------------------------

mkdir -p "$bindir" "$sharedir/thinkterm" \
  "$sharedir/bash-completion/completions" \
  "$sharedir/zsh/site-functions" \
  "$sharedir/fish/vendor_completions.d"

# Per-platform: where the binaries, completions and shell integration come
# from inside the unpacked archive.
case "$os" in
  linux)
    src_bin="$root/bin"
    src_completion="$root/share/shell-completion"
    src_integration="$root/share/shell-integration"
    src_doc="$root"
    ;;
  macos)
    src_bin="$root/Contents/MacOS"
    src_completion="$root/Contents/Resources/shell-completion"
    src_integration="$root/Contents/Resources"
    src_doc="$root/Contents/Resources"
    ;;
esac

say
say "installed:"

if [ "$os" = macos ]; then
  # The bundle is moved whole so the signature and notarization ticket stay
  # intact. The old bundle is renamed aside, never deleted first: if it
  # cannot be moved (a locked file, someone else's ownership) the script
  # stops with both the old install and the new download intact, and a
  # running ThinkTerm keeps its files under the renamed path until it quits.
  # The command-line tools are symlinks into the bundle: a copy would run
  # outside the seal, and the wezterm shim locates thinkterm next to itself.
  mkdir -p "$app_dir"
  rm -rf "$app_dir/.ThinkTerm.app.tmp" "$app_dir/.ThinkTerm.app.old"
  mv "$root" "$app_dir/.ThinkTerm.app.tmp"
  if [ -e "$app" ]; then
    mv "$app" "$app_dir/.ThinkTerm.app.old" || die "could not move the existing $app aside; nothing was changed"
  fi
  mv "$app_dir/.ThinkTerm.app.tmp" "$app"
  say "  $app"
  # The new bundle is in place at this point, so a leftover that will not
  # delete (a locked file inside the old one) is worth a note, not a failure.
  if ! rm -rf "$app_dir/.ThinkTerm.app.old" 2>/dev/null; then
    warn "could not remove the old bundle left at $app_dir/.ThinkTerm.app.old; delete it by hand"
  fi
  # The previous run put the bundle somewhere else (the other variant's
  # location, or another --app-dir): one ThinkTerm.app per machine.
  if [ -n "$prev_app" ] && [ "$prev_app" != "$app" ] && [ -d "$prev_app" ]; then
    say "removing the previous bundle at $prev_app"
    rm -rf "$prev_app"
  fi
  for name in thinkterm wezterm thinkterm-mux-server strip-ansi-escapes; do
    [ -x "$app/Contents/MacOS/$name" ] || continue
    ln -sfn "$app/Contents/MacOS/$name" "$bindir/$name"
    say "  $bindir/$name -> ThinkTerm.app"
  done
  src_completion="$app/Contents/Resources/shell-completion"
  src_integration="$app/Contents/Resources"
  src_doc="$app/Contents/Resources"
else
  # Every binary is copied under a staging name first, and only then renamed
  # into place: a failure part-way (disk full, read-only prefix) leaves the
  # existing install untouched, the rename is atomic so no reader ever sees
  # a half-written binary, and a running thinkterm-mux-server keeps its old
  # inode. Paths are not word-split into a list: a prefix may contain spaces.
  for f in "$src_bin"/*; do
    name=$(basename "$f")
    cp "$f" "$bindir/.$name.tmp"
    chmod 755 "$bindir/.$name.tmp"
  done

  # The variants are mutually exclusive: same CLI, same mux server, and only
  # one of them may own those names in a prefix. Swapping is the one job a
  # package manager could not do for a bare tarball, so it is done here.
  desktop_entry="$sharedir/applications/com.roversx.thinkterm.desktop"
  icon="$sharedir/icons/hicolor/128x128/apps/com.roversx.thinkterm.png"
  if [ "$variant" = server ]; then
    for other in "$bindir/thinkterm-gui" "$bindir/open-thinkterm-here" "$bindir/open-wezterm-here" \
                 "$desktop_entry" "$icon"; do
      if [ -e "$other" ]; then
        say "removing $other (desktop variant, replaced by --server)"
        rm -f "$other"
      fi
    done
  fi

  for f in "$src_bin"/*; do
    name=$(basename "$f")
    mv -f "$bindir/.$name.tmp" "$bindir/$name"
    say "  $bindir/$name"
  done

  if [ "$variant" = desktop ]; then
    # The launcher runs with the session's PATH, which on many distros does
    # not include ~/.local/bin, so the entry points at the binary by absolute
    # path. Spliced with substr rather than sed or awk's sub(): the path is
    # data, and in either replacement a prefix containing & or the delimiter
    # would be interpreted; substr starts after the 15 characters of
    # "Exec=thinkterm ". Exec is
    # field-split by the launcher, so the path is double-quoted there, which
    # the spec allows; TryExec is taken whole and must not be.
    mkdir -p "$(dirname "$desktop_entry")" "$(dirname "$icon")"
    awk -v exe="$bindir/thinkterm" '
      /^Exec=thinkterm /   { $0 = "Exec=\"" exe "\" " substr($0, 16) }
      /^TryExec=thinkterm$/ { $0 = "TryExec=" exe }
      { print }' "$root/share/applications/com.roversx.thinkterm.desktop" > "$desktop_entry"
    cp "$root/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png" "$icon"
    say "  $desktop_entry"
    say "  $icon"
  fi

  # Refresh the menu caches when the tools exist, after an install or a
  # removal alike; the entry works without them, so failures are ignored.
  have update-desktop-database && update-desktop-database "$sharedir/applications" 2>/dev/null || true
  have gtk-update-icon-cache && gtk-update-icon-cache -q -t "$sharedir/icons/hicolor" 2>/dev/null || true
fi

[ -f "$src_completion/bash" ] && cp "$src_completion/bash" "$sharedir/bash-completion/completions/thinkterm"
[ -f "$src_completion/zsh" ]  && cp "$src_completion/zsh"  "$sharedir/zsh/site-functions/_thinkterm"
[ -f "$src_completion/fish" ] && cp "$src_completion/fish" "$sharedir/fish/vendor_completions.d/thinkterm.fish"
if [ -f "$src_integration/wezterm.sh" ]; then
  mkdir -p "$sharedir/thinkterm/shell-integration"
  cp "$src_integration/wezterm.sh" "$sharedir/thinkterm/shell-integration/wezterm.sh"
fi
for doc in NOTICE LICENSE.md LICENSE-MIT; do
  [ -f "$src_doc/$doc" ] && cp "$src_doc/$doc" "$sharedir/thinkterm/$doc"
done

# Written last, so it only ever describes a completed install.
{
  echo "variant=$variant"
  echo "version=$version"
  echo "os=$os"
  echo "arch=$arch"
  echo "prefix=$prefix"
  [ -z "$app_dir" ] || echo "app=$app"
  echo "installed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "installer=install.sh"
} > "$manifest"

# ---- report ----------------------------------------------------------------

say "  $sharedir/thinkterm/ (licenses, shell integration, install-manifest)"
say "  $sharedir/{bash-completion,zsh,fish}/... (completions)"

# The binaries were swapped under it, so a mux server that is still running
# is the old build. Client and server refuse to talk across a protocol
# change, so the next `thinkterm connect` would fail with a version error
# unless it is restarted.
if have pgrep && pgrep -u "$(id -u)" -x thinkterm-mux-server >/dev/null 2>&1; then
  say
  say "A thinkterm-mux-server from before this install is still running. Restart it to pick up the new"
  say "version; until then a newer CLI or GUI may refuse to connect to it."
fi
if [ "$os" = macos ] && have pgrep && pgrep -u "$(id -u)" -x thinkterm-gui >/dev/null 2>&1; then
  say
  say "ThinkTerm is running; the new version starts the next time you open it."
fi

# Nothing is done to the shell startup files: editing them is the user's
# call. But ~/.local/bin on Debian and Ubuntu only enters PATH at the next
# login, and only if it existed then -- so say so.
case ":$PATH:" in
  *":$bindir:"*) ;;
  *)
    say
    say "$bindir is not on your PATH. Add this to your shell startup file, then open a new shell:"
    say "  export PATH=\"$bindir:\$PATH\""
    ;;
esac

if [ -f "$sharedir/thinkterm/shell-integration/wezterm.sh" ]; then
  say
  say "Shell integration (prompt marks, cwd tracking) is optional; to turn it on, add to your shell startup file:"
  say "  . \"$sharedir/thinkterm/shell-integration/wezterm.sh\""
fi

if system=$(command -v thinkterm 2>/dev/null) && [ "$system" != "$bindir/thinkterm" ]; then
  say
  say "note: another thinkterm is already on PATH at $system and comes first; \`thinkterm\` will run that one."
fi

if [ -n "$missing_libs" ]; then
  say
  say "WARNING: thinkterm-gui needs libraries this machine does not have. The CLI and mux server"
  say "work without them; the GUI will not start until they are installed."
  say "  library                        package (apt / dnf / zypper)"
  for lib in $missing_libs; do
    printf '  %-30s %s\n' "$lib" "$(lib_package "$lib")"
  done
fi

if [ "$variant" = server ]; then
  say
  say "The mux server needs no root and no service file to run: \`thinkterm-mux-server --daemonize\`."
  if [ "$os" = linux ]; then
    say "To keep it running after you log out from SSH: \`loginctl enable-linger \$USER\`."
  fi
fi
