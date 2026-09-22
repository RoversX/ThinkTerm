"""Exercise real Git indexes and hook chaining without touching the caller's repo."""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

CI = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("privacy_guard", CI / "check-privacy.py")
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


def personal_path():
    # Build synthetic rejected data at runtime; do not commit a real identity.
    return b"/Users/" + b"synthetic-private-person" + b"/project"


class Rules(unittest.TestCase):
    def test_hostname_literals_and_source_expressions(self):
        private = b"synthetic-private-machine"
        for suffix in (b".local", b".LAN", b".home.arpa"):
            for prefix in (b'"', b"'", b"`", b"user@", b"https://"):
                self.assertTrue(GUARD.content_issues("test.rs", prefix + private + suffix + b'"'))
        for data in (b'"myhost.local"', b'"example.lan"', b"entry" + b".local", b'"entry.local_name"'):
            self.assertFalse(GUARD.content_issues("test.rs", data))
        label = b'"Dockerfile' + b'.local"'
        self.assertFalse(GUARD.content_issues("third_party/material-icon-theme/icons.json", label))
        self.assertTrue(GUARD.content_issues("test.rs", label))

    def test_personal_names_are_literal_bounded_and_independent_of_exemptions(self):
        name = b"synthetic-private-machine"
        pattern = GUARD.personal_pattern([name, b"test[42]"])
        self.assertTrue(GUARD.content_issues("test.rs", name.upper(), pattern))
        self.assertTrue(GUARD.content_issues("test.rs", b"test[42]", pattern))
        self.assertFalse(GUARD.content_issues("test.rs", b"test4", pattern))
        self.assertFalse(GUARD.content_issues("test.rs", b"prefix-" + name, pattern))
        self.assertTrue(GUARD.content_issues("test.rs", b'"myhost.local"', GUARD.personal_pattern([b"myhost"])))

    def test_ci_does_not_read_machine_names_or_local_list(self):
        with mock.patch.dict(os.environ, {"CI": "true"}), mock.patch.object(GUARD.socket, "gethostname") as hostname, mock.patch("builtins.open") as opened:
            self.assertEqual(GUARD.local_names(), ())
            hostname.assert_not_called()
            opened.assert_not_called()

    def test_native_and_escaped_windows_paths(self):
        for data in (personal_path(), b"C:\\Users\\" + b"synthetic-person\\work", b"C:\\\\Users\\\\" + b"synthetic-person\\\\work"):
            self.assertTrue(GUARD.content_issues("example.rs", data))
        self.assertFalse(GUARD.content_issues("example.rs", b"/Users/alice/project /home/example/app"))

    def test_keys_and_runtime_header_strings(self):
        header = b"-----BEGIN " + b"OPENSSH PRIVATE KEY-----"
        self.assertTrue(GUARD.content_issues("key.txt", header + b"\n" + b"A" * 64 + b"\n"))
        self.assertTrue(GUARD.content_issues("key.json", header + b"\\n" + b"A" * 64 + b"\\n"))
        self.assertTrue(GUARD.content_issues("key.pem", header + b"\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-256-CBC,example\n\n" + b"A" * 64 + b"\n"))
        self.assertFalse(GUARD.content_issues("key.rs", b'let header = "' + header + b'\\n";'))

    def test_token_and_ci_credentials(self):
        for data in (b"ghp" + b"_" + b"A" * 36, b"ENCRYPTED" + b"[fake-ciphertext]"):
            self.assertTrue(GUARD.content_issues("settings.txt", data))

    def test_endpoint_examples(self):
        prefix = b"ssh://alice@"
        self.assertTrue(GUARD.content_issues("test.rs", prefix + b"10.8.9.10"))
        for ip in (b"192.0.2.10", b"198.51.100.8", b"203.0.113.1", b"127.0.0.1"):
            self.assertFalse(GUARD.content_issues("test.rs", prefix + ip))

    def test_packaging_template_is_not_a_blanket_exemption(self):
        self.assertFalse(GUARD.path_issues("assets/macos/ThinkTerm.app/Contents/Info.plist"))
        self.assertTrue(GUARD.path_issues("assets/macos/ThinkTerm.app/Contents/MacOS/thinkterm"))
        self.assertTrue(GUARD.path_issues("release/My App.app/Contents/MacOS/app"))
        self.assertTrue(GUARD.path_issues("release/output.zip"))
        self.assertTrue(GUARD.path_issues("ios/build/XCBuildData/manifest.json"))

    def test_upstream_exception_is_scoped(self):
        data = b"/home/" + b"wez/code"
        self.assertFalse(GUARD.content_issues("docs/config/fonts.md", data))
        self.assertTrue(GUARD.content_issues("new-test.rs", data))

    def test_credential_filenames_and_templates(self):
        for path in (".env", ".env.local", "keys/id_ed25519", "signing.p12", "android/local.properties"):
            self.assertTrue(GUARD.path_issues(path))
        self.assertFalse(GUARD.path_issues(".env.example"))
        self.assertFalse(GUARD.path_issues("keys/id_ed25519.pub"))


