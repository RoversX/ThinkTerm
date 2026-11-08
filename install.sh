#!/bin/sh
# thinkterm-installer-v1
# ThinkTerm installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh | sh
#
# Downloads the release for this machine, verifies it against GitHub's digest
# or the signed release inventory, and installs it for the current user -- no root,
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
#   --sha256 HASH     Verify a --from archive against this SHA-256.
#   --mirror M        auto tries GitHub, then ranks mirrors if GitHub is slow;
#                     direct uses only GitHub; a mirror host tries that host,
#                     then GitHub. Mirrors require a trusted checksum.
#   --dry-run         Say what would be done and stop before touching the disk.
#
# GITHUB_TOKEN in the environment is sent with every request. It is what lets
# the script work against a private repository -- the API and the release
# assets both refuse anonymous access there -- and lifts the anonymous API
# rate limit on a public one. This script is something you run yourself; the
# app's own background update check deliberately does not read GITHUB_TOKEN
# and takes THINKTERM_UPDATE_TOKEN instead (see PRIVACY.md).
#
# Windows has an installer on the releases page; this script stops there.
#
# Deliberately not here: version comparison, uninstall. Re-running the script
# is the upgrade: it overwrites whatever is installed with the release it
# fetched, and going from --server to --desktop is the same run plus the GUI.
# Removing ThinkTerm is deleting what it put in place: ThinkTerm.app on
# macOS, the commands in bin/, share/thinkterm/ with the completions beside
# it, and on Linux the launcher and icon. A manifest under share/thinkterm/ records what was
# installed for a future `thinkterm update`.

set -eu

repo="RoversX/thinkterm"
# The oldest glibc each variant's Linux binaries load on: the newest glibc
# symbol version any of them needs. ci/check-glibc.sh holds every release to
# these, and thinkterm-update carries the same numbers.
min_glibc_server="2.31"
min_glibc_desktop="2.35"   # the GUI's alone; the rest need min_glibc_server_main
# The server is built twice. From this glibc on, a host gets the build made
# beside the desktop's, which links the host's OpenSSL 3; an older host has
# only OpenSSL 1.1 and gets the one made on Ubuntu 20.04 with OpenSSL built in.
min_glibc_server_main="2.34"
# Hosts probed in parallel when GitHub is slow under --mirror auto.
# thinkterm-update carries the same list for Settings.
mirrors="ghfast.top gh-proxy.org ghproxy.net gh.llkk.cc"
# Bytes a second under which a source that is not the last one is given up.
min_rate=307200

variant=""
tag=""
version=""
prefix="${HOME}/.local"
app_dir_opt=""
from=""
expected_sha256=""
release_info=""
mirror="auto"
dry_run=""
old_glibc=""

usage() {
  cat <<'EOF'
Install ThinkTerm on Linux or macOS.

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
  --sha256 HASH     Verify a --from archive against this SHA-256.
  --mirror M        auto (default) tries GitHub, then probes mirrors in parallel
                    if it is slow: ghfast.top gh-proxy.org ghproxy.net gh.llkk.cc.
                    direct uses only GitHub; a mirror host tries it, then GitHub.
                    Mirrors require a SHA-256 from GitHub or signed metadata.
  --dry-run         Say what would be done and stop before touching the disk.
EOF
}

# ---- output ----------------------------------------------------------------

# What the terminal can show, decided once. Logged or piped output gets plain
# lines; a terminal gets colour, and, when it can redraw a line in place,
# spinners and a download bar. The opening wordmark and the bar need UTF-8.
# 24-bit colour only where COLORTERM promises it -- Terminal.app does not, and
# draws box-drawing glyphs badly, so its bar is coloured spaces instead.
esc=$(printf '\033')
tty_out=""
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then tty_out=1; fi
ui_color=""
if [ -n "$tty_out" ] && [ -z "${NO_COLOR:-}" ]; then ui_color=1; fi
err_color=""
if [ -t 2 ] && [ "${TERM:-dumb}" != dumb ] && [ -z "${NO_COLOR:-}" ]; then err_color=1; fi
# Redrawing waits between frames, and POSIX sleep only takes whole seconds.
anim=""
if [ -n "$tty_out" ] && sleep 0.01 2>/dev/null; then anim=1; fi
unicode=""
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in *UTF-8* | *utf-8* | *UTF8* | *utf8*) unicode=1 ;; esac
case "${TERM_PROGRAM:-}" in Apple_Terminal | iTerm.app | vscode | WezTerm | ThinkTerm | ghostty) unicode=1 ;; esac
palette=basic
if [ -n "$ui_color" ]; then
  case "${COLORTERM:-}" in truecolor | 24bit) palette=truecolor ;; esac
  if [ "$palette" = basic ]; then
    case "${TERM:-}:${TERM_PROGRAM:-}" in *256color*:* | *:Apple_Terminal) palette=256 ;; esac
  fi
fi
cols=$({ stty size </dev/tty; } 2>/dev/null | cut -d' ' -f2) || cols=""
case "$cols" in "" | *[!0-9]*) cols=${COLUMNS:-80} ;; esac
# Spinners and the bar draw on the terminal even inside $(...).
exec 4>&1

# One colour for each palette: truecolor "r;g;b", the nearest 256-colour
# index, and a basic SGR code.
color() {
  if [ -z "$ui_color" ]; then return; fi
  case "$palette" in
    truecolor) printf '%s[38;2;%sm' "$esc" "$1" ;;
    256)       printf '%s[38;5;%sm' "$esc" "$2" ;;
    *)         printf '%s[%sm' "$esc" "$3" ;;
  esac
}
ui_reset=""; ui_bold=""; ui_dim=""
if [ -n "$ui_color" ]; then ui_reset="$esc[0m"; ui_bold="$esc[1m"; ui_dim="$esc[2m"; fi
ui_accent=$(color '150;160;255' 147 34)
ui_success=$(color '120;210;140' 114 32)
ui_warning=$(color '240;190;90' 221 33)
err_reset=""; err_warning=""; err_failure=""
if [ -n "$err_color" ]; then err_reset="$esc[0m"; err_warning="$esc[33m"; err_failure="$esc[31m"; fi
if [ -n "$unicode" ]; then
  mark_ok="✓"; mark_fail="✗"; dot="·"; arrow="→"
else
  mark_ok="+"; mark_fail="x"; dot="-"; arrow="->"
fi

say()  { printf '%s\n' "$*"; }
ok()   { printf '  %s%s%s %s\n' "$ui_success" "$mark_ok" "$ui_reset" "$*"; }
# A line under the step it belongs to.
note() { printf '    %s%s%s\n' "$ui_dim" "$*" "$ui_reset"; }
warn() { printf '  %s!%s %s\n' "$err_warning" "$err_reset" "$*" >&2; }
die()  { printf '  %s%s%s %s\n' "$err_failure" "$mark_fail" "$err_reset" "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# A path as the user would type it.
pretty() {
  case "$1" in
    "$HOME") printf '~' ;;
    "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

show_cursor() { [ -z "$anim" ] || printf '%s[?25h' "$esc" >&4; }
hide_cursor() { [ -z "$anim" ] || printf '%s[?25l' "$esc" >&4; }
# What a command running under a spinner or the bar prints on stderr is held
# here, so it cannot land in the middle of a line being redrawn.
errlog=""
if [ -n "$anim" ]; then
  errlog=$(mktemp "${TMPDIR:-/tmp}/thinkterm-install-err.XXXXXX") || anim=""
fi
leave() { show_cursor; [ -z "$errlog" ] || rm -f "$errlog"; }
trap leave EXIT
trap 'leave; exit 130' INT
trap 'leave; exit 143' TERM
# Prints the first line of that, once the redrawn line is gone, and empties it.
show_error() {
  if [ -s "$errlog" ]; then
    printf '    %s%s%s\n' "$ui_dim" "$(head -n1 "$errlog")" "$ui_reset" >&2
  fi
  : > "$errlog"
}

spinner_frame() {
  if [ -n "$unicode" ]; then
    case $(($1 % 10)) in
      0) printf '⠋' ;; 1) printf '⠙' ;; 2) printf '⠹' ;; 3) printf '⠸' ;; 4) printf '⠼' ;;
      5) printf '⠴' ;; 6) printf '⠦' ;; 7) printf '⠧' ;; 8) printf '⠇' ;; *) printf '⠏' ;;
    esac
  else
    case $(($1 % 4)) in 0) printf '-' ;; 1) printf '\\' ;; 2) printf '|' ;; *) printf '/' ;; esac
  fi
}

