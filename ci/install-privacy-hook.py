#!/usr/bin/env python3
"""Install a local hook while preserving an existing pre-commit hook."""

import os
from pathlib import Path
import shutil
import subprocess
import sys

MARKER = "# THINKTERM_PRIVACY_GUARD_V1"
WRAPPER = '''#!/bin/sh
# THINKTERM_PRIVACY_GUARD_V1
set -eu
hooks_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
python3 "$hooks_dir/thinkterm-privacy-guard.py" --staged
if [ -x "$hooks_dir/pre-commit.before-privacy-guard" ]; then
    exec "$hooks_dir/pre-commit.before-privacy-guard" "$@"
fi
'''


def main():
    raw = subprocess.check_output(["git", "rev-parse", "--git-path", "hooks/pre-commit"], text=True).strip()
    hook = Path(os.path.abspath(raw))
    backup = hook.with_name("pre-commit.before-privacy-guard")
    scanner = hook.with_name("thinkterm-privacy-guard.py")
    managed = hook.is_file() and MARKER in hook.read_text(errors="replace")
    if os.path.lexists(hook) and not managed and os.path.lexists(backup):
        sys.exit("Existing hook and backup both present; refusing to overwrite either.")
    hook.parent.mkdir(parents=True, exist_ok=True)
    # Keep a local copy: linked worktrees on older branches may lack ci/ scripts.
    scanner_temp = scanner.with_name("thinkterm-privacy-guard.py.new")
    shutil.copyfile(Path(__file__).with_name("check-privacy.py"), scanner_temp)
    scanner_temp.replace(scanner)
    if os.path.lexists(hook) and not managed:
        hook.rename(backup)
    temp = hook.with_name("pre-commit.privacy-new")
    temp.write_text(WRAPPER)
    temp.chmod(0o755)
    temp.replace(hook)
    print("Privacy hook installed. Existing hook preserved and chained.")
    print("Linked worktrees share this hook. Re-run this installer after scanner updates.")


if __name__ == "__main__":
    main()
