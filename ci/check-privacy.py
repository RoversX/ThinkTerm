#!/usr/bin/env python3
"""Check Git blobs, never unstaged files. Requires only Python 3 and Git."""

import argparse
import ipaddress
import json
import re
import subprocess
import sys
from pathlib import PurePosixPath


SAMPLE_USERS = set(b"a ada alice b bob dev developer example me myname pi public root shared someone test testuser u user username x yourusername".split())
# Existing public upstream examples; these are not blanket file exclusions.
UPSTREAM_USERS = {
    b"wez": ("docs/", "assets/", "wezterm-escape-parser/src/tmux_cc/mod.rs"),
    b"alec": ("docs/config/plugins.md",),
    b"joe": ("wezterm-escape-parser/src/tmux_cc/mod.rs",),
    b"fred": ("wezterm-ssh/src/config.rs",),
    b"garrus": ("assets/windows/mesa/opengl32.dll",),
}
APP_TEMPLATE = {
    "assets/macos/ThinkTerm.app/Contents/Info.plist",
    "assets/macos/ThinkTerm.app/Contents/Resources/ThinkTerm.icns",
    "assets/macos/ThinkTerm.app/Contents/Resources/ThinkTerm_simple.icns",
    "assets/macos/ThinkTerm.app/Contents/Resources/terminal.icns",
    "assets/macos/ThinkTerm.app/libEGL.dylib",
    "assets/macos/ThinkTerm.app/libGLESv1_CM.dylib",
    "assets/macos/ThinkTerm.app/libGLESv2.dylib",
    "assets/windows/conhost/OpenConsole.exe",
}
ARCHIVES = (".zip", ".tar.gz", ".tar.xz", ".tgz", ".dmg", ".apk", ".aab", ".ipa", ".deb", ".rpm", ".appimage", ".msi", ".exe")
HOME = re.compile(rb"(?:/(?:Users|home)/|[A-Za-z]:[\\/]+Users[\\/]+)([A-Za-z_\x80-\xff][A-Za-z0-9_.\x80-\xff-]*)")
# Match actual multiline key material, not the header strings used by SSH code.
PRIVATE_KEY = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----(?:\r?\n|\\n)(?:(?:Proc-Type:|DEK-Info:)[^\r\n]+\r?\n)*[\r\n]*(?:[A-Za-z0-9+/=]{32,})(?:\r?\n|\\n)")
TOKENS = re.compile(rb"(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,}|xox[baprs]-[A-Za-z0-9-]{20,}|sk-(?:proj-|ant-)[A-Za-z0-9_-]{32,}|AKIA[0-9A-Z]{16})")
SSH_IP = re.compile(rb"ssh://[^\s/@\"'<>]+@((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?=[:/\s\"'<>]|$)")
DOCUMENTATION_NETS = tuple(ipaddress.ip_network(n) for n in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24"))


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.PIPE)


def path_issues(path):
    parts = PurePosixPath(path).parts
    name = parts[-1].lower()
    issues = set()
    if path not in APP_TEMPLATE and (name.endswith(ARCHIVES) or any(p.lower().endswith(".app") for p in parts)):
        issues.add("release-artifact: distribute packages outside Git")
    if any(p in ("target", "XCBuildData", "DerivedData") for p in parts) or path.startswith(("ios/build/", "android/build/", "android/app/build/")):
        issues.add("build-output: ignore generated files")
    if (name.startswith(".env") and name not in (".env.example", ".env.sample", ".env.template")) or name in ("probe_key", "probe_user", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "local.xcconfig", "local.properties") or name.endswith((".p12", ".pfx", ".keystore", ".jks")):
        issues.add("local-credentials: keep this file untracked")
    return issues


def content_issues(path, data):
    issues = set()
    if b"PRIVATE KEY-----" in data and PRIVATE_KEY.search(data):
        issues.add("private-key: remove key material")
    if any(marker in data for marker in (b"/Users/", b"/home/", b"Users\\", b"Users/")):
        for match in HOME.finditer(data):
            user = match[1].lower()
            if user in SAMPLE_USERS:
                continue
            if any(path == prefix or (prefix.endswith("/") and path.startswith(prefix)) for prefix in UPSTREAM_USERS.get(user, ())):
                continue
            issues.add("personal-home: use an environment variable or an alice/example fixture")
    # Binary assets are not interpreted as text tokens or endpoint examples.
    if b"\0" not in data[:8000]:
        if any(marker in data for marker in (b"gh", b"github_pat_", b"xox", b"sk-", b"AKIA")) and TOKENS.search(data):
            issues.add("provider-token: remove the credential")
        if b"ENCRYPTED" + b"[" in data:
            issues.add("embedded-ci-credential: use repository secrets")
        for match in SSH_IP.finditer(data):
            try:
                address = ipaddress.ip_address(match[1].decode("ascii"))
            except ValueError:
                continue
            if not address.is_loopback and not any(address in net for net in DOCUMENTATION_NETS):
                issues.add("ssh-endpoint: use a documentation IP in examples")
    return issues


def entries(tree=None):
    if tree is None:
        for row in git("ls-files", "--full-name", "--stage", "-z", "--", ":/").split(b"\0"):
            if not row:
                continue
            meta, path = row.split(b"\t", 1)
            mode, oid, stage = meta.split()
            if stage != b"0":
                raise ValueError("Resolve unmerged index entries before running the privacy check")
            if mode != b"160000":  # Submodules are separate repositories.
                yield path.decode("utf-8", "surrogateescape"), oid
    else:
        tree_id = git("rev-parse", "--verify", "--end-of-options", tree + "^{tree}").strip().decode("ascii")
        for row in git("ls-tree", "--full-tree", "-r", "-z", tree_id).split(b"\0"):
            if not row:
                continue
            meta, path = row.split(b"\t", 1)
            mode, kind, oid = meta.split()
            if kind == b"blob":
                yield path.decode("utf-8", "surrogateescape"), oid


def check(tree=None):
    failures = 0
    count = 0
    with subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as batch:
        try:
            for path, oid in entries(tree):
                batch.stdin.write(oid + b"\n")
                batch.stdin.flush()
                header = batch.stdout.readline().split()
                if len(header) != 3 or header[1] != b"blob":
                    raise ValueError("Unable to read a Git blob")
                size = int(header[2])
                data = batch.stdout.read(size)
                if len(data) != size or batch.stdout.read(1) != b"\n":
                    raise ValueError("Incomplete Git blob")
                issues = path_issues(path) | content_issues(path, data)
                count += 1
                for issue in sorted(issues):
                    # No matched values, snippets or key material in terminal/CI logs.
                    print("privacy:", json.dumps(path), "-", issue, file=sys.stderr)
                    failures += 1
        finally:
            batch.stdin.close()
            batch.stdout.close()
        if batch.wait() != 0:
            raise ValueError("Git blob reader failed")
    if failures:
        print(f"Privacy check blocked: {failures} finding(s). Fix and re-stage the files.", file=sys.stderr)
        return 1
    print(f"Privacy check passed: {count} Git blobs checked.")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true", help="check the complete index (default)")
    mode.add_argument("--tree", help="check a committed tree, e.g. HEAD")
    args = parser.parse_args()
    try:
        return check(args.tree)
    except (OSError, ValueError, subprocess.CalledProcessError):
        print("Privacy check could not complete; commit blocked. Check Python 3, Git and the index.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