# Runs a command with a spinner beside LABEL, and returns its status. Its
# output goes wherever the caller sends it; the spinner draws on fd 4.
bg_pid=""
with_spinner() {
  label=$1; shift
  if [ -z "$anim" ]; then
    printf '  %s>%s %s...\n' "$ui_accent" "$ui_reset" "$label" >&4
    "$@"
    return
  fi
  "$@" 2>"$errlog" &
  bg_pid=$!
  hide_cursor
  i=0
  while kill -0 "$bg_pid" 2>/dev/null; do
    printf '\r%s[K  %s%s%s %s' "$esc" "$ui_accent" "$(spinner_frame "$i")" "$ui_reset" "$label" >&4
    i=$((i + 1))
    sleep 0.08
  done
  printf '\r%s[K' "$esc" >&4
  show_cursor
  if wait "$bg_pid"; then status=0; else status=$?; fi
  bg_pid=""
  [ "$status" -eq 0 ] || show_error
  return "$status"
}

# THINKTERM in a 3x5 pixel font, two pixels to a character cell with the
# half blocks, shaded lavender to blue from left to right. Draws the first
# $1 letters with a cursor after them when $2 is 1.
wordmark() {
  awk -v shown="$1" -v cursor="$2" -v palette="$palette" -v color="$ui_color" -v esc="$esc" -v clear="$tty_out" '
    function shade(x,   f, r, g, b) {
      if (!color) return ""
      f = (x - 1) / (width - 1)
      r = int(200 - 116 * f); g = int(206 - 106 * f); b = int(255 - 5 * f)
      return rgb(r, g, b)
    }
    function rgb(r, g, b,   q) {
      if (palette == "truecolor") return esc "[38;2;" r ";" g ";" b "m"
      if (palette == "256") return esc "[38;5;" (16 + 36 * cube(r) + 6 * cube(g) + cube(b)) "m"
      return esc "[34m"
    }
    function cube(v) { return v < 48 ? 0 : (v < 115 ? 1 : int((v - 35) / 40)) }
    BEGIN {
      row[1] = "###.#.#.###.#..#.#.#.###.###.##..#...#"
      row[2] = ".#..#.#..#..##.#.#.#..#..#...#.#.##.##"
      row[3] = ".#..###..#..#.##.##...#..##..##..#.#.#"
      row[4] = ".#..#.#..#..#..#.#.#..#..#...#.#.#...#"
      row[5] = ".#..#.#.###.#..#.#.#..#..###.#.#.#...#"
      split("1 5 9 13 18 22 26 30 34", start, " ")
      width = length(row[1])
      edge = shown >= 9 ? width + 2 : start[shown + 1]
      reset = color ? esc "[0m" : ""
      for (line = 0; line < 3; line++) {
        top = 2 * line; bottom = top + 1   # pixel row 0 is the blank above
        out = ""
        for (x = 1; x <= width + 2; x++) {
          if (cursor && x == edge) {
            t = top >= 1; b = bottom <= 5; paint = color ? rgb(225, 228, 255) : ""
          } else {
            t = x < edge && top >= 1 && substr(row[top], x, 1) == "#"
            b = x < edge && bottom <= 5 && substr(row[bottom], x, 1) == "#"
            paint = shade(x)
          }
          if (t && b) out = out paint "█" reset
          else if (t) out = out paint "▀" reset
          else if (b) out = out paint "▄" reset
          else out = out " "
        }
        sub(/ +$/, "", out)
        if (clear) printf "  %s%s[K\n", out, esc
        else printf "  %s\n", out
      }
    }'
}

# The wordmark types itself out on a terminal that can redraw; elsewhere it
# is printed once, or, without UTF-8 or room, the name in plain text.
banner() {
  if [ "${THINKTERM_INSTALL_PROGRESS:-}" = 1 ] && [ -z "$tty_out" ]; then
    say; wordmark 9 0; say '  installer'; say
    return
  fi
  if [ -z "$tty_out" ] || [ -z "$unicode" ] || [ "$cols" -lt 44 ]; then
    printf '\n  %sThinkTerm%s %sinstaller%s\n\n' "$ui_bold" "$ui_reset" "$ui_dim" "$ui_reset"
    return
  fi
  if [ -z "$anim" ] || [ "${1:-}" = still ]; then
    say; wordmark 9 0
  else
    hide_cursor
    printf '%s[2J%s[H\n' "$esc" "$esc"
    wordmark 0 1; sleep 0.3
    for n in 1 2 3 4 5 6 7 8 9; do
      printf '%s[2H' "$esc"; wordmark "$n" 1; sleep 0.06
    done
    for c in 0 1 0 1 0; do
      printf '%s[2H' "$esc"; wordmark 9 "$c"; sleep 0.22
    done
    show_cursor
  fi
  printf '  %sinstaller%s\n\n' "$ui_dim" "$ui_reset"
}

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
    --sha256)   [ $# -ge 2 ] || die "--sha256 needs a value"; expected_sha256="$2"; shift ;;
    --sha256=*) expected_sha256="${1#--sha256=}" ;;
    # Trusted metadata delivered by the client over its authenticated SSH connection.
    --release-info) [ $# -ge 2 ] || die "--release-info needs a file"; release_info="$2"; shift ;;
    --mirror)   [ $# -ge 2 ] || die "--mirror needs a value"; mirror="$2"; shift ;;
    --mirror=*) mirror="${1#--mirror=}" ;;
    --dry-run)  dry_run=1 ;;
    -h|--help)  banner still; usage; exit 0 ;;
    *) die "unknown option '$1' (try --help)" ;;
  esac
  shift
done

[ -n "$prefix" ] || die "--prefix needs a directory"
case "$tag" in *[!A-Za-z0-9._+-]*) die "invalid release version" ;; esac
if [ -n "$expected_sha256" ]; then
  [ -n "$from" ] || die "--sha256 requires --from"
  case "$expected_sha256" in *[!0-9a-f]*) die "invalid SHA-256" ;; esac
  [ "${#expected_sha256}" -eq 64 ] || die "invalid SHA-256"
fi
tag_requested=""
[ -z "$tag" ] || tag_requested=1
# Where the archive is fetched from, in order: "github" or a mirror's host.
case "$mirror" in
  direct) sources="github" ;;
  auto)   sources="github $mirrors" ;;
  *)
    mirror="${mirror#https://}"; mirror="${mirror%/}"
    case "$mirror" in
      "" | *[!A-Za-z0-9.-]*) die "--mirror takes direct, auto or a host name, not '$mirror'" ;;
    esac
    sources="$mirror github"
    ;;
