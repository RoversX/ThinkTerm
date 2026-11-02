# Remote server updates

The SSH update/install flow prefers an online `install.sh` signed with the
pinned release key in the separate `thinkterm-installer-v1` SSHSIG namespace.
The client fetches it from the `installer-v1` prerelease assets using the
configured sources, without sending tokens. All requests, redirects, sources,
and the detached signature share a four-second budget; each HTTP request is
limited to 1.5 seconds or the remaining budget, whichever is smaller. Missing
curl, network failure, invalid signatures, incompatible protocol headers, or
an exhausted budget select the bundled script. Nothing unverified is executed.
There is no persistent script cache.

The selected script and trusted release metadata travel over authenticated
SSH. The server downloads the archive itself. The client does not download or
upload the archive on its behalf. Already released clients need one upgrade
to gain this selection flow; later script fixes do not require a client build.

The `publish installer` workflow tests, signs, and publishes `install.sh` and
`install.sh.sig` when the script changes on main, or when dispatched on main.
It uses the existing `THINKTERM_RELEASE_SIGNING_KEY` secret and does not build
application packages. The channel remains a prerelease so it is not offered
as the latest application release. Updating only an unsigned website copy
cannot update this signed channel: the signing workflow must succeed.
During publication, a temporarily mismatched script/signature pair safely
falls back to the bundled copy. Mirror copies may lag the origin.

The v1 script must preserve `--release-info`, `--version`, `--server`,
`--desktop`, `--mirror`, and noninteractive SSH behavior. Its first two lines
are `#!/bin/sh` and `# thinkterm-installer-v1`. Breaking that contract requires
a new channel and namespace; old clients continue using v1 or their fallback.

The client first asks the GitHub releases API for its exact release version.
If that fails, it tries `thinkterm-release.txt` and its detached `.sig` file,
using the configured download sources. A pinned Ed25519 public key verifies
the SSHSIG signature in the `thinkterm-release` namespace, and the inventory
must name the exact requested version. The desktop application's own updater
continues to use its existing lookup and installation flow.

The standalone installer also supports signed metadata when an explicit
`--version` lookup fails. It needs `ssh-keygen` with `-Y verify` to verify it.
If no verifier or no signed inventory is available for an older release, it
uses GitHub directly with a warning; it never authorizes a mirror without a
trusted SHA-256. A downloaded inventory with an invalid signature or wrong
version is rejected. Resolving an unspecified latest version still requires
the GitHub API.

With automatic source selection, the server probes GitHub for up to three
seconds. If it is slow, it probes the known mirrors concurrently and ranks
them by observed throughput, including connection setup. Probes are limited
in time and size. If all sources fail, sources that delivered bytes before
the speed cutoff get a second pass in rank order, allowing slow downloads
while still detecting stalled transfers and verifying the archive.
Transfers retry interrupted downloads using HTTP ranges;
a source that refuses resume gets a fresh attempt. Partials are never reused
after a checksum failure or across sources. Temporary files are removed when
the installer exits, so resume applies within an invocation, not across
separate installer runs. Explicit direct/mirror choices retain their meaning.

Linux server installs prefer the `.tar.xz` archive when both the release and
the host support it, otherwise they use `.tar.gz`. SSH output includes a static
wordmark and periodic byte counts, percentage, and rate without terminal
redrawing. Installing and restarting remain separate decisions in the client.

## Release signing

The repository's Actions secret `THINKTERM_RELEASE_SIGNING_KEY` contains the
complete, unencrypted OpenSSH Ed25519 private key dedicated to release signing.
The public half is pinned in `assets/release-signing.pub` and `install.sh`.
Never put the private half in the repository. Tests generate disposable keys
inside temporary directories; they do not use the release key or ssh-agent.

The publish job collects both this run's packages and existing assets from a
partial rerun, then `ci/sign-release.sh` generates and signs a canonical
inventory. It verifies the result against the pinned public key before any
metadata is uploaded. A missing secret or mismatched key fails publishing.
Archives are uploaded before the inventory/signature pair. Symbol bundles
are excluded. The workflow does not change existing releases unless invoked
for that release with publishing enabled.

The inventory format is UTF-8 with LF endings:

```text
thinkterm-release-v1
version 9.9.9
<64 lowercase SHA-256 hex digits> <byte count> <asset filename>
```

Rows are sorted by filename. Filenames cannot contain paths or whitespace,
duplicates are rejected, and metadata is bounded to 1 MiB. The client-to-server
copy also permits `-` in place of an absent API checksum; this never authorizes
a mirror and is not accepted in signed inventories.

Replacing a package after signing requires regenerating the inventory and
signature. This includes manually uploaded macOS packages. Rerunning the
publish workflow includes existing assets for platforms not rebuilt in that
run. Older releases need separately prepared inventories for the fallback to
work; this change does not backfill them automatically. Already released
clients retain their original API/raw-script dependencies until upgraded.

Keep the signing secret and pinned public key in sync when rotating keys, and
publish clients that trust the replacement key. A new key by itself cannot
make old clients trust new signatures. Merge workflow changes before running
the project's release process.

## Verification

```sh
python3 ci/test-remote-update.py -v
cargo test -p thinkterm-update -p wezterm-client --no-fail-fast
cargo test --no-fail-fast --workspace --exclude wezterm-ssh
python3 ci/check-privacy.py
```

The offline installer suite replaces network commands and platform probes,
uses isolated HOME/prefix/temp directories, and covers signing interoperability,
metadata fallback, wrong signatures/versions, fake mirror packages, source
selection, missing xz/verifier, resumed transfers, and non-TTY progress.
Real Linux/OpenSSH backends, GUI presentation, actual mainland connectivity,
and the GitHub signing-secret configuration still need integration validation.
