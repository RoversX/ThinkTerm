#!/usr/bin/env python3
"""Check Git blobs, never unstaged files. Requires only Python 3 and Git."""

import argparse
import ipaddress
import json
import os
import re
import socket
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
# A machine name in a value position: quoted, after `@`, or inside a URL. A
# bare foo.local is deliberately not read as a host, because in source it is
# far more often a field access -- catalog.local, entry.local -- and the
# leak this looks for is a real name committed as a literal.
HOSTNAME = re.compile(rb"(?:['\"`]|@|://)([A-Za-z0-9][A-Za-z0-9_-]{0,62})\.(?:local|lan|home\.arpa)(?![A-Za-z0-9_.-])", re.IGNORECASE)
SAMPLE_HOSTS = set(b"alice bob example host hostname localhost machine myhost server test".split())
# Vendored labels that share the shape without naming a machine.
VENDOR_HOSTS = {
    b"dockerfile": ("third_party/material-icon-theme/",),
    b"containerfile": ("third_party/material-icon-theme/",),
}
# Personal names are never stored in the repository: they are read from the
# machine at scan time, plus this optional untracked file, one per line.
LOCAL_LIST = "ci/privacy-local.txt"


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.PIPE)


def local_names():
    """This machine's own names, read at run time and never written down.

    A denylist of personal names committed to the repository would publish
    exactly the strings it is meant to keep out, so nothing here is stored:
    the names come from the running machine and from an untracked file. On CI
    the runner's own name says nothing about the developer and would risk
    matching an ordinary word, so the check is skipped there.
    """
    if os.environ.get("CI"):
        return ()
    names = set()
    try:
        names.add(socket.gethostname())
    except OSError:
        pass
    if sys.platform == "darwin":
        for key in ("ComputerName", "LocalHostName"):
            try:
                names.add(subprocess.check_output(["scutil", "--get", key], stderr=subprocess.DEVNULL, timeout=5).decode("utf-8", "replace"))
            except (OSError, subprocess.SubprocessError):
                pass
    try:
        root = git("rev-parse", "--show-toplevel").strip().decode("utf-8", "surrogateescape")
        with open(os.path.join(root, LOCAL_LIST), "rb") as handle:
            for line in handle:
                names.add(line.split(b"#", 1)[0].decode("utf-8", "replace"))
    except (OSError, subprocess.CalledProcessError):
        pass
    found = set()
    for name in names:
        bare = name.strip().strip(".")
        for form in (bare, bare.rsplit(".", 1)[0] if "." in bare else bare):
            # Names shorter than four characters are skipped, including local
            # list entries, because they collide with ordinary source words.
            if len(form) >= 4:
                found.add(form.encode("utf-8", "replace"))
    return tuple(sorted(found))


def personal_pattern(names=None):
    names = local_names() if names is None else names
    if not names:
        return None
    return re.compile(rb"(?<![A-Za-z0-9_-])(?:" + rb"|".join(re.escape(n) for n in names) + rb")(?![A-Za-z0-9_-])", re.IGNORECASE)


def path_issues(path):
    parts = PurePosixPath(path).parts
    name = parts[-1].lower()
    issues = set()
    if path == LOCAL_LIST:
        issues.add("personal-host-list: keep this file untracked")
    if path not in APP_TEMPLATE and (name.endswith(ARCHIVES) or any(p.lower().endswith(".app") for p in parts)):
        issues.add("release-artifact: distribute packages outside Git")
    if any(p in ("target", "XCBuildData", "DerivedData") for p in parts) or path.startswith(("ios/build/", "android/build/", "android/app/build/")):
        issues.add("build-output: ignore generated files")
    if (name.startswith(".env") and name not in (".env.example", ".env.sample", ".env.template")) or name in ("probe_key", "probe_user", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "local.xcconfig", "local.properties") or name.endswith((".p12", ".pfx", ".keystore", ".jks")):
        issues.add("local-credentials: keep this file untracked")
    return issues


def content_issues(path, data, personal=None):
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
        for match in HOSTNAME.finditer(data):
            host = match[1].lower()
            if host in SAMPLE_HOSTS:
                continue
            if any(path.startswith(prefix) for prefix in VENDOR_HOSTS.get(host, ())):
                continue
            issues.add("personal-hostname: use an example host in fixtures")
        if personal is not None and personal.search(data):
            issues.add("personal-machine: remove the local machine name")
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
    personal = personal_pattern()
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
                issues = path_issues(path) | content_issues(path, data, personal)
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