esac
bindir="$prefix/bin"
sharedir="$prefix/share"

banner

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

# A desktop app this prefix does not own: a distro package, or an install
# under another prefix. The server variant never touches it, and it is worth
# saying so, because the two then go separate ways: its launcher finds
# `thinkterm` through PATH, where ~/.local/bin usually comes first, and this
# install cannot start a GUI. Not on macOS, where an app runs its own binaries
# and updates itself.
other_gui=""
if [ "$os" = linux ]; then
  if other_gui=$(command -v thinkterm-gui 2>/dev/null) && [ ! "$other_gui" -ef "$bindir/thinkterm-gui" ]; then
    :
  elif [ -f /usr/share/applications/com.roversx.thinkterm.desktop ]; then
    other_gui=/usr/share/applications/com.roversx.thinkterm.desktop
  else
    other_gui=""
  fi
fi

# Never guessed from the machine: a headless box may want the TUI, and a
# desktop may be somebody's SSH target. Ask when there is someone to ask.
# Under `curl | sh` stdin is the script itself, so the question goes through
# /dev/tty; with no terminal at all (a provisioning script) the flag is
# mandatory.
if [ -z "$variant" ]; then
  if [ -t 2 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
    if [ -n "$prev_variant" ]; then
      note "ThinkTerm $prev_variant${prev_version:+ $prev_version} is installed under $(pretty "$prefix")." > /dev/tty
    fi
    if [ -n "$other_gui" ]; then
      note "The desktop app is also at $(pretty "$other_gui"); the server variant leaves it at its own version." > /dev/tty
    fi
    printf '  %sWhich ThinkTerm do you want on this machine?%s\n' "$ui_bold" "$ui_reset" > /dev/tty
    printf '    %s1%s  desktop  %sGUI + CLI + TUI + mux server%s\n' "$ui_accent" "$ui_reset" "$ui_dim" "$ui_reset" > /dev/tty
    printf '    %s2%s  server   %sCLI + TUI + mux server, no GUI (headless hosts, SSH targets)%s\n' \
      "$ui_accent" "$ui_reset" "$ui_dim" "$ui_reset" > /dev/tty
    printf '  Choice %s[1/2]%s: ' "$ui_dim" "$ui_reset" > /dev/tty
    read -r choice < /dev/tty || choice=""
    printf '\n' > /dev/tty
    case "$choice" in
      1 | d | desktop) variant=desktop ;;
      2 | s | server)  variant=server ;;
      *) die "no choice made; pass --desktop or --server" ;;
    esac
  else
    die "pass --desktop (GUI + CLI + TUI + mux server) or --server (the same without the GUI)"
  fi
fi
case "$os" in linux) os_name=Linux ;; *) os_name=macOS ;; esac
ok "$os_name $arch $dot $variant"

# ---- preflight -------------------------------------------------------------

if [ "$os" = linux ]; then
  # The binaries link against glibc; musl (Alpine) cannot run them at all,
  # and an older glibc fails at load time with a message that blames a
  # symbol version rather than the real cause. Check here, in words.
  if [ "$variant" = server ]; then
    min_glibc=$min_glibc_server
    newer="Ubuntu 20.04, Debian 11, RHEL 9"
  else
    min_glibc=$min_glibc_desktop
    newer="Ubuntu 22.04, Debian 12, RHEL 10"
  fi
  glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || glibc=""
  if [ -z "$glibc" ]; then
    if ldd --version 2>&1 | grep -qi musl; then
      die "this host uses musl libc; the release tarballs are built against glibc. Build from source instead."
    fi
    warn "could not determine the glibc version; continuing, but the $variant binaries need glibc >= $min_glibc"
  elif lowest=$(printf '%s\n%s\n' "$min_glibc" "$glibc" | sort -V 2>/dev/null | head -n1) \
      && [ -n "$lowest" ] && [ "$lowest" != "$min_glibc" ]; then
    also=""
    if [ "$variant" = desktop ] && [ "$(printf '%s\n%s\n' "$min_glibc_server" "$glibc" \
        | sort -V 2>/dev/null | head -n1)" = "$min_glibc_server" ]; then
      also=" The server variant (--server) runs on it."
    fi
    die "glibc $glibc is too old: the $variant binaries need >= $min_glibc ($newer or newer). Build from source instead.$also"
  elif [ "$variant" = server ] && [ "$(printf '%s\n%s\n' "$min_glibc_server_main" "$glibc" \
      | sort -V 2>/dev/null | head -n1)" != "$min_glibc_server_main" ]; then
    old_glibc=1
    note "glibc $glibc: the server built for older glibc, with OpenSSL built in"
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

fetch() (
  ulimit -f 2048 || exit 1
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    exec curl --disable -fsSL --proto =https --proto-redir =https --connect-timeout 3 --max-time 8 --retry 0 -H "Authorization: Bearer $GITHUB_TOKEN" "$@"
  else
    exec curl --disable -fsSL --proto =https --proto-redir =https --connect-timeout 3 --max-time 8 --retry 0 "$@"
  fi
)

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

