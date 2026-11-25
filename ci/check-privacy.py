#!/usr/bin/env python3
"""Check Git blobs, never unstaged files. Requires only Python 3 and Git."""

import argparse
import codecs
import ipaddress
import json
import os
import re
import socket
import subprocess
import sys
from itertools import chain
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
    "assets/macos/ThinkTerm.app/Contents/Resources/ThinkTerm_otter.icns",
    "assets/macos/ThinkTerm.app/Contents/Resources/ThinkTerm_red.icns",
    "assets/macos/ThinkTerm.app/Contents/Resources/ThinkTerm_simple.icns",
    "assets/macos/ThinkTerm.app/Contents/Resources/terminal.icns",
    "assets/macos/ThinkTerm.app/libEGL.dylib",
    "assets/macos/ThinkTerm.app/libGLESv1_CM.dylib",
    "assets/macos/ThinkTerm.app/libGLESv2.dylib",
    "assets/windows/conhost/OpenConsole.exe",
}
ARCHIVES = (".zip", ".tar.gz", ".tar.xz", ".tgz", ".dmg", ".apk", ".aab", ".ipa", ".deb", ".rpm", ".appimage", ".msi", ".exe")
HOME = re.compile(rb"(?:/(?:Users|home)/|[A-Za-z]:[\\/]+Users[\\/]+)([A-Za-z_\x80-\xff][A-Za-z0-9_.\x80-\xff-]*)")
# The same path as tools write it into a file or folder name, its slashes
# turned into dashes: /Users/<name>/src as -Users-<name>-src (coding agents
# name their project folders so), C:\Users\<name> as C--Users-<name>. The
# name ends at the next dash, which is where the path went on. Not after a
# letter, digit or dash, so words like nav-home-link and --home-dir pass.
DASHED_HOME = re.compile(rb"(?<![A-Za-z0-9_.-])(?:[A-Za-z]-)?-(?:Users|home)-([A-Za-z_\x80-\xff][A-Za-z0-9_\x80-\xff]*)")
# Match actual multiline key material, not the header strings used by SSH code.
PRIVATE_KEY = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----(?:\r?\n|\\n)(?:(?:Proc-Type:|DEK-Info:)[^\r\n]+\r?\n)*[\r\n]*([A-Za-z0-9+/=]{32,})(?:\r?\n|\\n)")
TOKENS = re.compile(
    rb"gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,}|xox[baprs]-[A-Za-z0-9-]{20,}"
    rb"|sk-(?:proj-|ant-)[A-Za-z0-9_-]{32,}|AKIA[0-9A-Z]{16}|tskey-auth-(?:[A-Za-z0-9_]+-)?[A-Za-z0-9_]{16,}"
)
TOKEN_MARKERS = (b"gh", b"github_pat_", b"xox", b"sk-", b"AKIA", b"tskey-auth-")
CI_CREDENTIAL = re.compile(rb"ENCRYPTED" rb"\[([^\]\r\n]*)")
DOCUMENTATION_NETS = tuple(ipaddress.ip_network(n) for n in (
    "192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24", "2001:db8::/32", "3fff::/20",
))
# A machine name in a value position: quoted, after `@`, or inside a URL. A
# bare foo.local is deliberately not read as a host, because in source it is
# far more often a field access -- catalog.local, entry.local -- and the
# leak this looks for is a real name committed as a literal.
DNS_LABEL = rb"[A-Za-z0-9][A-Za-z0-9_-]{0,62}"
HOST_LABELS = rb"(" + DNS_LABEL + rb"(?:\." + DNS_LABEL + rb")*)\."
LOCAL_SUFFIX = rb"(?:local|lan|home\.arpa)"
HOSTNAME = re.compile(rb"""(?:['"`]|@|://)(""" + DNS_LABEL + rb")\." + LOCAL_SUFFIX + rb"(?![A-Za-z0-9_.-])", re.IGNORECASE)
REPORT_SUFFIXES = rb"|".join(re.escape(s.encode()) for s in (*ARCHIVES, ".txt", ".log", ".csv"))
PATH_HOSTNAME = re.compile(rb"(?:/|@)" + HOST_LABELS + LOCAL_SUFFIX + rb"(?=/|$|(?:" + REPORT_SUFFIXES + rb")\Z)", re.IGNORECASE)
SSH_LOCAL_HOST = re.compile(HOST_LABELS + rb"(?:local|lan|home\.arpa|internal|corp)", re.IGNORECASE)
SSH_URL = re.compile(
    rb"""\bssh://(?:[^\s/@"'<>`]+@)?(\[[\w.:%-]+\]|[\w.%-]+)(?=[:/?#\s"'<>`]|$)""",
    re.IGNORECASE,
)
SSH_CONFIG = re.compile(
    rb"""^[ \t]*hostname(?:[ \t]*=[ \t]*|[ \t]+)["']?([\w.:%\[\]-]+)["']?[ \t]*(?=#|\r?$)""",
    re.IGNORECASE | re.MULTILINE,
)
SSH_COMMAND = re.compile(rb"(?<![\w.-])ssh[ \t]+", re.IGNORECASE)
SSH_WORD = re.compile(rb"""[ \t]*(?:"([^"\r\n]*)"|'([^'\r\n]*)'|((?:\\[^\r\n]|[^\s"'`\\;|&<>()])+))""")
SSH_OPTION_WITH_VALUE = re.compile(rb"[BbcDEeFIiJLlmOoPpQRSWw]")
# Match only the existing fixture directive or command arguments in each file.
SSH_FIXTURES = {
    "wezterm-ssh/src/config.rs": b"HostName 10.0.0.1",
    "wezterm-gui/src/ssh_hosts.rs": b"HostName 10.0.0.1",
    "docs/config/lua/SshDomain.md": b"-T -o BatchMode=yes me@10.0.0.5",
}
SAMPLE_HOSTS = set(b"alice bob example host hostname localhost machine myhost server test".split())
# Vendored labels that share the shape without naming a machine.
VENDOR_HOSTS = {
    b"dockerfile": ("third_party/material-icon-theme/",),
    b"containerfile": ("third_party/material-icon-theme/",),
}
# Personal names are never stored in the repository: they are read from the
# machine at scan time, plus this optional untracked file, one per line.
LOCAL_LIST = "ci/privacy-local.txt"


