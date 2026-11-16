#!/usr/bin/env python3
"""Write the licenses the iOS app shows (Settings, Open Source Licenses).

Everything the app ships that is not ThinkTerm's own: the WezTerm code it
grew from, the fonts and icons in the bundle, the Rust standard library,
and every crate linked into the core (thinkterm-mobile, for the iPhone
target, as `cargo tree` resolves it). Each with its license as its authors
wrote it, from the crate's own files; a crate that ships none gets the
standard text of what its manifest names. Proc-macro crates run in the
compiler and are not in the app, so they are left out.

    python3 ios/tools/gen-licenses.py OUT.json [TARGET]

Run by ios/build.sh; the output is generated, not kept in the tree. It
holds no paths of the machine it was made on.
"""

import hashlib
import html
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
TARGET = sys.argv[2] if len(sys.argv) > 2 else "aarch64-apple-ios"

# A crate's license files: LICENSE, LICENSE-MIT, COPYING, NOTICE and so on,
# and the files of a LICENSES/ directory (REUSE's layout).
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)", re.I)


def run(*args):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True).stdout


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read().strip()


def license_files(directory):
    found = []
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if not LICENSE_FILE.match(name):
            continue
        if os.path.isfile(path):
            found.append(path)
        elif os.path.isdir(path):
            found += [os.path.join(path, n) for n in sorted(os.listdir(path))
                      if os.path.isfile(os.path.join(path, n))]
    return found


LIST_ITEM = re.compile(r"^(\s*([-*+•]|\(?[0-9a-zA-Z]{1,3}[.)])\s)")


def reflow(text):
    """A license file's paragraphs as running text: they are wrapped at 80
    columns for a terminal, and a phone's narrower lines would break each
    of them mid-sentence. A paragraph that is not plain prose (a list, a
    table, art, lines aligned with runs of spaces) keeps its lines."""
    out = []
    for paragraph in re.split(r"\n[ \t]*\n", text):
        lines = paragraph.split("\n")
        indents = [len(l) - len(l.lstrip()) for l in lines if l.strip()]
        prose = (
            len(lines) > 1
            and len(set(indents)) <= 2
            and not any(LIST_ITEM.match(l) for l in lines[1:])
            and not any(re.search(r"\S {3,}\S", l) for l in lines)
            and not any(set(l.strip()) <= set("-=*_#~+|") for l in lines)
        )
        if prose:
            # A heading in capitals on the paragraph's first line, and each
            # copyright line, stay lines of their own.
            lines = [l.strip() for l in lines]
            heading = lines[0].isupper() and len(lines[0]) < 60
            joined = [lines[0]]
            for i, line in enumerate(lines[1:], 1):
                if (i == 1 and heading) or line.lower().startswith(("copyright", "(c)", "©")):
                    joined.append(line)
                else:
                    joined[-1] += " " + line
            out.append("\n".join(joined))
        else:
            out.append(paragraph)
    return "\n\n".join(out)


class Texts:
    """License texts, each kept once however many crates share it."""

    def __init__(self):
        self.by_id = {}

    def add(self, text):
        text = text.strip()
        key = "t" + hashlib.sha1(text.encode()).hexdigest()[:12]
        self.by_id[key] = reflow(text)
        return key


texts = Texts()
sysroot = run("rustc", "--print", "sysroot").strip()
# Standard texts by SPDX id: the Rust toolchain's copies, and the few it
# lacks that a crate here needs (kept beside this script, or already in the
# tree).
STANDARD = os.path.join(sysroot, "share", "doc", "rust", "licenses")
STANDARD_HERE = {
    "MPL-2.0": os.path.join(HERE, "licenses", "MPL-2.0.txt"),
    "CC0-1.0": os.path.join(ROOT, "third_party", "simple-icons", "LICENSE.md"),
}


def standard_texts(expression, holders):
    """The standard text of each license `expression` names, for a crate
    that ships none. MIT's template gets the crate's authors as its
    holders."""
    ids = re.findall(r"[A-Za-z0-9.+-]+", expression.replace("/", " OR "))
    ids = [i for i in ids if i not in ("OR", "AND", "WITH")]
    found = []
    for spdx in dict.fromkeys(ids):
        path = STANDARD_HERE.get(spdx) or os.path.join(STANDARD, spdx + ".txt")
        if not os.path.isfile(path):
            continue
        text = read(path)
        if spdx == "MIT" and holders:
            text = text.replace("<year> <copyright holders>", holders)
        found.append(texts.add(text))
    return found


