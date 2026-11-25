"""Exercise real Git indexes and hook chaining without touching the caller's repo."""

import codecs
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


def tailnet_key():
    return b"tskey-auth-" + b"sampleID123-" + b"Z" * 32


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

    def test_local_list_permission_error_is_reported(self):
        with mock.patch.object(GUARD, "git", side_effect=(b"true\n", b".\n")), mock.patch("builtins.open", side_effect=PermissionError):
            with self.assertRaises(GUARD.LocalListError):
                list(GUARD.local_list_names())

    def test_native_and_escaped_windows_paths(self):
        for data in (personal_path(), b"C:\\Users\\" + b"synthetic-person\\work", b"C:\\\\Users\\\\" + b"synthetic-person\\\\work"):
            self.assertTrue(GUARD.content_issues("example.rs", data))
        self.assertFalse(GUARD.content_issues("example.rs", b"/Users/alice/project /home/example/app"))

    def test_home_paths_written_with_dashes(self):
        # A folder named after a path, as coding agents name theirs.
        person = b"synthetic-person"
        for data in (
            b"/tmp/agent/-Users-" + person + b"-src-app/notes",
            b'"C--Users-' + person + b'-work"',
            b"-home-" + person + b"-project",
        ):
            self.assertTrue(GUARD.content_issues("example.rs", data), data)
        for data in (b"/tmp/-Users-alice-project", b"-home-example-app", b"nav-home-link", b"smart-home-hub", b"--home-dir", b"Manage-Users-Page"):
            self.assertFalse(GUARD.content_issues("example.rs", data), data)

    def test_keys_and_runtime_header_strings(self):
        header = b"-----BEGIN " + b"OPENSSH PRIVATE KEY-----"
        self.assertTrue(GUARD.content_issues("key.txt", header + b"\n" + b"A" * 64 + b"\n"))
        self.assertTrue(GUARD.content_issues("key.json", header + b"\\n" + b"A" * 64 + b"\\n"))
        self.assertTrue(GUARD.content_issues("key.pem", header + b"\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-256-CBC,example\n\n" + b"A" * 64 + b"\n"))
        self.assertFalse(GUARD.content_issues("key.rs", b'let header = "' + header + b'\\n";'))

    def test_token_and_ci_credentials(self):
        for data in (b"ghp" + b"_" + b"A" * 36, b"github_pat_" + b"A" * 64, b"ENCRYPTED" + b"[fake-ciphertext]"):
            self.assertTrue(GUARD.content_issues("settings.txt", data))

    def test_tailnet_auth_key_and_placeholders(self):
        self.assertIn("provider-token: remove the credential", GUARD.content_issues("env.txt", tailnet_key()))
        for sample in (b"tskey-example", b"tskey-auth-example", "tskey-auth-…".encode()):
            self.assertFalse(GUARD.content_issues("env.txt", sample))

    def test_endpoint_examples(self):
        prefix = b"ssh://alice@"
        self.assertTrue(GUARD.content_issues("test.rs", prefix + b"10.8.9.10"))
        for ip in (b"192.0.2.10", b"198.51.100.8", b"203.0.113.1", b"127.0.0.1"):
            self.assertFalse(GUARD.content_issues("test.rs", prefix + ip))

    def test_ssh_destinations_and_configuration(self):
        ipv4 = b"10.8.9.10"
        ipv6 = b"fd00::42"
        for data in (
            b"ssh alice@" + ipv4, b"ssh " + ipv6, b"ssh://[" + ipv6 + b"]:2222/path",
            b"ssh -vp 2222 -i 'sample key' -- \"alice@[" + ipv6 + b"]\" uptime",
            rb"ssh -i C:\keys\sample alice@" + ipv4, b"ssh alice@" + ipv6 + b"%eth0",
            b"ssh -p2222 alice@" + ipv6, b"ssh://" + ipv4 + b":22",
            b"Host lab\n\tHostName " + ipv4 + b"\r\n", b'hOsTnAmE = "' + ipv6 + b'" # fixture',
        ):
            with self.subTest(data=data):
                self.assertIn("ssh-endpoint: use a documentation IP in examples", GUARD.content_issues("settings", data))
        for prefix in (b"ssh alice@", b"ssh://", b"HostName="):
            for host in (b"node.team" + b".lan", b"node" + b".home.arpa", b"node" + b".internal"):
                self.assertIn("personal-hostname: use an example host in fixtures", GUARD.content_issues("settings", prefix + host))
        for data in (
            b"ssh alice@buildbox", b"HostName myhost.local", b"ssh -p 22 alice@192.0.2.5",
            b"ssh://[2001:db8::42]:22", b"HostName ::1", b"ssh alice@[::ffff:127.0.0.1]",
            b"ssh jump echo " + ipv4, b"outside SSH: alice@" + ipv4, b"not-ssh alice@" + ipv4,
            b'hostname = "' + ipv4 + b'",',
        ):
            with self.subTest(data=data):
                self.assertFalse(GUARD.content_issues("settings", data))

    def test_ssh_fixture_exceptions_require_both_path_and_spelling(self):
        config = b"HostName " + b"10.0.0.1"
        command = b"ssh " + b"-T -o BatchMode=yes me@" + b"10.0.0.5"
        for path, fixture in (
            ("wezterm-ssh/src/config.rs", config), ("wezterm-gui/src/ssh_hosts.rs", config),
            ("docs/config/lua/SshDomain.md", command),
        ):
            self.assertFalse(GUARD.content_issues(path, fixture))
            self.assertTrue(GUARD.content_issues("new-fixture", fixture))
            self.assertTrue(GUARD.content_issues(path, fixture + b"9"))
        self.assertTrue(GUARD.content_issues("docs/config/lua/SshDomain.md", command.replace(b"me@", b"other@")))

    def test_utf16_uses_the_same_rules_as_utf8(self):
        name = b"synthetic-private-machine"
        personal = GUARD.personal_pattern([name])
        data = b"\n".join((name, personal_path(), tailnet_key(), b"ssh alice@" + b"fd00::42"))
        expected = GUARD.content_issues("settings", data, personal)
        self.assertEqual({rule.split(":", 1)[0] for rule in expected}, {"personal-machine", "personal-home", "provider-token", "ssh-endpoint"})
        for bom, encoding in ((codecs.BOM_UTF16_LE, "utf-16-le"), (codecs.BOM_UTF16_BE, "utf-16-be")):
            with self.subTest(encoding=encoding):
                self.assertEqual(GUARD.content_issues("settings", bom + data.decode().encode(encoding), personal), expected)
                self.assertFalse(GUARD.content_issues("settings", bom + "ssh alice@example.local".encode(encoding), personal))
        self.assertFalse(GUARD.content_issues("asset.bin", b"\0" + tailnet_key(), personal))

    def test_path_rules_handle_dashed_and_non_utf8_components(self):
        raw = b"reports/\xff/-Users-" + b"synthetic-private-person-src/notes.txt"
        path = raw.decode("utf-8", "surrogateescape")
        issues, shown = GUARD.inspect_blob(path, b"")
        self.assertTrue(issues)
        self.assertEqual(shown.encode("utf-8", "surrogateescape"), b"reports/\xff/[redacted]/notes.txt")
        for path in ("Users/alice/notes.txt", "logs/myhost.local/safe.txt", "keys/tskey-auth-example"):
            self.assertEqual(GUARD.inspect_blob(path, b""), (set(), path))

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

    def test_private_paths_are_blocked_and_redacted_in_index_and_tree(self):
        self.env.pop("CI", None)
        local = self.repo / GUARD.LOCAL_LIST
        local.parent.mkdir()
        name = "synthetic-unit-machine"
        local.write_text(name + "\n")
        user = "synthetic-unit-person"
        key = tailnet_key().decode()
        host = "synthetic-unit-node" + ".lan"
        paths = (
            Path("Users", user, "report.txt"),
            Path("notes", name.upper() + "-backup", "info.txt"),
            Path("notes", host, "trace.txt"),
            Path("keys", key + ".zip"),
            Path("exports", host + ".tar.gz"),
        )
        for relative in paths:
            path = self.repo / relative
            path.parent.mkdir(parents=True)
            path.touch()
        self.git("add", "--", *(str(path) for path in paths))
        tree = self.git("write-tree").strip().decode()
        for mode in (("--staged",), ("--tree", tree)):
            with self.subTest(mode=mode):
                result = self.scan(*mode)
                self.assertEqual(result.returncode, 1)
                log = result.stdout + result.stderr
                for value in (user, name, key, host):
                    self.assertNotIn(value.lower().encode(), log.lower())
                for rule in (b"personal-home", b"personal-machine", b"personal-hostname", b"provider-token", b"release-artifact"):
                    self.assertIn(rule, log)
                for shown in (b"Users/[redacted]/report.txt", b"notes/[redacted]/info.txt", b"notes/[redacted]/trace.txt", b"keys/[redacted]", b"exports/[redacted]"):
                    self.assertIn(shown, log)

    def test_content_match_also_redacts_an_ordinary_filename(self):
        self.env["CI"] = "true"
        user = "synthetic-content-user"
        path = self.repo / (user + ".txt")
        path.write_bytes(("/home/" + user + "/projects").encode("utf-16"))
        self.git("add", "--", path.name)
        result = self.scan()
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"personal-home", result.stderr)
        self.assertIn(b"[redacted]", result.stderr)
        self.assertNotIn(user.encode(), result.stdout + result.stderr)

    def test_unreadable_local_list_blocks_without_printing_its_contents(self):
        self.env.pop("CI", None)
        local = self.repo / GUARD.LOCAL_LIST
        self.assertEqual(self.scan().returncode, 0)
        local.mkdir(parents=True)
        result = self.scan()
        self.assertEqual(result.returncode, 2)
        self.assertIn(GUARD.LOCAL_LIST.encode(), result.stderr)
        self.assertNotIn(str(self.repo).encode(), result.stderr)
        local.rmdir()
        local.write_bytes(b"synthetic-private-name\n\xff")
        result = self.scan()
        self.assertEqual(result.returncode, 2)
        self.assertNotIn(b"synthetic-private-name", result.stderr)
        self.env["CI"] = "true"
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
