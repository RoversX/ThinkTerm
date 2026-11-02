#!/usr/bin/env python3
"""Offline installer/CI integration tests; all writes use isolated temp dirs."""

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parent.parent
TAG = "9.9.9"
GZIP = f"thinkterm-server-{TAG}-linux-x86_64.tar.gz"
XZ = GZIP[:-2] + "xz"

FAKE_CURL = r'''
import json, os, sys, time
from pathlib import Path
from urllib.parse import urlparse
args = sys.argv[1:]
root = Path(os.environ["TEST_ROOT"])
config = json.loads((root / "network.json").read_text())
url = args[-1]
host = urlparse(url).hostname
name = url.rsplit("/", 1)[-1]
dest = Path(args[args.index("-o") + 1])
speed_limit = int(args[args.index("--speed-limit") + 1]) if "--speed-limit" in args else 0
with (root / "requests" / f"{time.time_ns()}-{os.getpid()}").open("w") as log:
    log.write(json.dumps({"host": host, "name": name, "probe": "--range" in args,
                          "resume": "-C" in args, "speed_limit": speed_limit,
                          "auth": any("Authorization:" in x for x in args)}) + "\n")
if host == "api.github.com":
    if config.get("api_failure", False):
        print("403", end="")
        sys.exit(22)
    dest.write_bytes((root / "release.json").read_bytes())
    print("200", end="")
    sys.exit(0)
if name in ("thinkterm-release.txt", "thinkterm-release.txt.sig"):
    if config.get("no_metadata", False) or (config.get("metadata_mirror_only") and host == "github.com"):
        sys.exit(22)
    dest.write_bytes((root / name).read_bytes())
    sys.exit(0)
if "--range" in args:
    rate = config.get("rates", {}).get(host, 10)
    dest.write_bytes(b"probe")
    print("206", rate, "1")
    sys.exit(0)
if host in config.get("fail_hosts", []):
    sys.exit(22)
if host in config.get("timeout_hosts", []):
    sys.exit(28)
data = (root / name).read_bytes()
if host in config.get("bad_hosts", []):
    data = b"tampered archive"
counter = root / ("count-" + host)
attempt = int(counter.read_text()) if counter.exists() else 0
counter.write_text(str(attempt + 1))
if speed_limit > config.get("transfer_rates", {}).get(host, speed_limit):
    dest.write_bytes(data[:20])
    sys.exit(28)
if config.get("resume") and attempt == 0:
    dest.write_bytes(data[:20])
    sys.exit(18)
if config.get("no_range") and attempt == 1 and dest.exists() and dest.stat().st_size:
    sys.exit(33)
if config.get("slow"):
    dest.write_bytes(data[:20])
    time.sleep(2.4)
if "-C" in args and dest.exists():
    offset = dest.stat().st_size
    assert dest.read_bytes() == data[:offset], "resume prefix corrupted"
    with dest.open("ab") as output:
        output.write(data[offset:])
else:
    dest.write_bytes(data)
'''


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="thinkterm-update-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "tools"
        self.bin.mkdir()
        self.home = self.root / "home"
        self.home.mkdir()
        (self.root / "tmp").mkdir()
        (self.root / "requests").mkdir()
        for name in ("awk", "basename", "cat", "chmod", "cp", "cut", "date", "dirname", "find",
                     "grep", "gzip", "head", "id", "ln", "mkdir", "mktemp", "mv", "rm", "sed", "sh",
                     "sort", "stty", "tar", "tr", "wc", "xz", "sha256sum", "shasum", "ssh-keygen", "sleep", "ditto"):
            tool = shutil.which(name)
            if tool:
                (self.bin / name).symlink_to(tool)
        self.tool("uname", '#!/bin/sh\ncase "$1" in -s) echo "${TEST_OS:-Linux}";; *) echo x86_64;; esac\n')
        self.tool("getconf", '#!/bin/sh\necho "glibc ${TEST_GLIBC:-2.35}"\n')
        self.tool("pgrep", "#!/bin/sh\nexit 1\n")
        self.tool("curl", f"#!{sys.executable}\n" + FAKE_CURL)
        self.script = self.root / "install.sh"
        self.script.write_bytes((ROOT / "install.sh").read_bytes())
        self.env = dict(os.environ, HOME=str(self.home), TMPDIR=str(self.root / "tmp"),
                        PATH=str(self.bin), TEST_ROOT=str(self.root), TERM="dumb",
                        THINKTERM_INSTALL_PROGRESS="1")
        for name in ("GITHUB_TOKEN", "THINKTERM_UPDATE_TOKEN", "BASH_ENV", "ENV"):
            self.env.pop(name, None)
        self.network = {}
        self.packages = []
        self.package(GZIP)
        self.write_api()
        self.key = None

    def tool(self, name, text):
        path = self.bin / name
        if path.is_symlink():
            path.unlink()
        path.write_text(text)
        path.chmod(0o755)

    def package(self, name):
        path = self.root / name
        with tarfile.open(path, "w:xz" if name.endswith(".xz") else "w:gz") as archive:
            entry = tarfile.TarInfo("thinkterm-example/bin/thinkterm")
            entry.mode = 0o755
            data = b"#!/bin/sh\nexit 0\n"
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
        self.packages.append(name)
        return path

    def write_api(self, missing_digest=False):
        assets = []
        for name in self.packages:
            data = (self.root / name).read_bytes()
            assets.append(dict(name=name, size=len(data),
                               digest=None if missing_digest else "sha256:" + hashlib.sha256(data).hexdigest()))
        (self.root / "release.json").write_text(json.dumps(dict(tag_name=TAG, assets=assets), indent=2))

    def sign_metadata(self, tag=TAG):
        keygen = shutil.which("ssh-keygen")
        self.assertIsNotNone(keygen)
        self.key = self.root / "test-key"
        subprocess.run([keygen, "-q", "-t", "ed25519", "-N", "", "-C", "example", "-f", str(self.key)], check=True)
        public = self.key.with_suffix(".pub").read_text().strip()
        pinned = (ROOT / "assets/release-signing.pub").read_text().strip()
        self.script.write_text(self.script.read_text().replace(pinned, public))
        rows = ["thinkterm-release-v1", f"version {tag}"]
        for name in self.packages:
            data = (self.root / name).read_bytes()
            rows.append(f"{hashlib.sha256(data).hexdigest()} {len(data)} {name}")
        info = self.root / "thinkterm-release.txt"
        info.write_text("\n".join(rows) + "\n")
        subprocess.run([keygen, "-Y", "sign", "-f", str(self.key), "-n", "thinkterm-release", str(info)],
                       check=True, capture_output=True)

    def run_install(self, *args, success=True):
        (self.root / "network.json").write_text(json.dumps(self.network))
        result = subprocess.run(["/bin/sh", str(self.script), "--server", "--version", TAG, *args],
                                env=self.env, capture_output=True, text=True, timeout=35)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse((self.home / ".local/bin/thinkterm").exists())
        self.assertEqual(list((self.root / "tmp").iterdir()), [], "temporary files leaked")
        return result.stdout + result.stderr

    def requests(self):
        path = self.root / "requests"
        return [json.loads(entry.read_text()) for entry in sorted(path.iterdir())]

    def test_fast_github_never_contacts_mirrors(self):
        self.network["rates"] = {"github.com": 900000}
        output = self.run_install()
        self.assertIn("█", output)
        self.assertNotIn("\x1b", output)
        self.assertTrue(all(r["host"] in ("api.github.com", "github.com") for r in self.requests()))

    def test_slow_github_ranks_mirrors_and_rejects_fake_package(self):
        self.network.update(rates={"ghfast.top": 900000, "gh-proxy.org": 800000}, bad_hosts=["ghfast.top"])
        output = self.run_install()
        self.assertIn("mismatch", output)
        self.assertIn("from gh-proxy.org", output)
        self.assertEqual(len([r for r in self.requests() if r["probe"]]), 5)

    def slow_mirrors(self):
        mirrors = [f"mirror-{name}.example" for name in ("a", "b", "c", "d")]
        self.script.write_text("\n".join(
            f'mirrors="{" ".join(mirrors)}"' if line.startswith("mirrors=") else line
            for line in self.script.read_text().splitlines()) + "\n")
        self.network.update(rates={host: 900000 - n * 100000 for n, host in enumerate(mirrors)},
                            transfer_rates={host: 131072 for host in mirrors},
                            fail_hosts=["github.com"])
        return mirrors

    def test_fast_probes_slow_downloads_retry_fastest_mirror(self):
        mirrors = self.slow_mirrors()
        self.run_install()
        downloads = [r for r in self.requests() if r["name"] == GZIP and not r["probe"]]
        self.assertEqual([r["host"] for r in downloads], mirrors + ["github.com", mirrors[0]])
        self.assertEqual(downloads[-1]["speed_limit"], 1)
        self.assertTrue((self.home / ".local/bin/thinkterm").exists())

    def test_slow_fallback_still_rejects_corrupt_mirror(self):
        mirrors = self.slow_mirrors()
        self.network["bad_hosts"] = [mirrors[0]]
        output = self.run_install()
        self.assertIn("mismatch", output)
        downloads = [r for r in self.requests() if r["name"] == GZIP and not r["probe"]]
        self.assertEqual([r["host"] for r in downloads], mirrors + ["github.com", *mirrors[:2]])
        self.assertEqual([r["speed_limit"] for r in downloads[-2:]], [1, 1])

    def test_http_failures_do_not_get_slow_download_retry(self):
        mirrors = self.slow_mirrors()
        self.network["fail_hosts"].extend(mirrors)
        self.run_install(success=False)
        downloads = [r for r in self.requests() if r["name"] == GZIP and not r["probe"]]
        self.assertEqual([r["host"] for r in downloads], mirrors + ["github.com"])

    def test_connection_timeouts_do_not_get_slow_download_retry(self):
        mirrors = self.slow_mirrors()
        self.network["timeout_hosts"] = mirrors
        self.run_install(success=False)
        downloads = [r for r in self.requests() if r["name"] == GZIP and not r["probe"]]
        self.assertEqual([r["host"] for r in downloads], mirrors + ["github.com"])

    def test_explicit_mirror_does_not_add_slow_fallback(self):
        mirrors = self.slow_mirrors()
        self.run_install("--mirror", mirrors[0], success=False)
        downloads = [r for r in self.requests() if r["name"] == GZIP]
        self.assertEqual([r["host"] for r in downloads], [mirrors[0], "github.com"])
        self.assertFalse(any(r["probe"] for r in downloads))

    def test_signed_metadata_fallback_and_xz(self):
        self.package(XZ)
        self.sign_metadata()
        self.network.update(api_failure=True, metadata_mirror_only=True)
        self.run_install()
        self.assertTrue(any(r["name"] == XZ for r in self.requests()))

    def test_old_openssh_uses_github_only(self):
        self.sign_metadata()
        self.tool("ssh-keygen", "#!/bin/sh\necho 'unknown option -- Y' >&2\nexit 1\n")
        self.network["api_failure"] = True
        output = self.run_install()
        self.assertIn("does not support", output)
        self.assertTrue(all(r["host"] in ("api.github.com", "github.com") for r in self.requests()))

    def test_no_verifier_and_no_xz_keep_github_gzip(self):
        for tool in ("ssh-keygen", "xz"):
            (self.bin / tool).unlink(missing_ok=True)
        self.network["api_failure"] = True
        self.run_install()
        self.assertTrue(all(r["host"] in ("api.github.com", "github.com") for r in self.requests()))
        self.assertTrue(any(r["name"] == GZIP for r in self.requests()))

    def test_xz_absent_chooses_gzip_even_when_release_has_xz(self):
        self.package(XZ)
        self.write_api()
        (self.bin / "xz").unlink(missing_ok=True)
        self.network["rates"] = {"github.com": 900000}
        self.run_install()
        self.assertFalse(any(r["name"] == XZ for r in self.requests()))

    def test_tampered_metadata_is_rejected(self):
        self.sign_metadata()
        info = self.root / "thinkterm-release.txt"
        info.write_bytes(info.read_bytes() + b"tampered\n")
        self.network["api_failure"] = True
        self.assertIn("verification failed", self.run_install(success=False))

    def test_signed_wrong_version_is_rejected(self):
        self.sign_metadata(tag="9.9.8")
        self.network["api_failure"] = True
        self.assertIn("different version", self.run_install(success=False))

    def test_tampered_signature_is_rejected(self):
        self.sign_metadata()
        (self.root / "thinkterm-release.txt.sig").write_text("invalid signature\n")
        self.network["api_failure"] = True
        self.assertIn("verification failed", self.run_install(success=False))

    def test_missing_old_release_metadata_falls_back_without_mirrors(self):
        self.network.update(api_failure=True, no_metadata=True)
        output = self.run_install()
        self.assertIn("without mirrors", output)
        archives = [r for r in self.requests() if r["name"] == GZIP]
        self.assertTrue(all(r["host"] == "github.com" for r in archives))

    def test_glibc_floor_uses_compatible_package(self):
        name = GZIP.replace(".tar.gz", "-glibc2.31.tar.gz")
        self.package(name)
        self.write_api()
        self.env["TEST_GLIBC"] = "2.31"
        self.network["rates"] = {"github.com": 900000}
        self.run_install()
        self.assertTrue(any(r["name"] == name for r in self.requests()))

    @unittest.skipUnless(sys.platform == "darwin", "macOS ditto required")
    def test_macos_server_from_install_stays_inside_isolated_home(self):
        self.env["TEST_OS"] = "Darwin"
        archive = self.root / f"ThinkTerm-macos-x86_64-{TAG}.zip"
        with zipfile.ZipFile(archive, "w") as output:
            entry = zipfile.ZipInfo("ThinkTerm.app/Contents/MacOS/thinkterm")
            entry.create_system = 3
            entry.external_attr = 0o100755 << 16
            output.writestr(entry, b"#!/bin/sh\nexit 0\n")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.run_install("--from", str(archive), "--sha256", digest)
        self.assertTrue((self.home / ".local/libexec/ThinkTerm.app").is_dir())
        self.assertTrue((self.home / ".local/bin/thinkterm").is_symlink())

    def test_missing_digest_never_uses_mirror(self):
        self.write_api(missing_digest=True)
        self.run_install()
        self.assertTrue(all(r["host"] in ("api.github.com", "github.com") for r in self.requests()))

    def test_broken_transfer_resumes(self):
        self.network["resume"] = True
        self.run_install("--mirror", "direct")
        downloads = [r for r in self.requests() if r["name"] == GZIP]
        self.assertEqual(len(downloads), 2)
        self.assertTrue(all(r["resume"] for r in downloads))

    def test_range_refusal_restarts_cleanly(self):
        self.network.update(resume=True, no_range=True)
        self.run_install("--mirror", "direct")
        self.assertEqual(len([r for r in self.requests() if r["name"] == GZIP]), 3)

    def test_non_tty_progress_reports_speed(self):
        self.network["slow"] = True
        output = self.run_install("--mirror", "direct")
        self.assertIn("KB/s", output)
        self.assertIn("% of", output)

    def test_from_checksum_passes_and_rejects_corruption(self):
        data = (self.root / GZIP).read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        self.run_install("--from", str(self.root / GZIP), "--sha256", "0" * 64, success=False)
        self.run_install("--from", str(self.root / GZIP), "--sha256", digest)
        self.assertEqual(self.requests(), [])

    def test_trusted_client_metadata_skips_api_and_checks_version(self):
        self.sign_metadata()
        info = self.root / "thinkterm-release.txt"
        self.network["rates"] = {"github.com": 900000}
        self.run_install("--release-info", str(info))
        self.assertFalse(any(r["host"] == "api.github.com" for r in self.requests()))

    def test_installer_signer_uses_separate_namespace_and_pinned_key(self):
        self.sign_metadata()
        repo = self.root / "installer-ci"
        (repo / "assets").mkdir(parents=True)
        shutil.copyfile(self.key.with_suffix(".pub"), repo / "assets/release-signing.pub")
        shutil.copyfile(ROOT / "install.sh", repo / "install.sh")
        env = dict(os.environ, THINKTERM_RELEASE_SIGNING_KEY=self.key.read_text())
        command = ["bash", str(ROOT / "ci/sign-installer.sh"), "out"]
        result = subprocess.run(command, cwd=repo, env=env, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((repo / "out/install.sh").read_bytes(), (ROOT / "install.sh").read_bytes())
        signers = repo / "signers"
        signers.write_text('example namespaces="thinkterm-installer-v1" ' + self.key.with_suffix(".pub").read_text())
        result = subprocess.run([shutil.which("ssh-keygen"), "-Y", "verify", "-f", str(signers),
                                 "-I", "example", "-n", "thinkterm-installer-v1", "-s", "out/install.sh.sig"],
                                cwd=repo, input=(repo / "out/install.sh").read_bytes(), capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        shutil.copyfile(ROOT / "assets/release-signing.pub", repo / "assets/release-signing.pub")
        self.assertNotEqual(subprocess.run(command, cwd=repo, env=env, capture_output=True).returncode, 0)

    def test_ci_signer_merges_existing_packages_and_checks_its_key(self):
        self.sign_metadata()
        repo = self.root / "ci-work"
        (repo / "ci").mkdir(parents=True)
        (repo / "assets").mkdir()
        for name in ("sign-release.sh", "release-manifest.py"):
            shutil.copyfile(ROOT / "ci" / name, repo / "ci" / name)
        shutil.copyfile(self.key.with_suffix(".pub"), repo / "assets/release-signing.pub")
        dist, previous = repo / "dist", repo / "previous"
        dist.mkdir(); previous.mkdir()
        (dist / "example-new.tar.gz").write_bytes(b"new package")
        (previous / "example-old.tar.gz").write_bytes(b"old package")
        (previous / "example-new.tar.gz").write_bytes(b"replaced package")
        env = dict(os.environ, THINKTERM_RELEASE_SIGNING_KEY=self.key.read_text())
        result = subprocess.run(["bash", "ci/sign-release.sh", TAG, "dist", "previous"],
                                cwd=repo, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        info = (dist / "thinkterm-release.txt").read_text()
        self.assertIn("example-old.tar.gz", info)
        self.assertIn(hashlib.sha256(b"new package").hexdigest(), info)
        self.assertNotIn(hashlib.sha256(b"replaced package").hexdigest(), info)
        shutil.copyfile(ROOT / "assets/release-signing.pub", repo / "assets/release-signing.pub")
        result = subprocess.run(["bash", "ci/sign-release.sh", TAG, "dist", "previous"],
                                cwd=repo, env=env, capture_output=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
