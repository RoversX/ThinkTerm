#!/bin/bash
# Fail when a Linux release binary needs a newer glibc than install.sh
# promises for its variant -- and thinkterm-update, which a test keeps in step
# with it. A binary loads on a glibc that has every GLIBC_x.y symbol version
# it references, so the newest of those is the oldest glibc it runs on.
# Without this check, one call into a newer libc function (the GUI's hypot is
# why the desktop needs 2.35) would leave hosts that install.sh lets through
# with binaries that fail to load.
#
#   ci/check-glibc.sh target/release              the Ubuntu 22.04 leg
#   ci/check-glibc.sh target/release old-server   the Ubuntu 20.04 leg

set -eu

dir=${1:?usage: ci/check-glibc.sh <directory of the built binaries> [old-server]}
mode=${2:-}
case "$mode" in
  "" | old-server) ;;
  *) echo "unknown mode '$mode'; the only one is old-server" >&2; exit 2 ;;
esac
script="$(dirname "$0")/../install.sh"

floor() {
  sed -n "s/^$1=\"\([0-9.]*\)\".*/\1/p" "$script"
}
server=$(floor min_glibc_server)
server_main=$(floor min_glibc_server_main)
desktop=$(floor min_glibc_desktop)
if [ -z "$server" ] || [ -z "$server_main" ] || [ -z "$desktop" ]; then
  echo "install.sh sets no min_glibc_server, min_glibc_server_main or min_glibc_desktop" >&2
  exit 1
fi

status=0

check() {
  local bin=$1 floor=$2 need
  need=$(objdump -T "$dir/$bin" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' \
    | sed 's/^GLIBC_//' | sort -V | tail -n1)
  if [ -z "$need" ]; then
    echo "$bin: no glibc symbol versions found" >&2
    status=1
  elif [ "$(printf '%s\n%s\n' "$need" "$floor" | sort -V | tail -n1)" != "$floor" ]; then
    echo "$bin needs glibc $need, but install.sh lets hosts with $floor install it" >&2
    status=1
  else
    echo "$bin: glibc $need (install.sh: $floor)"
  fi
}

# The server tarball's binaries; the desktop's are the same and the GUI.
servers="thinkterm wezterm thinkterm-mux-server thinkterm-plugin-server thinkterm-tailnet strip-ansi-escapes"

if [ "$mode" = old-server ]; then
  # The server for hosts below min_glibc_server_main. Their own OpenSSL is
  # 1.1, which upstream no longer fixes, so this build has to carry its own.
  for bin in $servers; do
    check "$bin" "$server"
    if objdump -p "$dir/$bin" | grep -qE 'NEEDED +lib(ssl|crypto)\.so'; then
      echo "$bin links the host's OpenSSL; the old-glibc server has to carry its own" >&2
      status=1
    fi
  done
else
  for bin in $servers; do
    check "$bin" "$server_main"
  done
  check thinkterm-gui "$desktop"
  check thinkterm-connect-auth "$desktop"
fi

exit $status