class GitIntegration(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
        self.git("init", "-q")
        self.git("config", "user.name", "Privacy tests")
        self.git("config", "user.email", "tests@example.invalid")

    def tearDown(self):
        self.temp.cleanup()

    def run_command(self, args):
        return subprocess.run(args, cwd=self.repo, env=self.env, capture_output=True)

    def git(self, *args):
        result = self.run_command(["git", *args])
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        return result.stdout

    def scan(self, *args):
        return self.run_command([sys.executable, str(CI / "check-privacy.py"), *args])

    def test_local_list_is_used_without_disclosing_or_staging_it(self):
        self.env.pop("CI", None)
        local = self.repo / GUARD.LOCAL_LIST
        local.parent.mkdir()
        name = b"synthetic-private-machine"
        local.write_bytes(name + b" # fixture\n")
        (self.repo / "test.txt").write_bytes(name.upper())
        self.git("add", "test.txt")
        result = self.scan()
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"personal-machine", result.stderr)
        self.assertNotIn(name.lower(), (result.stdout + result.stderr).lower())
        self.env["CI"] = "true"
        self.assertEqual(self.scan().returncode, 0)
        self.git("add", GUARD.LOCAL_LIST)
        self.assertIn(b"personal-host-list", self.scan().stderr)

    def test_hostname_check_reads_index_and_redacts_matches(self):
        self.env["CI"] = "true"
        name = b"synthetic-private-machine" + b".local"
        path = self.repo / "test.txt"
        path.write_bytes(b'"' + name + b'"')
        self.git("add", "test.txt")
        path.write_text('"myhost.local"')
        result = self.scan()
        self.assertEqual(result.returncode, 1)
        self.assertNotIn(name, result.stdout + result.stderr)
        self.git("add", "test.txt")
        self.assertEqual(self.scan().returncode, 0)

    def test_scans_staged_bytes_and_redacts_values(self):
        path = self.repo / "settings.txt"
        secret = b"ghp" + b"_" + b"A" * 36
        path.write_bytes(secret + b"\n" + personal_path())
        self.git("add", "settings.txt")
        path.write_text("clean working file\n")
        result = self.scan("--staged")
        self.assertEqual(result.returncode, 1)
        self.assertNotIn(secret, result.stdout + result.stderr)
        self.assertNotIn(personal_path(), result.stdout + result.stderr)
        self.assertIn(b"settings.txt", result.stderr)
        self.git("add", "settings.txt")
        self.assertEqual(self.scan().returncode, 0)

    def test_unstaged_and_untracked_data_are_not_uploaded(self):
        path = self.repo / "settings.txt"
        path.write_text("safe\n")
        self.git("add", "settings.txt")
        path.write_bytes(personal_path())
        (self.repo / "probe_key").write_bytes(personal_path())
        self.assertEqual(self.scan().returncode, 0)

    def test_force_added_artifact_is_blocked_then_deletion_passes(self):
        (self.repo / ".gitignore").write_text("*.zip\n")
        (self.repo / "release file.zip").write_bytes(b"PK\0dummy")
        self.git("add", ".gitignore")
        self.git("add", "-f", "release file.zip")
        self.assertEqual(self.scan().returncode, 1)
        self.git("rm", "--cached", "release file.zip")
        self.assertEqual(self.scan().returncode, 0)

    def test_tree_mode_ignores_working_changes(self):
        (self.repo / "test.txt").write_text("safe\n")
        self.git("add", "test.txt")
        self.git("commit", "-qm", "Add safe fixture")
        (self.repo / "test.txt").write_bytes(personal_path())
        self.git("add", "test.txt")
        self.assertEqual(self.scan("--tree", "HEAD").returncode, 0)
        self.assertEqual(self.scan("--staged").returncode, 1)

    def test_running_from_subdirectory_still_checks_root(self):
        (self.repo / "settings.txt").write_bytes(personal_path())
        self.git("add", "settings.txt")
        nested = self.repo / "nested"
        nested.mkdir()
        result = subprocess.run([sys.executable, str(CI / "check-privacy.py"), "--staged"], cwd=nested, env=self.env, capture_output=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"settings.txt", result.stderr)

    def test_installer_preserves_hook_and_works_on_old_branches(self):
        hooks = self.repo / ".git" / "hooks"
        hook = hooks / "pre-commit"
        original = '#!/bin/sh\nprintf chained > hook-ran\nexit 9\n'
        hook.write_text(original)
        hook.chmod(0o755)
        install = [sys.executable, str(CI / "install-privacy-hook.py")]
        self.assertEqual(self.run_command(install).returncode, 0)
        self.assertEqual(self.run_command(install).returncode, 0)
        self.assertEqual((hooks / "pre-commit.before-privacy-guard").read_text(), original)
        # There is no ci/ directory in this throwaway repo: use the installed copy.
        result = self.run_command([str(hook)])
        self.assertEqual(result.returncode, 9)
        self.assertEqual((self.repo / "hook-ran").read_text(), "chained")
        (self.repo / "hook-ran").unlink()
        (self.repo / "settings.txt").write_bytes(personal_path())
        self.git("add", "settings.txt")
        self.assertEqual(self.run_command([str(hook)]).returncode, 1)
        self.assertFalse((self.repo / "hook-ran").exists())


if __name__ == "__main__":
    unittest.main()