class LocalListError(OSError):
    """The local name list is present but could not be read."""


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.PIPE)


def local_list_names():
    # A tree can also be scanned in a bare repository with no working files.
    if git("rev-parse", "--is-inside-work-tree").strip() == b"false":
        return
    root = git("rev-parse", "--show-toplevel").rstrip(b"\r\n").decode("utf-8", "surrogateescape")
    filename = os.path.join(root, LOCAL_LIST)
    try:
        with open(filename, encoding="utf-8-sig") as handle:
            for line in handle:
                yield line.split("#", 1)[0]
    except (OSError, UnicodeError) as error:
        if isinstance(error, FileNotFoundError) and not os.path.lexists(filename):
            return
        raise LocalListError from error


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
    names.update(local_list_names())
    found = set()
    for name in names:
        bare = name.strip().strip(".")
        for form in (bare, bare.rsplit(".", 1)[0] if "." in bare else bare):
            # Names shorter than four characters are skipped, including local
            # list entries, because they collide with ordinary source words.
            if len(form) >= 4:
                found.add(form.encode("utf-8", "replace"))
    return tuple(sorted(found))


def personal_pattern(names=None, *, filename=False):
    names = local_names() if names is None else names
    if not names:
        return None
    boundary = rb"[A-Za-z0-9]" if filename else rb"[A-Za-z0-9_-]"
    return re.compile(rb"(?<!" + boundary + rb")(?:" + rb"|".join(re.escape(n) for n in names) + rb")(?!" + boundary + rb")", re.IGNORECASE)


def redacted_path(path, values):
    if not values:
        return path
    hidden = {part.lower() for value in values for part in value.split(b"/") if part}
    parts = path.encode("utf-8", "surrogateescape").split(b"/")
    for index, part in enumerate(parts):
        # A dashed home path can contain more of the username after the match.
        folded_part = part.lower()
        if any(value in folded_part for value in hidden):
            parts[index] = b"[redacted]"
    return b"/".join(parts).decode("utf-8", "surrogateescape")


def path_findings(path, personal):
    parts = PurePosixPath(path).parts
    name = parts[-1].lower()
    # Include root-level home directories. Upstream content exceptions do not cover filenames.
    data = b"/" + path.encode("utf-8", "surrogateescape")
    yield from private_literals("", data, personal, host_pattern=PATH_HOSTNAME)
    if path == LOCAL_LIST:
        yield "personal-host-list: keep this file untracked", None
    if path not in APP_TEMPLATE and (name.endswith(ARCHIVES) or any(p.lower().endswith(".app") for p in parts)):
        yield "release-artifact: distribute packages outside Git", None
    if any(p in ("target", "XCBuildData", "DerivedData") for p in parts) or path.startswith(("ios/build/", "android/build/", "android/app/build/")):
        yield "build-output: ignore generated files", None
    if (name.startswith(".env") and name not in (".env.example", ".env.sample", ".env.template")) or name in ("probe_key", "probe_user", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "local.xcconfig", "local.properties") or name.endswith((".p12", ".pfx", ".keystore", ".jks")):
        yield "local-credentials: keep this file untracked", None


def path_issues(path, personal=None):
    return {issue for issue, _ in path_findings(path, personal)}


def example_host(path, label):
    label = label.lower()
    return label in SAMPLE_HOSTS or any(path.startswith(prefix) for prefix in VENDOR_HOSTS.get(label, ()))


