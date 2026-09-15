import contextlib
import importlib.util
import io
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).resolve().parents[1] / "verify_macos_bundle.py"
SPEC = importlib.util.spec_from_file_location("verify_macos_bundle", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
verify_macos_bundle = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify_macos_bundle)


LOCATION_ENTITLEMENT = "com.apple.security.personal-information.location"


class FixtureRunner:
    def __init__(self):
        self.commands = []
        self.metadata = "flags=0x10002(adhoc,runtime)"
        self.entitlements = {LOCATION_ENTITLEMENT: True}
        self.failures = set()

    def __call__(self, command):
        command = tuple(command)
        self.commands.append(command)
        if "--verify" in command:
            phase, stdout, stderr = "verification", "", ""
        elif "-dvvv" in command:
            phase, stdout, stderr = "metadata", "", self.metadata
        else:
            phase, stdout, stderr = "entitlements", plistlib.dumps(self.entitlements).decode("utf-8"), ""
        if phase in self.failures:
            return subprocess.CompletedProcess(command, 1, stdout, stderr)
        return subprocess.CompletedProcess(command, 0, stdout, stderr)


class VerifyMacosBundleTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="monhop-bundle-")
        self.addCleanup(self.directory.cleanup)
        self.bundle = Path(self.directory.name) / "MonHop.app"
        info = self.bundle / "Contents" / "Info.plist"
        info.parent.mkdir(parents=True)
        with info.open("wb") as destination:
            plistlib.dump(
                {
                    "CFBundleIdentifier": "com.manuelgozzi.monhop",
                    "NSLocationUsageDescription": "Identify the selected Wi-Fi attachment.",
                    "NSLocationWhenInUseUsageDescription": "Identify the selected Wi-Fi attachment.",
                },
                destination,
            )
        self.runner = FixtureRunner()

    def info(self):
        path = self.bundle / "Contents" / "Info.plist"
        with path.open("rb") as source:
            return plistlib.load(source)

    def write_info(self, info):
        with (self.bundle / "Contents" / "Info.plist").open("wb") as destination:
            plistlib.dump(info, destination)

    def verify(self):
        verify_macos_bundle.verify_bundle(self.bundle, self.runner)

    def test_accepts_a_strict_hardened_bundle_with_location_access(self):
        self.verify()
        self.assertEqual(
            self.runner.commands,
            [
                ("codesign", "--verify", "--deep", "--strict", str(self.bundle)),
                ("codesign", "-dvvv", str(self.bundle)),
                ("codesign", "--display", "--entitlements", "-", "--xml", str(self.bundle)),
            ],
        )

    def test_rejects_missing_or_false_location_entitlement(self):
        for entitlement in ({}, {LOCATION_ENTITLEMENT: False}):
            with self.subTest(entitlement=entitlement):
                self.runner.entitlements = entitlement
                with self.assertRaisesRegex(
                    verify_macos_bundle.BundleVerificationError, "location entitlement must be true"
                ):
                    self.verify()

    def test_rejects_missing_location_usage_and_incorrect_identifier(self):
        missing_usage = self.info()
        del missing_usage["NSLocationWhenInUseUsageDescription"]
        self.write_info(missing_usage)
        with self.assertRaisesRegex(
            verify_macos_bundle.BundleVerificationError, "NSLocationWhenInUseUsageDescription"
        ):
            self.verify()

        incorrect_identifier = self.info()
        incorrect_identifier["NSLocationWhenInUseUsageDescription"] = "Identify the selected Wi-Fi attachment."
        incorrect_identifier["CFBundleIdentifier"] = "com.example.monhop"
        self.write_info(incorrect_identifier)
        with self.assertRaisesRegex(verify_macos_bundle.BundleVerificationError, "bundle identifier"):
            self.verify()

    def test_rejects_invalid_signature(self):
        self.runner.failures.add("verification")
        with self.assertRaisesRegex(verify_macos_bundle.BundleVerificationError, "codesign verification failed"):
            self.verify()

    def test_rejects_absent_or_malformed_runtime_metadata(self):
        for metadata, message in (("flags=0x2(adhoc)", "hardened runtime"), ("Signature=adhoc", "no flags")):
            with self.subTest(metadata=metadata):
                self.runner.metadata = metadata
                with self.assertRaisesRegex(verify_macos_bundle.BundleVerificationError, message):
                    self.verify()

    def test_rejects_codesign_command_failure(self):
        self.runner.failures.add("entitlements")
        with self.assertRaisesRegex(verify_macos_bundle.BundleVerificationError, "entitlement display failed"):
            self.verify()

    def test_cli_reports_only_the_safe_verifier_error(self):
        output = io.StringIO()
        error = verify_macos_bundle.BundleVerificationError("signed app has no readable entitlements.")
        with mock.patch.object(verify_macos_bundle, "verify_bundle", side_effect=error):
            with contextlib.redirect_stderr(output):
                code = verify_macos_bundle.main([str(self.bundle)])
        self.assertEqual(code, 1)
        self.assertEqual(output.getvalue(), "macOS bundle verification failed: signed app has no readable entitlements.\n")


if __name__ == "__main__":
    unittest.main()
