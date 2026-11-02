#!/usr/bin/env python3
"""Write canonical release metadata from local packages (later dirs win)."""

import hashlib
import re
import sys
from pathlib import Path


def safe_name(name):
    return len(name) <= 200 and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", name)


def manifest(tag, directories):
    if not safe_name(tag):
        raise ValueError("invalid release tag")
    packages = {}
    for directory in directories:
        for path in Path(directory).iterdir():
            name = path.name
            if name in ("thinkterm-release.txt", "thinkterm-release.txt.sig"):
                continue
            if name.endswith(("-dSYM.zip", "-pdb.zip")):
                continue
            if not safe_name(name) or path.is_symlink() or not path.is_file():
                raise ValueError("unexpected release asset")
            packages[name] = path
    if not packages:
        raise ValueError("no packages to sign")
    rows = ["thinkterm-release-v1", f"version {tag}"]
    for name, path in sorted(packages.items()):
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
                size += len(chunk)
        if not 0 < size <= 8 * 1024 * 1024 * 1024:
            raise ValueError("invalid release asset size")
        rows.append(f"{digest.hexdigest()} {size} {name}")
    result = "\n".join(rows) + "\n"
    if len(result) > 1024 * 1024:
        raise ValueError("release metadata exceeds size limit")
    return result


if __name__ == "__main__":
    if len(sys.argv) < 4:
        sys.exit("usage: release-manifest.py TAG OUTPUT PACKAGE_DIR [PACKAGE_DIR ...]")
    try:
        Path(sys.argv[2]).write_text(manifest(sys.argv[1], sys.argv[3:]), encoding="utf-8")
    except (OSError, ValueError) as error:
        sys.exit(str(error))
