#!/usr/bin/env python3
"""Exercise signing logs with fake tools and credentials, never a real keychain."""

import base64
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PAYLOAD = "Example Person /home/user/private user@example.invalid example-key-material"
MOCK = r'''
import json
import os
from pathlib import Path
import plistlib
import sys

tool = Path(sys.argv[0]).name
args = sys.argv[1:]
if tool == "sleep":
    sys.exit(0)
stage = tool + " " + " ".join(args[:2] if tool == "xcrun" else args[:1])
payload = os.environ["MOCK_PAYLOAD"]
home = Path(os.environ["HOME"])
print(payload, file=sys.stderr)
print(" ".join(args), file=sys.stderr)
if stage == os.environ.get("MOCK_FAIL_STAGE"):
    print(payload)
    print(os.environ.get("MOCK_REASON", "unrecognized example failure"), file=sys.stderr)
    sys.exit(42)
if stage == os.environ.get("MOCK_FAIL_ONCE") and not (home / "failed-once").exists():
    (home / "failed-once").touch()
    print(os.environ.get("MOCK_REASON", "unrecognized example failure"), file=sys.stderr)
    sys.exit(42)
if tool == "security" and args == ["default-keychain", "-d", "user"]:
    print('"/home/user/example.keychain"')
elif tool == "security" and args[0] == "create-keychain":
    (home / "created").touch()
elif tool == "security" and args[0] == "delete-keychain":
    # Like the real tool, deleting a keychain that was never made fails.
    if not (home / "created").exists():
        print("The specified keychain could not be found.", file=sys.stderr)
        sys.exit(50)
    (home / "deleted").touch()
elif tool == "security" and args[0] == "default-keychain" and args[-1] == "/home/user/example.keychain":
    Path(os.environ["HOME"], "restored").touch()
elif tool == "uuidgen":
    print("00000000-0000-0000-0000-000000000000")
elif tool == "codesign" and args[0] == "-dv":
    print("Authority=Developer ID Application: Example Person", file=sys.stderr)
elif tool == "xcrun" and args[:2] == ["notarytool", "submit"]:
    sys.stdout.buffer.write(plistlib.dumps({"status": os.environ.get("MOCK_STATUS", "Accepted"), "id": payload}))
    sys.exit(int(os.environ.get("MOCK_SUBMIT_EXIT", "0")))
elif tool == "xcrun" and args[:2] == ["notarytool", "log"]:
    bundle = "submission.zip/Example Person.app/"
    issue = {"severity": "error", "code": None, "message": os.environ.get("MOCK_REASON", "The signature is invalid.")}
    paths = [
        bundle + "Contents/MacOS/example",  # shipped, so named
        bundle + "Contents/MacOS/example",  # the same file's other architecture
        bundle + "Contents/" + payload,
        "../../home/user/private-note.txt",
        bundle + "Contents/MacOS/../../../home/user/private-note.txt",
        bundle + "Contents/../../bin/uuidgen",  # exists, but outside the bundle
        bundle + "Contents/Resources/ExamplePerson.txt",  # not in the bundle
    ]
    print(json.dumps({"status": "Invalid", "ticketContents": None,
                      "issues": [{**issue, "path": path} for path in paths]}))
else:
    print(payload)
'''


class SigningLogTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="tt-signing-", dir="/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.home = self.root / "home"
        self.scratch = self.root / "scratch"
        self.bin = self.root / "bin"
        for path in (self.home, self.scratch, self.bin):
            path.mkdir()
        for tool in ("security", "codesign", "uuidgen", "openssl", "xcrun", "spctl", "sleep"):
            path = self.bin / tool
            path.write_text("#!" + sys.executable + "\n" + MOCK)
            path.chmod(0o755)
        self.env = {
            "PATH": str(self.bin) + ":/usr/bin:/bin",
            "HOME": str(self.home),
            "TMPDIR": str(self.scratch),
            "MOCK_PAYLOAD": PAYLOAD,
        }
        self.credentials = {
            "MACOS_CERT": base64.b64encode(b"0example").decode(),
            "MACOS_CERT_PW": base64.b64encode(b"example-password").decode(),
            "MACOS_TEAM_ID": "EXAMPLE123",
            "MACOS_NOTARY_KEY": base64.b64encode(b"-----BEGIN PRIVATE KEY-----\nexample\n").decode(),
            "MACOS_NOTARY_KEY_ID": "EXAMPLE1234",
            "MACOS_NOTARY_ISSUER": "00000000-0000-0000-0000-000000000000",
        }

    def run_script(self, name, *args, extra=None):
        return subprocess.run(
            ["/bin/bash", str(ROOT / "ci" / name), *map(str, args)],
            env={**self.env, **self.credentials, **(extra or {})},
            text=True, capture_output=True, timeout=30,
        )

    def assert_private(self, result):
        output = result.stdout + result.stderr
        for marker in ("Example Person", "/home/user", "user@example.invalid", "example-key-material", "example-password", "EXAMPLE123", str(self.root)):
            self.assertNotIn(marker, output)
        self.assertEqual(list(self.scratch.iterdir()), [])
        return output

    def clear_markers(self):
        for marker in ("created", "deleted", "restored", "failed-once"):
            (self.home / marker).unlink(missing_ok=True)

    def test_preflight_success_hides_success_output(self):
        result = self.run_script("macos-check-signing.sh")
        output = self.assert_private(result)
        self.assertEqual(result.returncode, 0, output)
        self.assertTrue((self.home / "restored").exists())
        self.assertTrue((self.home / "deleted").exists())

    def test_preflight_failures_keep_reason_and_cleanup(self):
        cases = [
            # (environment, expected reason, whether a keychain was made)
            ({"MACOS_TEAM_ID": "example"}, "MACOS_TEAM_ID is not a team ID", False),
            ({"MOCK_FAIL_STAGE": "security create-keychain", "MOCK_REASON": "permission denied"}, "access was denied", False),
            ({"MOCK_FAIL_STAGE": "security import", "MOCK_REASON": "MAC verification failed"}, "could not be decrypted", True),
            ({"MOCK_FAIL_STAGE": "codesign --keychain", "MOCK_REASON": "no identity found"}, "No matching signing identity", True),
            ({"MOCK_FAIL_STAGE": "xcrun notarytool history", "MOCK_REASON": "HTTP status code: 401"}, "Authentication was rejected", True),
            ({"MOCK_FAIL_STAGE": "xcrun notarytool history", "MOCK_REASON": "connection timed out"}, "request timed out", True),
            ({"MOCK_FAIL_STAGE": "xcrun notarytool history", "MOCK_REASON": "::error::" + PAYLOAD}, "No recognized diagnostic", True),
        ]
        for extra, expected, made in cases:
            with self.subTest(extra=extra):
                self.clear_markers()
                result = self.run_script("macos-check-signing.sh", extra=extra)
                output = self.assert_private(result)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, output)
                # Cleanup removes only what the run made, without a second error.
                self.assertNotIn("Remove signing keychain", output)
                self.assertEqual((self.home / "deleted").exists(), made)

    def app(self):
        app = self.root / "Example Person.app"
        (app / "Contents/MacOS").mkdir(parents=True, exist_ok=True)
        (app / "Contents/MacOS/example").write_text("example")
        return app

    def test_notarization_success_hides_ids_paths_and_verification_output(self):
        result = self.run_script("macos-notarize-local.sh", self.app())
        output = self.assert_private(result)
        self.assertEqual(result.returncode, 0, output)
        self.assertIn("Notarized and stapled successfully", output)

    def test_notarization_failures_are_classified(self):
        cases = [
            ({"MOCK_FAIL_STAGE": "xcrun notarytool submit", "MOCK_REASON": "HTTP status code: 403"}, "Access was denied"),
            ({"MOCK_STATUS": "Invalid", "MOCK_REASON": "The signature is invalid; hardened runtime is missing"}, "Hardened Runtime"),
            ({"MOCK_STATUS": PAYLOAD, "MOCK_REASON": PAYLOAD}, "No recognized diagnostic"),
            ({"MOCK_FAIL_STAGE": "xcrun stapler staple", "MOCK_REASON": "ticket not found"}, "ticket is not available"),
            ({"MOCK_FAIL_STAGE": "xcrun stapler validate", "MOCK_REASON": "signature invalid"}, "signature is invalid"),
            ({"MOCK_FAIL_STAGE": "codesign --verify", "MOCK_REASON": "sealed resource is missing"}, "missing resources"),
            ({"MOCK_FAIL_STAGE": "spctl --assess", "MOCK_REASON": PAYLOAD}, "Assess Gatekeeper acceptance"),
        ]
        for extra, expected in cases:
            with self.subTest(extra=extra):
                result = self.run_script("macos-notarize-local.sh", self.app(), extra=extra)
                output = self.assert_private(result)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, output)

    def test_notarization_rejection_names_only_shipped_files(self):
        # The submit itself may or may not fail on a rejected submission.
        for submit_exit in ("0", "1"):
            with self.subTest(submit_exit=submit_exit):
                result = self.run_script("macos-notarize-local.sh", self.app(), extra={
                    "MOCK_STATUS": "Invalid", "MOCK_SUBMIT_EXIT": submit_exit, "MOCK_REASON": "The binary is not signed."})
                output = self.assert_private(result)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("Submit for notarization failed", output)
                self.assertIn("unsigned", output)
                self.assertEqual(output.count("Rejected file: Contents/MacOS/example\n"), 1, output)
                self.assertIn("5 rejected file name(s) withheld", output)
                for name in ("private-note", "uuidgen", "ExamplePerson"):
                    self.assertNotIn(name, output)

    def test_notarization_staple_retry_logs_no_error(self):
        result = self.run_script("macos-notarize-local.sh", self.app(), extra={"MOCK_FAIL_ONCE": "xcrun stapler staple", "MOCK_REASON": "file does not exist"})
        output = self.assert_private(result)
        self.assertEqual(result.returncode, 0, output)
        self.assertIn("retrying", output)
        self.assertNotIn("error", output)

    def test_local_signing_failures_are_named(self):
        for mode, expected in (("typo", "MACOS_SIGNING_MODE has to be"), ("development", "No matching certificate")):
            with self.subTest(mode=mode):
                result = subprocess.run(
                    ["/bin/bash", "-c", '. "$1"; signing_run "Sign app bundle" bash "$2" "$3" "$4"', "example",
                     str(ROOT / "ci/macos-signing-log.sh"), str(ROOT / "ci/macos-sign-local.sh"), str(self.app()), mode],
                    env={**self.env, **self.credentials}, text=True, capture_output=True, timeout=30,
                )
                output = self.assert_private(result)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, output)

    def test_deploy_failures_hide_output_and_clean_up(self):
        source = (ROOT / "ci/deploy.sh").read_text()
        # Execute the actual signing branch, stopping at a mocked error.
        # This avoids packaging or invoking the absolute codesign executable.
        branch = source.split('    elif [ -n "$MACOS_TEAM_ID" ] ; then\n', 1)[1].split('    else\n', 1)[0]
        cases = [
            # (failing stage, its output, expected reason, whether a keychain was made)
            ("security default-keychain", "permission denied", "access was denied", False),
            ("security import", "MAC verification failed", "could not be decrypted", True),
        ]
        for stage, reason, expected, made in cases:
            with self.subTest(stage=stage):
                self.clear_markers()
                result = subprocess.run(
                    ["/bin/bash", "-c", 'set -e\n. "$1"\n' + branch, "example", str(ROOT / "ci/macos-signing-log.sh")],
                    env={**self.env, **self.credentials, "MOCK_FAIL_STAGE": stage, "MOCK_REASON": reason},
                    text=True, capture_output=True, timeout=30,
                )
                output = self.assert_private(result)
                self.assertEqual(result.returncode, 42, output)
                self.assertIn(expected, output)
                self.assertNotIn("Remove signing keychain", output)
                self.assertEqual((self.home / "restored").exists(), made)
                self.assertEqual((self.home / "deleted").exists(), made)

    def test_multiline_mask_is_one_workflow_command(self):
        result = subprocess.run(
            ["/bin/bash", "-c", '. "$1"; signing_mask "$2"', "example", str(ROOT / "ci/macos-signing-log.sh"), "example%\r\n::error::example"],
            env={**self.env, "GITHUB_ACTIONS": "true"}, text=True, capture_output=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "::add-mask::example%25%0D%0A::error::example\n")
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()