tmp=$(mktemp -d "${TMPDIR:-/tmp}/thinkterm-install.XXXXXX")
# Linux binaries are staged on the destination filesystem, in a directory
# owned by this invocation. Never glob temporary files in the shared bin/.
bin_stage=""
app_staging=""
probe_pids=""
cleanup() {
  [ -z "$bg_pid" ] || kill "$bg_pid" 2>/dev/null || true
  for pid in $probe_pids; do kill "$pid" 2>/dev/null || true; done
  for pid in $probe_pids; do wait "$pid" 2>/dev/null || true; done
  leave
  rm -rf "$tmp"
  [ -z "$bin_stage" ] || rm -rf "$bin_stage" 2>/dev/null || true
  [ -z "$app_staging" ] || rm -rf "$app_dir/.ThinkTerm.app.tmp" 2>/dev/null || true
  [ -z "$app_staging" ] || rm -rf "$app_dir/.ThinkTerm.app.old" 2>/dev/null || true
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# ---- resolve the release ---------------------------------------------------

api="https://api.github.com/repos/$repo/releases"
asset=""
digest=""
url=""
asset_api_url=""
size=""

asset_name() {
  case "$os/$variant" in
    linux/desktop) echo "thinkterm-$1-linux-$arch.tar.gz" ;;
    linux/server)  echo "thinkterm-server-$1-linux-$arch${old_glibc:+-glibc$min_glibc_server}.tar.gz" ;;
    # One bundle serves both variants: it already holds every binary.
    macos/*)       echo "ThinkTerm-macos-$arch-$1.zip" ;;
  esac
}

# This public key is shared with assets/release-signing.pub, which the client
# embeds. Private signing material is only held by the release workflow.
release_signing_key='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILio64rghh4lVquJuwWvNpMVnmV78OnXQmKYDH8Z83t0 thinkterm-release'

validate_release_info() {
  [ -f "$1" ] && [ "$(wc -c < "$1")" -le 1048576 ] || return 1
  LC_ALL=C awk -v version="$tag" -v missing="$2" '
    NR == 1 { if ($0 != "thinkterm-release-v1") exit 1; next }
    NR == 2 { if ($0 != "version " version) exit 1; next }
    {
      if (NF != 3 || $0 != $1 " " $2 " " $3) exit 1
      if (!(missing && $1 == "-") && (length($1) != 64 || $1 ~ /[^0-9a-f]/)) exit 1
      if ($2 !~ /^[0-9]+$/ || $2 + 0 <= 0 || $2 + 0 > 8589934592) exit 1
      if (length($3) > 200 || $3 !~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/ || seen[$3]++) exit 1
    }
    END { if (NR < 3) exit 1 }
  ' "$1"
}

# No token goes to a mirror. Time and file limits apply even when an older
# curl cannot enforce --max-filesize on a chunked response.
fetch_metadata() (
  ulimit -f 2048 || exit 1
  curl --disable -fsSL --proto =https --proto-redir =https --connect-timeout 3 \
    --max-time 8 --retry 0 --max-filesize 1048576 -o "$2" "$1"
)

signed_release_info() {
  have ssh-keygen || { warn "ssh-keygen is unavailable; signed metadata cannot be verified"; return 1; }
  printf 'thinkterm-release namespaces="thinkterm-release" %s\n' "$release_signing_key" > "$tmp/signers"
  invalid_metadata=""
  metadata_sources="$sources"
  [ -z "${GITHUB_TOKEN:-}" ] || metadata_sources=github
  for metadata_source in $metadata_sources; do
    metadata_url="https://github.com/$repo/releases/download/$tag/thinkterm-release.txt"
    [ "$metadata_source" = github ] || metadata_url="https://$metadata_source/$metadata_url"
    if ! fetch_metadata "$metadata_url" "$tmp/release-info" 2>/dev/null \
        || ! fetch_metadata "$metadata_url.sig" "$tmp/release-info.sig" 2>/dev/null; then
      continue
    fi
    if ! ssh-keygen -Y verify -f "$tmp/signers" -I thinkterm-release -n thinkterm-release \
        -s "$tmp/release-info.sig" < "$tmp/release-info" > "$tmp/verify.log" 2>&1; then
      # An old OpenSSH cannot verify SSHSIG. It must not authorize a mirror.
      if grep -Ei 'unknown option|illegal option|unsupported operation|invalid option' "$tmp/verify.log" >/dev/null; then
        warn "ssh-keygen does not support release signatures; using GitHub only"
        return 1
      fi
      invalid_metadata=1
      warn "release signature verification failed from $metadata_source"
      continue
    fi
    if ! validate_release_info "$tmp/release-info" 0; then
      invalid_metadata=1
      warn "signed release metadata has an invalid format or a different version"
      continue
    fi
    release_info="$tmp/release-info"
    return 0
  done
  [ -z "$invalid_metadata" ] || return 2
  return 1
}

if [ -z "$from" ]; then
  release_json=""
  if [ -n "$release_info" ]; then
    [ -n "$tag" ] || die "client release metadata requires --version"
    validate_release_info "$release_info" 1 || die "invalid client release metadata or version mismatch"
  else
    api_path=latest
    [ -z "$tag" ] || api_path="tags/$tag"
    api_status=""
    if api_status=$(with_spinner "looking up release ${tag:-latest}" fetch --max-filesize 1048576 \
        -o "$tmp/release.json" -w '%{http_code}' "$api/$api_path"); then
      release_json=$(cat "$tmp/release.json")
      if [ -z "$tag" ]; then
        tag=$(printf '%s' "$release_json" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
        [ -n "$tag" ] || die "could not read the release tag from the GitHub API response"
      fi
    else
      case "$api_status" in
        404) warn "GitHub API did not find release ${tag:-latest} (HTTP 404)" ;;
        403|429) warn "GitHub API refused the lookup or rate-limited it (HTTP $api_status)" ;;
        *) warn "could not reach GitHub API or complete the lookup (HTTP ${api_status:-unknown})" ;;
      esac
      [ -n "$tag" ] || die "cannot resolve the latest release; retry or pass --version X"
      if signed_release_info; then
        ok "verified release metadata"
      else
        metadata_status=$?
        [ "$metadata_status" -ne 2 ] || die "release metadata verification failed; nothing was installed"
        warn "no verifiable release metadata is available; trying the GitHub archive directly, without mirrors"
        sources=github
      fi
    fi
  fi
  case "$tag" in ""|*[!A-Za-z0-9._+-]*) die "invalid release version" ;; esac
  version="${tag#v}"
  asset=$(asset_name "$version")
  if [ "$os/$variant" = linux/server ] && have xz; then
    xz_asset="${asset%.gz}.xz"
    if { [ -n "$release_info" ] && awk -v name="$xz_asset" '$3 == name { found=1 } END { exit !found }' "$release_info"; } \
        || { [ -n "$release_json" ] && printf '%s' "$release_json" | grep -qF "\"name\": \"$xz_asset\""; }; then
      asset="$xz_asset"
    fi
  fi
  url="https://github.com/$repo/releases/download/$tag/$asset"
  if [ -n "$release_info" ]; then
    row=$(awk -v name="$asset" '$3 == name { print $1 " " $2 }' "$release_info")
    [ -n "$row" ] || die "release $version exists, but has no package $asset for this host"
    digest=${row%% *}; size=${row#* }
    [ "$digest" != - ] || digest=""
  elif [ -n "$release_json" ]; then
    # GitHub stores a sha256 for every asset as it is uploaded and reports it
    # on the API as "sha256:<hex>". Read it from the same authenticated API response.
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
    [ "${#digest}" -eq 64 ] || digest=""
    # Its size, from the same object, for the download bar.
    size=$(printf '%s' "$release_json" \
      | awk -v want="\"name\": \"$asset\"" '
          index($0, want)      { inside = 1; next }
          inside && /"name":/  { exit }
          inside && /"size": *[0-9]/ { sub(/.*"size": */, ""); sub(/[^0-9].*/, ""); print; exit }')
    # The asset's own API address, for a private repository: the download URL
    # under github.com/.../releases/download answers only for public ones,
    # while the API serves the bytes to a token with Accept: octet-stream. In
    # the asset object "url" comes before "name", so it is the last "url"
    # seen when the name line matches.
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      asset_api_url=$(printf '%s' "$release_json" \
        | awk -v want="\"name\": \"$asset\"" '
            /^ *"url": *"https:\/\/api\.github\.com\/repos\/[^"]*\/releases\/assets\/[0-9]+"/ {
              u = $0; sub(/.*"url": *"/, "", u); sub(/".*/, "", u)
            }
            index($0, want) { print u; exit }')
    fi
    if [ -z "$digest" ]; then
      printf '%s' "$release_json" | grep -qF "\"name\": \"$asset\"" \
        || die "release $version has no $asset; see https://github.com/$repo/releases/tag/$tag"
    fi
  fi
else
  # No release lookup, so the version comes from --version, or else from the
  # name the release gave the archive. The manifest needs one: without it the
  # next update no longer recognises this install as the script's own.
  if [ -n "$tag" ]; then
    version="${tag#v}"
  else
    version=$(basename "$from" | sed -n \
      -e 's/^ThinkTerm-macos-[^-]*-\([0-9A-Za-z][0-9A-Za-z.+-]*\)\.zip$/\1/p' \
      -e 's/^thinkterm-\(server-\)\{0,1\}\([0-9A-Za-z][0-9A-Za-z.+-]*\)-linux-[^-]*\(-glibc[0-9.]*\)\{0,1\}\.tar\.\(gz\|xz\)$/\2/p')
  fi
fi

if [ -n "$size" ]; then
  case "$size" in *[!0-9]*) die "invalid release archive size" ;; esac
  [ "$size" -gt 0 ] && [ "$size" -le 8589934592 ] || die "invalid release archive size"
fi

if [ -n "$from" ]; then
  ok "ThinkTerm${version:+ $ui_bold$version$ui_reset} $ui_dim(from $(pretty "$from"))$ui_reset"
