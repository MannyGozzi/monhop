import json
import pathlib
import plistlib
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone


ROOT = pathlib.Path(__file__).resolve().parents[2]
SWIFT = ROOT / "scripts/macos-signing.swift"
WRAPPER = ROOT / "scripts/macos-signing.sh"
BUILD = ROOT / "scripts/build-macos-app.sh"


class MacOSSigningScriptTests(unittest.TestCase):
    def setUp(self):
        self.swift = SWIFT.read_text()
        self.wrapper = WRAPPER.read_text()
        self.build = BUILD.read_text()

    def test_scripts_parse_without_running_keychain_or_codesign_actions(self):
        subprocess.run(["/usr/bin/xcrun", "swiftc", "-typecheck", str(SWIFT)], check=True)
        subprocess.run(["/bin/bash", "-n", str(WRAPPER), str(BUILD)], check=True)

    def test_build_signs_but_never_sets_up_or_uses_ad_hoc_fallback(self):
        self.assertIn('"$workspace/scripts/macos-signing.sh" sign "$bundle"', self.build)
        self.assertNotIn("macos-signing.sh\" setup", self.build)
        self.assertIn('Run setup explicitly; build never creates or falls back to ad-hoc signing.', self.swift)
        self.assertNotIn('"--sign", "-"', self.swift)

    def test_app_explains_explicit_local_network_access_without_discovery(self):
        with (ROOT / "apps/monhop-desktop/Info.plist").open("rb") as source:
            info = plistlib.load(source)
        self.assertIn("computer you verify", info["NSLocalNetworkUsageDescription"])
        self.assertIn("does not enable", info["NSLocalNetworkUsageDescription"])
        self.assertNotIn("NSBonjourServices", info)

    def test_release_and_local_signing_share_the_location_entitlement(self):
        desktop = ROOT / "apps/monhop-desktop"
        config = json.loads((desktop / "tauri.conf.json").read_text())
        macos = config["bundle"]["macOS"]
        self.assertTrue(macos.get("hardenedRuntime", True))
        entitlements = desktop / macos["entitlements"]
        with entitlements.open("rb") as source:
            self.assertEqual(plistlib.load(source), {"com.apple.security.personal-information.location": True})
        self.assertIn(str(entitlements.relative_to(ROOT)), self.swift)
        self.assertIn('"--entitlements", entitlements.path', self.swift)

    def test_local_and_release_builds_verify_the_final_signed_bundle(self):
        self.assertIn('python3 "$workspace/scripts/verify_macos_bundle.py" "$bundle"', self.build)
        workflow = (ROOT / ".github/workflows/build.yml").read_text()
        self.assertIn("scripts/verify_macos_bundle.py", workflow)

    def test_setup_keeps_private_material_encrypted_and_passphrases_off_argv_and_environment(self):
        self.assertIn('"genrsa", "-aes256", "-passout", "fd:0"', self.swift)
        self.assertIn('"req", "-new", "-x509", "-key", keyURL.path, "-passin", "fd:0"', self.swift)
        self.assertIn('"pkcs12", "-export", "-inkey", keyURL.path, "-passin", "fd:0"', self.swift)
        self.assertNotIn('"-nodes"', self.swift)
        self.assertNotIn('pass:', self.swift)
        self.assertNotIn('set-key-partition-list', self.swift)
        self.assertNotIn('SecKeychainSetSearchList', self.swift)
        self.assertNotIn('SecTrustSettingsSetTrustSettings', self.swift)

    def test_setup_rejects_ambiguous_or_orphan_identity_state_and_limits_key_access(self):
        self.assertIn('Multiple MonHop signing certificates exist', self.swift)
        self.assertIn('has no matching private key. Refusing to regenerate it', self.swift)
        self.assertIn('monHopSigningPrivateKeys', self.swift)
        self.assertIn('setupLock()', self.swift)
        self.assertIn('O_NOFOLLOW', self.swift)
        self.assertIn('let keyAttributes = [kSecAttrIsPermanent, kSecAttrIsSensitive] as CFArray', self.swift)
        self.assertIn('attributes?[kSecAttrIsExtractable] as? Bool == false', self.swift)
        self.assertIn('SecAccessCreate(certificateCommonName as CFString, [trustedApplication] as CFArray, &access)', self.swift)
        self.assertIn('validateCodesignAccess(for: privateKey)', self.swift)

    def test_signing_is_scoped_to_login_keychain_and_bound_to_certificate(self):
        self.assertIn('"--keychain", identity.loginKeychainPath', self.swift)
        self.assertIn('"--timestamp=none"', self.swift)
        self.assertIn('"--options", "runtime"', self.swift)
        self.assertIn('certificate leaf = H', self.swift)
        self.assertIn('\"--test-requirement\", \"=\\(requirement)\"', self.swift)
        self.assertIn('embeddedRequirements.contains(requirement)', self.swift)

    def test_requirement_canonicalization_is_lowercase(self):
        source = '=identifier "com.manuelgozzi.monhop" and certificate leaf = H"ABCDEF0123456789ABCDEF0123456789ABCDEF01"'
        result = subprocess.run(["/usr/bin/csreq", "-r", source, "-t"], check=True, text=True, stdout=subprocess.PIPE)
        self.assertIn('H"abcdef0123456789abcdef0123456789abcdef01"', result.stdout)
        self.assertIn('String(format: "%02x", $0)', self.swift)

    def test_disposable_certificate_validation_and_in_memory_codesign_acl(self):
        with tempfile.TemporaryDirectory(prefix="monhop-signing-certificate-test-") as directory:
            directory = pathlib.Path(directory)
            valid = self._self_signed_fixture(directory, "valid", "codeSigning", "FALSE")
            self._assert_certificate_validation(valid, succeeds=True, validate_acl=True)

    def test_disposable_certificate_validation_rejects_corrupt_unsupported_and_invalid_dates(self):
        with tempfile.TemporaryDirectory(prefix="monhop-signing-certificate-negative-test-") as directory:
            directory = pathlib.Path(directory)
            valid = self._self_signed_fixture(directory, "valid", "codeSigning", "FALSE")
            damaged = directory / "damaged.der"
            bytes_ = bytearray(valid.read_bytes())
            bytes_[-1] ^= 1
            damaged.write_bytes(bytes_)
            self._assert_certificate_validation(damaged, succeeds=False)
            self._assert_certificate_validation(self._self_signed_fixture(directory, "wrong-eku", "serverAuth", "FALSE"), succeeds=False)
            self._assert_certificate_validation(self._self_signed_fixture(directory, "certificate-authority", "codeSigning", "TRUE"), succeeds=False)
            self._assert_certificate_validation(self._self_signed_fixture(directory, "authority-with-ca-false-subject", "codeSigning", "TRUE", "/CN=MonHop CA:FALSE fixture/OU=CA:FALSE"), succeeds=False)

            now = datetime.now(timezone.utc)
            self._assert_certificate_validation(self._dated_self_signed_fixture(directory, "expired", now - timedelta(days=3), now - timedelta(days=2)), succeeds=False)
            self._assert_certificate_validation(self._dated_self_signed_fixture(directory, "future", now + timedelta(days=2), now + timedelta(days=3)), succeeds=False)

    def _assert_certificate_validation(self, certificate_der, succeeds, validate_acl=False):
        implementation = self.swift.rsplit("\ndo {\n", 1)[0]
        acl = ""
        if validate_acl:
            acl = """
var trustedApplication: SecTrustedApplication?
try requireSuccess(SecTrustedApplicationCreateFromPath(codesignPath, &trustedApplication), "Creating disposable codesign ACL")
var access: SecAccess?
try requireSuccess(SecAccessCreate(certificateCommonName as CFString, [trustedApplication!] as CFArray, &access), "Creating disposable signing ACL")
try validateCodesignAccess(access!)
"""
        harness = """
let fixture = SecCertificateCreateWithData(nil, try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])) as CFData)!
try validateCertificate(fixture)
""" + acl
        probe = certificate_der.parent / f"validate_{certificate_der.stem}.swift"
        probe.write_text(implementation + "\n" + harness)
        result = subprocess.run(["/usr/bin/xcrun", "swift", str(probe), str(certificate_der)], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if succeeds:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stderr)

    def _self_signed_fixture(self, directory, name, eku, certificate_authority, subject=None):
        key = directory / f"{name}.key.pem"
        certificate = directory / f"{name}.pem"
        certificate_der = directory / f"{name}.der"
        password = b"disposable-test-passphrase\n"
        commands = [
            (["/usr/bin/openssl", "genrsa", "-aes256", "-passout", "fd:0", "-out", str(key), "2048"], password),
            (["/usr/bin/openssl", "req", "-new", "-x509", "-key", str(key), "-passin", "fd:0", "-out", str(certificate), "-days", "2", "-batch", "-sha256", "-subj", subject or f"/CN=MonHop {name} fixture", "-addext", f"basicConstraints=critical,CA:{certificate_authority}", "-addext", "keyUsage=critical,digitalSignature", "-addext", f"extendedKeyUsage={eku}"], password),
            (["/usr/bin/openssl", "x509", "-in", str(certificate), "-outform", "der", "-out", str(certificate_der)], None),
        ]
        for command, standard_input in commands:
            subprocess.run(command, input=standard_input, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        return certificate_der

    def _dated_self_signed_fixture(self, directory, name, start, end):
        key = directory / f"{name}.key.pem"
        request = directory / f"{name}.req.pem"
        certificate = directory / f"{name}.pem"
        certificate_der = directory / f"{name}.der"
        index = directory / f"{name}.index.txt"
        serial = directory / f"{name}.serial"
        config = directory / f"{name}.openssl.cnf"
        password = b"disposable-test-passphrase\n"
        index.write_text("")
        serial.write_text("01\n")
        config.write_text(
            "[ ca ]\n"
            "default_ca = local\n"
            "[ local ]\n"
            f"database = {index}\n"
            f"new_certs_dir = {directory}\n"
            f"serial = {serial}\n"
            "default_md = sha256\n"
            "policy = supplied\n"
            "x509_extensions = profile\n"
            "[ supplied ]\n"
            "commonName = supplied\n"
            "[ profile ]\n"
            "basicConstraints = critical,CA:FALSE\n"
            "keyUsage = critical,digitalSignature\n"
            "extendedKeyUsage = codeSigning\n"
        )
        commands = [
            (["/usr/bin/openssl", "genrsa", "-aes256", "-passout", "fd:0", "-out", str(key), "2048"], password),
            (["/usr/bin/openssl", "req", "-new", "-key", str(key), "-passin", "fd:0", "-out", str(request), "-subj", f"/CN=MonHop {name} fixture"], password),
            (["/usr/bin/openssl", "ca", "-selfsign", "-batch", "-config", str(config), "-keyfile", str(key), "-passin", "fd:0", "-in", str(request), "-out", str(certificate), "-startdate", start.strftime("%y%m%d%H%M%SZ"), "-enddate", end.strftime("%y%m%d%H%M%SZ")], password),
            (["/usr/bin/openssl", "x509", "-in", str(certificate), "-outform", "der", "-out", str(certificate_der)], None),
        ]
        for command, standard_input in commands:
            subprocess.run(command, input=standard_input, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        return certificate_der


if __name__ == "__main__":
    unittest.main()
