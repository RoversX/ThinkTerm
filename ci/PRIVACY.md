# Privacy checks

Install the local guard once per clone (Python 3 and Git are required):

```sh
python3 ci/install-privacy-hook.py
```

The installer preserves and chains any existing pre-commit hook, including
GitButler's hook. Linked worktrees share the installation. The scanner is copied
into Git's hooks directory so older branches still work; run the installer again
after updating the scanner. Fresh clones do not inherit hooks automatically.

Before committing, the guard reads the **complete Git index**, not the working
files. A secret still staged will be rejected even if the working file has been
cleaned. Fix the file, then stage the correction. Ignored, untracked local keys
are not read. Matched secret values are never printed.

```sh
python3 ci/check-privacy.py --staged
python3 ci/check-privacy.py --tree HEAD
```

Checks cover multiline private keys, selected credential token formats,
embedded CI credentials, literal home directories, literal SSH IPv4 endpoints,
local credential filenames, generated mobile build output, and release archives
or app bundles. Use `alice`/`example` and documentation IPs such as `192.0.2.10`
for fixtures. Existing public upstream examples and specific packaging template
files have narrow exceptions in the scanner; do not add blanket exclusions for
tests or source folders.

The `privacy` Actions workflow checks the committed snapshot on pushes and pull
requests without running a Rust/mobile build. Once installed on GitHub, its
`check` job can be required in repository branch rules. CI is **after upload**;
it cannot prevent a leaked commit from reaching GitHub. Local hooks can also be
bypassed. This guard does not scan every historical commit, recurse into
submodules, OCR images, inspect archives, or recognize every possible secret.
Keep reviewing `git diff --cached` and any newly added files before committing.

To uninstall locally, remove the managed `pre-commit` wrapper and restore
`pre-commit.before-privacy-guard` to `pre-commit` if that backup exists. Keep the
original hook; do not delete it as part of cleanup.