elif [ -n "$tag_requested" ]; then
  ok "ThinkTerm $ui_bold$version$ui_reset"
else
  ok "ThinkTerm $ui_bold$version$ui_reset ${ui_dim}(latest)$ui_reset"
fi
if [ -n "$prev_variant" ]; then
  case "$prev_variant/$variant" in
    server/desktop)  note "adds the GUI to server${prev_version:+ $prev_version}; everything else is kept" ;;
    desktop/server)  note "replaces desktop${prev_version:+ $prev_version}; the GUI and its launcher will be removed" ;;
    *)
      if [ "$prev_version" = "$version" ]; then note "already installed; reinstalling it"
      else note "replaces $prev_variant${prev_version:+ $prev_version}"; fi
      ;;
  esac
fi
if [ -z "$from" ] && [ -z "$digest" ]; then
  warn "no trusted checksum is available for $asset; the GitHub download will not be verified"
fi

if [ -n "$dry_run" ]; then
  if [ -n "$from" ]; then note "from  $(pretty "$from")"; else note "from  $url"; fi
  case "$mirror" in
    direct) ;;
    auto) note "via   $mirrors, if GitHub is slow" ;;
    *)    note "via   $mirror, then GitHub" ;;
  esac
  [ -z "$app_dir" ] || note "app   $(pretty "$app")"
  note "bin   $(pretty "$bindir")"
  say
  ok "Dry run: nothing was changed."
  exit 0
fi

# ---- download and verify ---------------------------------------------------

mb() { t=$(($1 * 10 / 1048576)); printf '%d.%d' $((t / 10)) $((t % 10)); }

# $1 of $2 bytes as a bar $3 cells wide. A line of light where 24-bit colour
# and UTF-8 are both there; coloured spaces where only 256 colours are, which
# every terminal draws exactly one cell wide; plain characters without colour.
progress_bar() {
  filled=$(($1 * $3 / $2)); [ "$filled" -le "$3" ] || filled=$3
  out=""; k=0
  while [ "$k" -lt "$3" ]; do
    if [ -z "$ui_color" ]; then
      if [ "$k" -lt "$filled" ]; then out="$out#"; else out="$out-"; fi
    elif [ "$palette" = truecolor ] && [ -n "$unicode" ] && [ "${TERM_PROGRAM:-}" != Apple_Terminal ]; then
      if [ "$k" -lt "$filled" ]; then
        out="$out$esc[38;2;$((200 - 116 * k / $3));$((206 - 106 * k / $3));$((255 - 5 * k / $3))m━"
      else
        out="$out$esc[38;2;70;70;80m━"
      fi
    elif [ "$palette" = basic ]; then
      if [ "$k" -lt "$filled" ]; then out="$out$esc[44m "; else out="$out$esc[0m$esc[2m-"; fi
    else
      if [ "$k" -ge "$filled" ]; then out="$out$esc[48;5;237m "
      elif [ $((k * 2)) -lt "$3" ]; then out="$out$esc[48;5;147m "
      else out="$out$esc[48;5;105m "; fi
    fi
    k=$((k + 1))
  done
  printf '%s%s' "$out" "$ui_reset"
}

# Runs curl with the remaining arguments in the background while drawing how
# much of $1 has arrived: a line with the amount and rate, and the bar under
# it. $2 names the mirror, if it is one; $3 is "token" to send GITHUB_TOKEN.
# Curl enforces the same transfer deadlines in interactive and SSH modes.
# A non-terminal gets periodic plain lines rather than redraw control codes.
download() {
  dest=$1; via=$2; auth=$3; shift 3
  initial_bytes=0
  [ ! -f "$dest" ] || initial_bytes=$(wc -c < "$dest")
  if [ -n "$size" ]; then
    set -- --max-filesize "$size" "$@"
  fi
  if [ "$auth" = token ] && [ -n "${GITHUB_TOKEN:-}" ]; then
    set -- -H "Authorization: Bearer $GITHUB_TOKEN" "$@"
  fi
  if [ -z "$anim" ]; then
    printf '  %s>%s downloading%s...\n' "$ui_accent" "$ui_reset" "${via:+ from $via}" >&4
    ( [ -z "$size" ] || ulimit -f "$(((size + 511) / 512))" || exit 1
      exec curl --disable -fsSL --proto =https --proto-redir =https "$@" ) &
    bg_pid=$!
    plain_started=$(date +%s)
    plain_initial=$initial_bytes
    while kill -0 "$bg_pid" 2>/dev/null; do
      sleep 2
      kill -0 "$bg_pid" 2>/dev/null || break
      plain_got=0
      [ ! -f "$dest" ] || plain_got=$(wc -c < "$dest")
      plain_elapsed=$(($(date +%s) - plain_started))
      [ "$plain_elapsed" -gt 0 ] || plain_elapsed=1
      plain_rate=$(((plain_got - plain_initial) / plain_elapsed))
      [ "$plain_rate" -ge 0 ] || plain_rate=0
      plain_percent=""
      [ -z "$size" ] || plain_percent=" $((plain_got * 100 / size))% of $(mb "$size") MB"
      printf '    %s MB%s, %s KB/s%s\n' "$(mb "$plain_got")" "$plain_percent" "$((plain_rate / 1024))" "${via:+ from $via}" >&4
    done
    if wait "$bg_pid"; then status=0; else status=$?; fi
    bg_pid=""
    return "$status"
  fi
  ( [ -z "$size" ] || ulimit -f "$(((size + 511) / 512))" || exit 1
    exec curl --disable -fsSL --proto =https --proto-redir =https "$@" ) 2>"$errlog" &
  bg_pid=$!
  hide_cursor
  started=$(date +%s)
  width=$((cols - 6)); [ "$width" -le 44 ] || width=44; [ "$width" -ge 10 ] || width=10
  i=0
  printf '\n' >&4
  while kill -0 "$bg_pid" 2>/dev/null; do
    got=0
    # shellcheck disable=SC2046
    if [ -f "$dest" ]; then set -- $(wc -c < "$dest"); got=${1:-0}; fi
    now=$(date +%s)
    # The parts of the status line, dropped from the least useful while the
    # line would not fit: a wrapped line breaks the redraw a line above.
    amount=" / $(mb "${size:-0}") MB"; [ -n "$size" ] || amount=" MB"
    from_part=${via:+ from $via}
    rate_part=""
    elapsed=$((now - started))
    if [ "$elapsed" -gt 0 ]; then
      rate=$(((got - initial_bytes) / elapsed))
      [ "$rate" -ge 0 ] || rate=0
      if [ "$rate" -ge 1048576 ]; then rate_part=" $dot $(mb "$rate") MB/s"
      else rate_part=" $dot $((rate / 1024)) KB/s"; fi
    fi
    got_mb=$(mb "$got")
    for drop in none rate from amount; do
      case $drop in
        rate) rate_part="" ;;
        from) from_part="" ;;
        amount) amount=" MB" ;;
      esac
      plain="  x downloading$from_part  $got_mb$amount$rate_part"
      [ "${#plain}" -ge "$cols" ] || break
    done
    line="downloading$from_part  $ui_bold$got_mb$ui_reset$ui_dim$amount$rate_part"
    printf '%s[1A\r%s[K  %s%s%s %s%s\n\r%s[K' "$esc" "$esc" \
      "$ui_accent" "$(spinner_frame "$i")" "$ui_reset" "$line" "$ui_reset" "$esc" >&4
    [ -z "$size" ] || printf '    %s' "$(progress_bar "$got" "$size" "$width")" >&4
    i=$((i + 1))
    sleep 0.1
  done
  printf '\r%s[K%s[1A\r%s[K' "$esc" "$esc" "$esc" >&4
  show_cursor
  if wait "$bg_pid"; then status=0; else status=$?; fi
  bg_pid=""
  [ "$status" -eq 0 ] || show_error
  return "$status"
}

