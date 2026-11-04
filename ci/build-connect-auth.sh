#!/bin/bash
set -euo pipefail

# The macOS helper is Swift. Linux and Windows have theirs in the workspace.
platform=${1:?usage: build-connect-auth.sh macos arm64|x86_64 output-directory}
arch=${2:?missing architecture}
output=${3:?missing output directory}
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$output"
output=$(cd "$output" && pwd)

case "$arch" in
  arm64 | x86_64) ;;
  *) echo "Unsupported architecture" >&2; exit 2 ;;
esac

case "$platform" in
  macos)
    xcrun swiftc -swift-version 5 -O -target "$arch-apple-macosx13.0" \
      "$root/thinkterm-reverse-access/native/macos.swift" \
      -o "$output/thinkterm-connect-auth"
    ;;
  *) echo "Unsupported platform" >&2; exit 2 ;;
esac