def private_literals(path, data, personal, *, text=True, host_pattern=HOSTNAME):
    if b"PRIVATE KEY-----" in data:
        for match in PRIVATE_KEY.finditer(data):
            yield "private-key: remove key material", match[1]
    if any(marker in data for marker in (b"/Users/", b"/home/", b"Users\\", b"Users/", b"-Users-", b"-home-")):
        for pattern in (HOME, DASHED_HOME):
            for match in pattern.finditer(data):
                user = match[1].lower()
                if user in SAMPLE_USERS:
                    continue
                if any(path == prefix or (prefix.endswith("/") and path.startswith(prefix)) for prefix in UPSTREAM_USERS.get(user, ())):
                    continue
                yield "personal-home: use an environment variable or an alice/example fixture", match[1]
    if not text:
        return
    for match in host_pattern.finditer(data):
        if not example_host(path, match[1]):
            yield "personal-hostname: use an example host in fixtures", match[1]
    if personal is not None:
        for match in personal.finditer(data):
            yield "personal-machine: remove the local machine name", match[0]
    if any(marker in data for marker in TOKEN_MARKERS):
        for match in TOKENS.finditer(data):
            yield "provider-token: remove the credential", match[0]
    if b"ENCRYPTED" + b"[" in data:
        for match in CI_CREDENTIAL.finditer(data):
            yield "embedded-ci-credential: use repository secrets", match[1] or match[0]


def ssh_command_host(data, offset):
    skip_value = False
    while True:
        match = SSH_WORD.match(data, offset)
        if not match:
            return None
        word = match[match.lastindex]
        offset = match.end()
        if skip_value:
            skip_value = False
        elif word.startswith(b"-"):
            # A short option may take its argument in the same word or the next.
            option = SSH_OPTION_WITH_VALUE.search(word, 1)
            skip_value = option is not None and option.end() == len(word)
        else:
            host = word.rsplit(b"@", 1)[-1].partition(b"\\")[0]
            return host, offset


def ssh_hosts(path, data):
    lower = data.lower()
    fixture = SSH_FIXTURES.get(path)
    if b"ssh://" in lower:
        for match in SSH_URL.finditer(data):
            yield match[1]
    if b"hostname" in lower:
        for match in SSH_CONFIG.finditer(data):
            if match[0].strip() != fixture:
                yield match[1]
    if b"ssh " in lower or b"ssh\t" in lower:
        for match in SSH_COMMAND.finditer(data):
            target = ssh_command_host(data, match.end())
            if target:
                host, end = target
                if fixture is None or data[match.end():end] != fixture:
                    yield host


def byte_findings(path, data, personal):
    text = b"\0" not in data[:8000]
    yield from private_literals(path, data, personal, text=text)
    if not text:
        return
    for host in ssh_hosts(path, data):
        host = host.strip(b"[]").rstrip(b".")
        try:
            address = ipaddress.ip_address(host.decode("ascii"))
        except ValueError:
            match = SSH_LOCAL_HOST.fullmatch(host)
            if match and not example_host(path, match[1]):
                yield "personal-hostname: use an example host in fixtures", match[1]
            continue
        if address.version == 6 and address.ipv4_mapped:
            address = address.ipv4_mapped
        if not address.is_loopback and not any(address in net for net in DOCUMENTATION_NETS):
            yield "ssh-endpoint: use a documentation IP in examples", host


def content_findings(path, data, personal):
    yield from byte_findings(path, data, personal)
    if data.startswith((codecs.BOM_UTF16_LE, codecs.BOM_UTF16_BE)) and not data.startswith(codecs.BOM_UTF32_LE):
        decoded = data.decode("utf-16", errors="replace").encode("utf-8")
        yield from byte_findings(path, decoded, personal)


def content_issues(path, data, personal=None):
    return {issue for issue, _ in content_findings(path, data, personal)}


def inspect_blob(path, data, personal=None, filename_personal=None):
    folded_path = b"/" + path.encode("utf-8", "surrogateescape").lower()
    issues = set()
    hidden = set()
    for issue, value in chain(path_findings(path, filename_personal), content_findings(path, data, personal)):
        issues.add(issue)
        # A name identified in the content can also occur in an ordinary filename.
        if value and len(value) <= len(folded_path) and value.lower() in folded_path:
            hidden.add(value)
    return issues, redacted_path(path, hidden)


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
    names = local_names()
    personal = personal_pattern(names)
    filename_personal = personal_pattern(names, filename=True)
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
                issues, display = inspect_blob(path, data, personal, filename_personal)
                count += 1
                if issues:
                    # No matched values, snippets or key material in terminal/CI logs.
                    for issue in sorted(issues):
                        print("privacy:", json.dumps(display), "-", issue, file=sys.stderr)
                    failures += len(issues)
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
    except LocalListError:
        print(f"Privacy check could not read {LOCAL_LIST}; commit blocked. Check its permissions and UTF-8 encoding.", file=sys.stderr)
        return 2
    except (OSError, ValueError, subprocess.CalledProcessError):
        print("Privacy check could not complete; commit blocked. Check Python 3, Git and the index.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