# Probes never feed the archive: a fast error page is not a verified package.
# Each child owns its curl process so cancellation also stops parallel probes.
probe_source() (
  probe_source_name=$1
  probe_url="$url"
  [ "$probe_source_name" = github ] || probe_url="https://$probe_source_name/$url"
  probe_pid=""
  trap '[ -z "$probe_pid" ] || { kill "$probe_pid" 2>/dev/null || true; wait "$probe_pid" 2>/dev/null || true; }' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  ulimit -f 2048 || exit 1
  curl --disable -fsSL --proto =https --proto-redir =https --connect-timeout 2 --max-time 3 \
    --range 0-1048575 --max-filesize 1048576 --retry 0 \
    -o "$tmp/probe-$probe_source_name" -w '%{http_code} %{size_download} %{time_total}\n' \
    "$probe_url" > "$tmp/probe-$probe_source_name.stats" 2>/dev/null &
  probe_pid=$!
  wait "$probe_pid" || true
  probe_pid=""
  awk -v source="$probe_source_name" '
    ($1 == 200 || $1 == 206) && $2 > 0 && $3 > 0 { rate=int($2/$3) }
    END { print rate+0, source }
  ' "$tmp/probe-$probe_source_name.stats" > "$tmp/rate-$probe_source_name"
  rm -f "$tmp/probe-$probe_source_name"
)

rank_sources() {
  github_fast=""
  if [ "$mirror" != auto ]; then
    printf '%s\n' $sources > "$tmp/ranked-sources"
    return
  fi
  if [ "${1:-}" != mirrors ]; then
    note "checking GitHub download speed"
    probe_source github
    github_rate=$(awk '{print $1}' "$tmp/rate-github")
    if [ "$github_rate" -ge "$min_rate" ]; then
      github_fast=1
      printf 'github\n' > "$tmp/ranked-sources"
      return
    fi
  fi
  note "GitHub is slow or unavailable; checking mirrors in parallel"
  probe_pids=""
  for probe_host in $mirrors; do
    probe_source "$probe_host" &
    probe_pids="$probe_pids $!"
  done
  for pid in $probe_pids; do wait "$pid" || true; done
  probe_pids=""
  # A source that failed the probe is still a last resort, after the measured
  # ones. A short probe can fail during DNS/TLS while a full transfer works.
  cat "$tmp"/rate-* | sort -nr | awk '{print $2}' > "$tmp/ranked-sources"
}

# The archive is all there and is the one GitHub recorded.
archive_complete() {
  [ -s "$archive" ] \
    && { [ -z "$size" ] || [ "$(wc -c < "$archive")" -eq "$size" ]; } \
    && [ "$(sha256 "$archive")" = "$digest" ]
}

verified_download() {
  source=$1
  transfer_mode=${2:-}
  source_url="$url"; source_via=""; source_auth=token
  if [ "$source" != github ]; then
    source_url="https://$source/$url"; source_via=$source; source_auth=""
  fi
  rm -f "$tmp/slow-$source"
  # What a source before this one brought is kept: they all serve the same
  # bytes, so this one goes on from there, and the whole is checked at the
  # end. One that will not go on from there starts it again.
  carried=0
  if [ -s "$archive" ]; then
    archive_complete && return 0
    carried=$(wc -c < "$archive")
    note "going on from $(mb "$carried") MB with $source"
  fi
  attempt=0
  while [ "$attempt" -lt 3 ]; do
    attempt=$((attempt + 1))
    had=0
    [ ! -f "$archive" ] || had=$(wc -c < "$archive")
    # If every measured link is slow, keep the fastest rather than refuse all
    # of them. If it stalls or breaks, the next source still gets a chance.
    transfer_rate=1; transfer_window=15
    if [ "$transfer_mode" != slow ]; then
      if [ "$mirror" != auto ] && [ "$source" != github ]; then
        transfer_rate=$min_rate; transfer_window=5
      fi
      if [ -f "$tmp/rate-$source" ] && [ "$(awk '{print $1}' "$tmp/rate-$source")" -ge "$min_rate" ]; then
        transfer_rate=$min_rate; transfer_window=5
      fi
    fi
    if download "$archive" "$source_via" "$source_auth" --connect-timeout 4 \
        --speed-limit "$transfer_rate" --speed-time "$transfer_window" --retry 0 \
        -C - -o "$archive" "$source_url"; then
      if archive_complete; then
        return 0
      fi
      # Bytes from another source may be what is wrong: this one gets to
      # bring the whole before it is the one blamed.
      if [ "$carried" -gt 0 ]; then
        warn "checksum or size mismatch going on with $source; starting again"
        rm -f "$archive"
        carried=0
        attempt=0
        continue
      fi
      warn "checksum or size mismatch from $source; discarded"
      rm -f "$archive"
      return 1
    else
      transfer_status=$?
      # Try other sources before allowing a slow transfer. Only one that
      # delivered bytes gets that fallback; a connection timeout does not.
      # Broken transfers resume; range refusal gets a fresh retry.
      case "$transfer_status" in
        22|63) break ;;
        28)
          if [ "$transfer_rate" -gt 1 ]; then
            if [ "$mirror" = auto ] && [ -f "$archive" ] \
                && [ "$(wc -c < "$archive")" -gt "$had" ]; then
              : > "$tmp/slow-$source"
            fi
            break
          fi
          ;;
        33|36) rm -f "$archive"; carried=0 ;;
      esac
      [ "$attempt" -ge 3 ] || note "retrying $source (resuming when supported)"
    fi
  done
  warn "$source download failed or was too slow"
  return 1
}

if [ -n "$from" ]; then
  [ -f "$from" ] || die "no such file: $from"
  archive="$from"
  if [ -n "$expected_sha256" ]; then
    got=$(sha256 "$archive") || die "SHA-256 verification requires sha256sum or shasum"
    [ "$got" = "$expected_sha256" ] || die "checksum mismatch for --from archive; nothing was installed"
    ok "verified SHA-256"
  fi