# The crates the app links, from the dependency tree of the core.
tree = run("cargo", "tree", "-p", "thinkterm-mobile", "--target", TARGET, "-e", "normal",
           "--prefix", "none", "-f", "{p}")
linked = set()
for line in tree.splitlines():
    m = re.match(r"^(\S+) v(\S+)( \(proc-macro\))?", line.strip())
    if m and not m.group(3):
        linked.add((m.group(1), m.group(2)))

metadata = json.loads(run("cargo", "metadata", "--format-version", "1"))
packages = {(p["name"], p["version"]): p for p in metadata["packages"]}

items = []
wezterm, thinkterm_crates, vendored = [], [], {}
crates = []
for key in sorted(linked):
    package = packages.get(key)
    if package is None:
        sys.exit(f"gen-licenses: {key[0]} {key[1]} is in the tree but not in cargo metadata")
    directory = os.path.dirname(package["manifest_path"])
    if package["source"] is None:
        relative = os.path.relpath(directory, ROOT)
        if relative.startswith("deps" + os.sep):
            # A vendored project: one entry for all its crates.
            vendored.setdefault(relative.split(os.sep)[1], []).append(package)
        elif package.get("license"):
            wezterm.append(package)
        else:
            thinkterm_crates.append(package)
    else:
        crates.append(package)

# ThinkTerm: the notice's first part (the rest is about other platforms'
# packages) and the GPL.
notice = read(os.path.join(ROOT, "NOTICE")).split("\n---\n")[0]
items.append({
    "section": "app",
    "name": "ThinkTerm",
    "license": "GPL-3.0-only",
    "url": "https://github.com/RoversX/ThinkTerm",
    "texts": [texts.add(notice), texts.add(read(os.path.join(ROOT, "LICENSE.md")))],
})

# The WezTerm crates the core is built from, MIT; one of them also carries
# Unicode's data license, in its own file.
wezterm_texts = [texts.add(read(os.path.join(ROOT, "LICENSE-MIT")))]
for package in wezterm:
    for path in license_files(os.path.dirname(package["manifest_path"])):
        wezterm_texts.append(texts.add(read(path)))
items.append({
    "section": "app",
    "name": "WezTerm",
    "license": "MIT",
    "url": "https://github.com/wezterm/wezterm",
    "includes": sorted(p["name"] for p in wezterm),
    "texts": list(dict.fromkeys(wezterm_texts)),
})

for name, members in sorted(vendored.items()):
    directory = os.path.join(ROOT, "deps", name)
    found = [texts.add(read(p)) for p in license_files(directory)]
    for package in members:
        found += [texts.add(read(p)) for p in license_files(os.path.dirname(package["manifest_path"]))]
    first = members[0]
    if not found:
        found = standard_texts(first.get("license") or "", "")
    items.append({
        "section": "app",
        "name": name,
        "version": first["version"],
        "license": first.get("license") or "",
        "url": first.get("repository") or "",
        "includes": sorted(p["name"] for p in members),
        "texts": list(dict.fromkeys(found)),
    })

