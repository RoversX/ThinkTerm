#!/usr/bin/env bash
# Merge a new tailscale-rs release into the vendored crates beside this script, three ways,
# file by file: the base is upstream's OLD version, ours is the copy here, theirs is upstream's
# NEW version. See PATCHES.txt, "Upgrading", for the steps around it.
#
#     deps/tailscale-rs/merge-upstream.sh OLD NEW      e.g. 0.7.1 0.8.0
#
# For each crate here (a folder with a Cargo.toml), in src/ and for Cargo.toml:
#   - a file we did not change takes upstream's new version;
#   - a file we changed is merged with `git merge-file`; overlapping changes are left between
#     conflict markers;
#   - a file upstream added is copied in, and one it removed is removed if we did not change it.
# What is left for a person is listed at the end, and the exit status is 1 if there is any.
# Nothing outside the crates' folders is touched: the root Cargo.toml and Cargo.lock are
# step 3 of "Upgrading".
set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 OLD NEW" >&2
    exit 2
fi
old=$1
new=$2
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { # crate version -> unpacked into $work/version/crate-version
    mkdir -p "$work/$2"
    curl -sSfL -A "tailscale-rs-vendor-merge" \
        "https://static.crates.io/crates/$1/$1-$2.crate" | tar xz -C "$work/$2"
}

# With a wrong OLD, upstream's own changes would pass for ours. The crate's own version is the
# one in [package]; the dependencies' tables have version lines too.
for manifest in "$here"/*/Cargo.toml; do
    have=$(awk '/^\[/ { in_package = ($0 == "[package]") } in_package && /^version = / { print; exit }' "$manifest")
    if [ "$have" != "version = \"$old\"" ]; then
        echo "$manifest is not at $old: ${have:-no version in [package]}" >&2
        exit 2
    fi
done

todo=()
for manifest in "$here"/*/Cargo.toml; do
    dir=$(dirname "$manifest")
    crate=$(basename "$dir")
    if ! fetch "$crate" "$old" 2>/dev/null; then
        todo+=("$crate: no $old on crates.io to merge from")
        continue
    fi
    if ! fetch "$crate" "$new" 2>/dev/null; then
        todo+=("$crate: no $new on crates.io; upstream may have merged it into another crate or renamed it (move our changes by hand)")
        continue
    fi
    base=$work/$old/$crate-$old
    theirs=$work/$new/$crate-$new

    while IFS= read -r rel; do
        o=$dir/$rel b=$base/$rel t=$theirs/$rel
        if [ ! -e "$b" ]; then
            # Added upstream.
            if [ ! -e "$o" ]; then
                mkdir -p "$(dirname "$o")"
                cp "$t" "$o"
                echo "added    $crate/$rel"
            elif ! cmp -s "$o" "$t"; then
                todo+=("$crate/$rel: added by upstream and by us")
            fi
        elif [ ! -e "$t" ]; then
            # Removed upstream.
            if [ -e "$o" ] && cmp -s "$o" "$b"; then
                rm "$o"
                echo "removed  $crate/$rel"
            elif [ -e "$o" ]; then
                todo+=("$crate/$rel: changed by us, removed or renamed upstream")
            fi
        elif [ ! -e "$o" ]; then
            cmp -s "$b" "$t" || todo+=("$crate/$rel: removed by us, changed upstream")
        elif cmp -s "$o" "$b"; then
            if ! cmp -s "$o" "$t"; then
                cp "$t" "$o"
                echo "updated  $crate/$rel"
            fi
        elif ! cmp -s "$b" "$t"; then
            if git merge-file -L ours -L "upstream-$old" -L "upstream-$new" "$o" "$b" "$t"; then
                echo "merged   $crate/$rel"
            else
                todo+=("$crate/$rel: conflicts between markers")
            fi
        fi
    done < <(
        {
            (cd "$base" && find src -type f && echo Cargo.toml)
            (cd "$theirs" && find src -type f)
        } | sort -u
    )
done

# The crates that ship without them have the tailscale crate's.
for f in LICENSE PATENTS; do
    if fetch tailscale "$new" 2>/dev/null && [ -e "$work/$new/tailscale-$new/$f" ]; then
        for dir in "$here"/*/; do
            if [ -e "$dir$f" ] && ! cmp -s "$dir$f" "$work/$new/tailscale-$new/$f"; then
                cp "$work/$new/tailscale-$new/$f" "$dir$f"
                echo "updated  $(basename "$dir")/$f"
            fi
        done
    fi
done

# The READMEs are not vendored (build, in PATCHES.txt).
while IFS= read -r line; do
    todo+=("${line#"$here"/}: a README include to make a one-line doc")
done < <(grep -rnE '^[[:space:]]*#!?\[doc = include_str!\(".*\.md"\)\]' "$here"/*/src || true)
while IFS= read -r f; do
    rm "$f"
done < <(find "$here"/*/ -name '*.md' -type f)

# The macOS monitor is every Apple system's (ios, in PATCHES.txt): a new `target_os = "macos"`
# compiles for iOS all the same when it picks a branch of a cfg_if, so look at every one.
while IFS= read -r line; do
    todo+=("${line#"$here"/}: make it target_vendor = \"apple\" (ios)")
done < <(grep -rn 'target_os = "macos"' "$here"/ts_netmon/src "$here"/ts_netmon/Cargo.toml |
    grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|#[[:space:]])' || true)

if [ ${#todo[@]} -eq 0 ]; then
    echo "merged without anything left to do by hand"
    exit 0
fi
echo
echo "left to do by hand:"
printf '  %s\n' "${todo[@]}"
exit 1