else
  archive="$tmp/$asset"
  verified=""
  came_from=""
  dl_started=$(date +%s)
  if [ -n "$asset_api_url" ]; then
    download "$archive" "" token --connect-timeout 4 --speed-limit 1 --speed-time 30 --retry 3 -H "Accept: application/octet-stream" -o "$archive" "$asset_api_url" \
      || die "download failed: $asset_api_url"
  elif [ "$sources" != github ] && [ -n "$digest" ] && { have sha256sum || have shasum; }; then
    rank_sources
    if [ -n "$github_fast" ]; then
      if verified_download github; then
        verified=1
      else
        printf '0 github\n' > "$tmp/rate-github"
        rank_sources mirrors
      fi
    fi
    if [ -z "$verified" ]; then
      for source in $(cat "$tmp/ranked-sources"); do
        if verified_download "$source"; then
          verified=1
          [ "$source" = github ] || came_from=$source
          break
        fi
      done
    fi
    if [ -z "$verified" ] && [ "$mirror" = auto ]; then
      # A quick probe can overestimate sustained speed. Revisit progressing
      # sources in rank order with stall protection, not the fast cutoff.
      for source in $(cat "$tmp/ranked-sources"); do
        [ -f "$tmp/slow-$source" ] || continue
        note "retrying $source, allowing a slower download"
        if verified_download "$source" slow; then
          verified=1
          [ "$source" = github ] || came_from=$source
          break
        fi
      done
    fi
    [ -n "$verified" ] || die "all download sources failed, were too slow, or failed verification for $asset; retry or use --from with --sha256"
  else
    if [ "$sources" != github ]; then
      if [ -n "$digest" ]; then
        warn "neither sha256sum nor shasum is available to check a mirror's download; downloading from GitHub"
      else
        warn "GitHub recorded no checksum to check a mirror's download against; downloading from GitHub"
      fi
    fi
    if [ -n "$digest" ] && { have sha256sum || have shasum; }; then
      verified_download github || die "GitHub download failed or did not pass verification"
      verified=1
    else
      download "$archive" "" token --connect-timeout 4 --speed-limit 1 --speed-time 30 --retry 3 \
        -o "$archive" "$url" || die "GitHub archive download failed (network error or package unavailable): $url"
    fi
  fi
  # shellcheck disable=SC2046
  set -- $(wc -c < "$archive")
  ok "downloaded $(mb "${1:-0}") MB${came_from:+ from $came_from} ${ui_dim}in $(($(date +%s) - dl_started))s$ui_reset"
  if [ -n "$digest" ] && [ -z "$verified" ]; then
    if got=$(sha256 "$archive"); then
      [ "$got" = "$digest" ] || die "checksum mismatch for $asset
  expected $digest
  got      $got
The download is corrupt or has been tampered with; nothing was installed."
      verified=1
    else
      warn "neither sha256sum nor shasum is available; the download was not verified"
    fi
  fi
  [ -z "$verified" ] || ok "verified SHA-256"
fi

mkdir "$tmp/x"
case "$os" in
  linux)
    case "$archive" in
      *.tar.xz) have xz || die "xz is required to unpack this archive"; tar_flag=xJf ;;
      *) tar_flag=xzf ;;
    esac
    with_spinner "unpacking" tar "$tar_flag" "$archive" -C "$tmp/x" || die "could not unpack $archive"
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
    # thinkterm[-server]-<ver>-linux-<arch>[-glibc<floor>].
    if [ -z "$version" ]; then
      version=$(basename "$root" | sed -n 's/^thinkterm-\(server-\)\{0,1\}\([0-9A-Za-z][0-9A-Za-z.+-]*\)-linux-[^-]*\(-glibc[0-9.]*\)\{0,1\}$/\2/p')
    fi
    ;;
  macos)
    # ditto, not unzip: it is what packed the archive, and it restores the
    # resource forks and extended attributes the code signature covers.
    with_spinner "unpacking" ditto -x -k "$archive" "$tmp/x" || die "could not unpack $archive"
    root=$(find "$tmp/x" -maxdepth 2 -name ThinkTerm.app -type d | head -n1)
    [ -n "$root" ] && [ -x "$root/Contents/MacOS/thinkterm" ] || die "unexpected zip layout: no ThinkTerm.app inside"
    if [ -z "$version" ]; then
      version=$(basename "$(dirname "$root")" | sed -n 's/^ThinkTerm-macos-[^-]*-\([0-9A-Za-z][0-9A-Za-z.+-]*\)$/\1/p')
    fi
    ;;
esac
# The manifest names the install by its version, and the next update
# rejects one without: a version this script could not tell goes in as
# unknown.
if [ -z "$version" ]; then
  warn "could not tell which version $archive is; recording it as unknown (pass --version to name it)"
  version=unknown
fi

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

commands=0
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