# The fonts and icons in the bundle, as NOTICE and the fonts' own name
# tables credit them.
OFL = read(os.path.join(ROOT, "assets", "fonts", "LICENSE_OFL.txt"))
ASSETS = [
    ("JetBrains Mono", "OFL-1.1", "https://github.com/JetBrains/JetBrainsMono",
     ["Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)\n\n" + OFL]),
    ("Fira Code", "OFL-1.1", "https://github.com/tonsky/FiraCode",
     ["Copyright 2014-2021 The Fira Code Project Authors (https://github.com/tonsky/FiraCode)\n\n" + OFL]),
    ("Symbols Nerd Font Mono", "MIT", "https://github.com/ryanoasis/nerd-fonts",
     ["Copyright (c) 2016, Ryan McIntyre\n\n"
      "Nerd Fonts is released under the MIT License. The glyphs it gathers keep "
      "their own projects' licenses, listed at "
      "https://github.com/ryanoasis/nerd-fonts#glyph-sets.",
      read(os.path.join(ROOT, "assets", "fonts", "LICENSE_POWERLINE_EXTRA.txt"))]),
    ("Lucide", "ISC AND MIT", "https://lucide.dev",
     [read(os.path.join(ROOT, "third_party", "lucide", "LICENSE"))]),
    ("Lobe Icons", "MIT", "https://github.com/lobehub/lobe-icons",
     [read(os.path.join(ROOT, "third_party", "lobe-icons", "LICENSE"))]),
    ("Simple Icons", "CC0-1.0", "https://simpleicons.org",
     [read(os.path.join(ROOT, "third_party", "simple-icons", "LICENSE.md")),
      "Brand marks and logos are trademarks of their respective owners. They are "
      "used solely to identify the corresponding product or service and imply no "
      "affiliation with or endorsement by those owners."]),
]
for name, spdx, url, bodies in ASSETS:
    items.append({"section": "assets", "name": name, "license": spdx, "url": url,
                  "texts": [texts.add(b) for b in bodies]})

# The Rust standard library, linked into the core. Its copyright file is
# HTML; the text of it, with the markup and the blank runs taken out.
copyright_html = os.path.join(sysroot, "share", "doc", "rust", "COPYRIGHT-library.html")
std_texts = []
if os.path.isfile(copyright_html):
    plain = html.unescape(re.sub(r"<[^>]+>", "", read(copyright_html)))
    plain = re.sub(r"[ \t]+\n", "\n", plain)
    plain = re.sub(r"\n{3,}", "\n\n", plain)
    std_texts.append(texts.add(plain))
std_texts += standard_texts("MIT OR Apache-2.0", "The Rust Project Developers")
version = re.search(r"\d+\.\d+\.\d+", run("rustc", "--version")).group(0)
items.append({"section": "rust", "name": "Rust standard library", "version": version,
              "license": "MIT OR Apache-2.0", "url": "https://github.com/rust-lang/rust",
              "texts": std_texts})

def repository(package):
    return (package.get("repository") or "").rstrip("/").removesuffix(".git").lower()


# A crate published without its project's license file gets the file a
# sibling crate of the same project and the same license ships (the
# tailscale-rs crates on crates.io, from the copy vendored in deps/), before
# a standard text. The license must match: one repository can hold crates
# under different ones.
by_repository = {}
for package in crates + [p for members in vendored.values() for p in members]:
    found = license_files(os.path.dirname(package["manifest_path"]))
    if found and repository(package):
        by_repository.setdefault((repository(package), package.get("license") or ""), found)

missing = []
for package in crates:
    found = [texts.add(read(p)) for p in license_files(os.path.dirname(package["manifest_path"]))]
    expression = package.get("license") or ""
    sibling = by_repository.get((repository(package), expression))
    if not found and sibling:
        found = [texts.add(read(p)) for p in sibling]
    if not found:
        found = standard_texts(expression, ", ".join(package.get("authors") or []))
        missing.append(package["name"])
    items.append({
        "section": "rust",
        "name": package["name"],
        "version": package["version"],
        "license": expression,
        "url": package.get("repository") or f"https://crates.io/crates/{package['name']}",
        "texts": list(dict.fromkeys(found)),
    })

unlicensed = [i["name"] for i in items if not i["texts"]]
if unlicensed:
    sys.exit("gen-licenses: no license text for " + ", ".join(unlicensed))

used = {t for i in items for t in i["texts"]}
out = {"items": items, "texts": {k: v for k, v in texts.by_id.items() if k in used}}
encoded = json.dumps(out, ensure_ascii=False, separators=(",", ":"))
# What ships must not carry this machine's paths.
for marker in (os.path.expanduser("~"), ROOT):
    if marker in encoded:
        sys.exit("gen-licenses: the output holds a path of this machine")
with open(sys.argv[1], "w", encoding="utf-8") as f:
    f.write(encoded)
print(f"licenses: {len(items)} entries, {len(out['texts'])} texts, "
      f"{len(encoded) // 1024} KiB; standard texts for {len(missing)} crates without their own")