if [ "$os" = macos ]; then
  # The bundle is moved whole so the signature and notarization ticket stay
  # intact. The old bundle is renamed aside, never deleted first: if it
  # cannot be moved (a locked file, someone else's ownership) the script
  # stops with both the old install and the new download intact, and a
  # running ThinkTerm keeps its files under the renamed path until it quits.
  # The command-line tools are symlinks into the bundle: a copy would run
  # outside the seal, and the wezterm shim locates thinkterm next to itself.
  mkdir -p "$app_dir"
  app_staging=1
  rm -rf "$app_dir/.ThinkTerm.app.tmp" "$app_dir/.ThinkTerm.app.old"
  mv "$root" "$app_dir/.ThinkTerm.app.tmp"
  if [ -e "$app" ]; then
    mv "$app" "$app_dir/.ThinkTerm.app.old" || die "could not move the existing $app aside; nothing was changed"
  fi
  mv "$app_dir/.ThinkTerm.app.tmp" "$app"
  # The new bundle is in place at this point, so a leftover that will not
  # delete (a locked file inside the old one) is worth a note, not a failure.
  if ! rm -rf "$app_dir/.ThinkTerm.app.old" 2>/dev/null; then
    warn "could not remove the old bundle left at $app_dir/.ThinkTerm.app.old; delete it by hand"
  fi
  # The previous run put the bundle somewhere else (the other variant's
  # location, or another --app-dir): one ThinkTerm.app per machine.
  if [ -n "$prev_app" ] && [ "$prev_app" != "$app" ] && [ -d "$prev_app" ]; then
    note "removing the previous bundle at $(pretty "$prev_app")"
    rm -rf "$prev_app"
  fi
  for name in thinkterm wezterm thinkterm-mux-server strip-ansi-escapes; do
    [ -x "$app/Contents/MacOS/$name" ] || continue
    ln -sfn "$app/Contents/MacOS/$name" "$bindir/$name"
    commands=$((commands + 1))
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
  bin_stage=$(mktemp -d "$bindir/.thinkterm-install.XXXXXX")
  for f in "$src_bin"/*; do
    name=$(basename "$f")
    cp "$f" "$bin_stage/$name"
    chmod 755 "$bin_stage/$name"
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
        note "removing $(pretty "$other") (desktop variant, replaced by --server)"
        rm -f "$other"
      fi
    done
  fi

  for f in "$src_bin"/*; do
    name=$(basename "$f")
    mv -f "$bin_stage/$name" "$bindir/$name"
    commands=$((commands + 1))
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
# The browser client, if the tarball carries one; the mux server serves it
# from here when a web_servers entry is configured.
# A tarball without one leaves an installed bundle alone.
if [ -d "$root/share/thinkterm/web" ]; then
  rm -rf "$sharedir/thinkterm/web"
  # -p: the server compares a .gz against its file by modification
  # time, so an install that rewrites them can mislead it.
  cp -Rp "$root/share/thinkterm/web" "$sharedir/thinkterm/web"
fi
if [ "$os" = linux ] && [ "$variant" = desktop ] && [ -d "$root/share/thinkterm/connect-auth" ]; then
  # Keep the supplied policy templates and instructions for administrator
  # setup; a user-prefix install must not write the system PAM policy.
  rm -rf "$sharedir/thinkterm/connect-auth"
  cp -Rp "$root/share/thinkterm/connect-auth" "$sharedir/thinkterm/connect-auth"
fi

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

if [ "$os" = macos ]; then
  ok "installed ${ui_dim}ThinkTerm.app $arrow $(pretty "$app_dir")$ui_reset"
  note "$commands commands $arrow $(pretty "$bindir")"
else
  ok "installed ${ui_dim}$commands commands $arrow $(pretty "$bindir")$ui_reset"
  [ "$variant" != desktop ] || note "app launcher $arrow $(pretty "$sharedir/applications")"
fi

# The binaries were swapped under it, so a mux server that is still running
# is the old build. Client and server refuse to talk across a protocol
# change, so the next `thinkterm connect` would fail with a version error
# unless it is replaced.
# Matched on the command line: Linux truncates process names to 15
# characters, so -x with the full binary name never matches there.
server_running=""
if have pgrep && pgrep -u "$(id -u)" -f '[t]hinkterm-mux-server' >/dev/null 2>&1; then
  server_running=1
fi

# On a server, the new build takes the running one's sessions over
# (--takeover) and carries on in its place, so nothing running in it ends.
# A server that cannot hand over -- a build from before the handoff -- says
# so and keeps running as it was. Only for someone at a terminal: `thinkterm
# update` on another machine runs this script over ssh without one and takes
# the server over itself, as its settings say. The desktop app moves its
# own session server over the next time it starts; swapping it under the
# app still open would cut that off.
take_over_server() {
  if have timeout; then
    timeout 120 "$bindir/thinkterm-mux-server" --daemonize --takeover
  else
    "$bindir/thinkterm-mux-server" --daemonize --takeover
  fi
}
takeover=""
if [ -n "$server_running" ] && [ "$variant" = server ] && [ -t 1 ]; then
  new_build=$version
  [ "$version" != unknown ] || new_build="the new version"
  attempt=1
  while :; do
    if with_spinner "moving the mux server's sessions to $new_build" take_over_server; then
      takeover=done
      break
    else
      code=$?
    fi
    # Tried again as the app's own takeover is: a failure part-way leaves
    # the running server as it was and need not recur. Not a server that
    # cannot hand over at all (3), nor one that timed out (124).
    if [ "$code" -eq 3 ] || [ "$code" -eq 124 ] || [ "$attempt" -ge 3 ]; then
      takeover=failed
      break
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  if [ "$takeover" = done ]; then
    ok "moved the mux server to $new_build ${ui_dim}its sessions carry on$ui_reset"
  else
    warn "the mux server could not hand its sessions over; it keeps running the old version"
  fi
fi

# What is left for the user, most pressing first: "!" for what stops
# ThinkTerm working until it is done, "." for what is worth knowing.
next_shown=""
next() {
  if [ -z "$next_shown" ]; then
    printf '\n  %sNext%s\n' "$ui_bold" "$ui_reset"
    next_shown=1
  fi
  case "$1" in
    !) printf '  %s!%s %s\n' "$ui_warning" "$ui_reset" "$2" ;;
    *) printf '  %s%s%s %s\n' "$ui_dim" "$dot" "$ui_reset" "$2" ;;
  esac
}
# A command to copy, under the item that asks for it.
cmd() { printf '      %s%s%s\n' "$ui_accent" "$1" "$ui_reset"; }
# The same path, spelled so it still works pasted into a startup file.
shell_path() {
  case "$1" in
    "$HOME"/*) printf '$HOME%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# Nothing is done to the shell startup files: editing them is the user's
# call. But ~/.local/bin on Debian and Ubuntu only enters PATH at the next
# login, and only if it existed then -- so say so.
case ":$PATH:" in
  *":$bindir:"*) ;;
  *)
    next ! "$(pretty "$bindir") is not on your PATH; add this to your shell startup file:"
    cmd "export PATH=\"$(shell_path "$bindir"):\$PATH\""
    ;;
esac

if system=$(command -v thinkterm 2>/dev/null) && [ "$system" != "$bindir/thinkterm" ]; then
  next ! "another thinkterm comes first on PATH: $(pretty "$system")"
fi

if [ -n "$other_gui" ] && [ "$variant" = server ]; then
  next ! "this install has no GUI, and the desktop app at $(pretty "$other_gui") stays at its own version."
  note "Its launcher may find this install first and stop working."
  # Only where it would work: the launcher looks in ~/.local/share, not under
  # another prefix, and the GUI needs a newer glibc than the server.
  if [ "$prefix" = "$HOME/.local" ] && { [ -z "$glibc" ] || [ "$(printf '%s\n%s\n' "$min_glibc_desktop" "$glibc" \
      | sort -V 2>/dev/null | head -n1)" = "$min_glibc_desktop" ]; }; then
    note "To keep them together, run:"
    cmd "curl -fsSL https://raw.githubusercontent.com/$repo/main/install.sh | sh -s -- --desktop"
  fi
fi

if [ -n "$missing_libs" ]; then
  next ! "the GUI needs these libraries; the CLI and mux server run without them:"
  printf '      %s%-30s %s%s\n' "$ui_dim" "library" "package (apt / dnf / zypper)" "$ui_reset"
  for lib in $missing_libs; do
    printf '      %-30s %s\n' "$lib" "$(lib_package "$lib")"
  done
fi

# The server variant's is dealt with below.
if [ -n "$server_running" ] && [ "$variant" != server ]; then
  next . "the mux server still running is the old version; restart it to update it."
fi
gui_running=""
if have pgrep && pgrep -u "$(id -u)" -f '[t]hinkterm-gui' >/dev/null 2>&1; then
  gui_running=1
  next . "ThinkTerm is open; the new version starts next time."
fi

if [ "$variant" = server ]; then
  if [ "$takeover" = done ]; then
    :
  elif [ "$takeover" = failed ]; then
    next ! "the mux server still runs the old version; to switch, stop it and start this one (its sessions end):"
    cmd "thinkterm cli stop-server"
    cmd "thinkterm-mux-server --daemonize"
  elif [ -n "$server_running" ]; then
    # Not tried without a terminal. Starting another one beside it would
    # change nothing: --takeover hands its sessions to the new build.
    next . "the mux server still running is the old version; move its sessions to this one:"
    cmd "thinkterm-mux-server --daemonize --takeover"
  else
    next . "start the mux server (no root, no service file):"
    cmd "thinkterm-mux-server --daemonize"
    if [ "$os" = linux ]; then
      next . "it usually keeps running after you log out of SSH; if it does not:"
      cmd "loginctl enable-linger \$USER   # some servers need an admin for this"
    fi
  fi
fi

if [ -f "$sharedir/thinkterm/shell-integration/wezterm.sh" ]; then
  next . "optional: shell integration (prompt marks, cwd), in your startup file:"
  cmd ". \"$(shell_path "$sharedir/thinkterm/shell-integration/wezterm.sh")\""
fi

say
if [ -n "$missing_libs" ]; then
  ok "${ui_bold}Installed.$ui_reset The desktop app needs the libraries listed above."
  exit 0
fi
if [ "$variant" = server ]; then
  ok "${ui_bold}Ready.$ui_reset Run ${ui_accent}thinkterm tui$ui_reset to get started."
  exit 0
fi

# Offer to open the app when there is someone to ask and a screen to open it
# on -- not over SSH, and not while the old version is still running.
can_open=""
if [ -z "$gui_running" ] && [ -z "${SSH_CONNECTION:-}${SSH_TTY:-}" ] \
    && [ -t 1 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
  case "$os" in
    macos) can_open=1 ;;
    *) if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then can_open=1; fi ;;
  esac
fi
if [ -z "$can_open" ]; then
  ok "${ui_bold}Ready.$ui_reset Run ${ui_accent}thinkterm$ui_reset to get started."
  exit 0
fi
printf '  %s%s%s %sReady.%s Open ThinkTerm now? %s[Y/n]%s ' \
  "$ui_success" "$mark_ok" "$ui_reset" "$ui_bold" "$ui_reset" "$ui_dim" "$ui_reset" > /dev/tty
read -r answer < /dev/tty || answer=n
case "$answer" in
  n | N | no | NO | No) ;;
  *)
    if [ "$os" = macos ]; then
      open "$app" || warn "could not open $(pretty "$app")"
    else
      nohup "$bindir/thinkterm" >/dev/null 2>&1 &
    fi
    ;;
esac
