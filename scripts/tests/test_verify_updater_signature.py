import base64
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "verify_updater_signature.py"
SPEC = importlib.util.spec_from_file_location("verify_updater_signature", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)

# RFC 8032 section 7.1, test 1 (empty message).
RFC_PUBLIC = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
RFC_SIGNATURE = bytes.fromhex(
    "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
)
# Signed by OpenSSL 3.6 with a throwaway key.
OPENSSL_PUBLIC = bytes.fromhex("8f32966c4446c98cf223d0ecea9171f0b91f39ffd3f19f5aad69ceaae1acb9ca")
OPENSSL_OTHER_PUBLIC = bytes.fromhex("a2c395207b35946db102075c5627b55032814ea0c364264b6db4f1d761ea738e")
OPENSSL_MESSAGE = b"MonHop release"
OPENSSL_SIGNATURE = bytes.fromhex(
    "8bd8e6a18a89720f8e9a16434a1cdec084dec4ef8341bfb2821f948cf652dbd88543ed4456ca0dd64f1e94afcc25cacccb71b322dd81c825aeca3ffa6fed7107"
)
# `cargo tauri signer sign` (tauri-cli 2.11.4) output for FIXTURE_DATA with a throwaway key 2E52BA623C911066.
FIXTURE_DATA = b"MonHop updater signature fixture\n"
FIXTURE_PUBLIC_KEY = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDJFNTJCQTYyM0M5MTEwNjYKUldSbUVKRThZcnBTTHNaK0lGQTM2"
    "V3NkRmpJbXBONXRnZkRGWVZxOTF4WjA1and4bktQcmN3RGgK"
)
FIXTURE_SIGNATURE = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSbUVKRThZcnBTTGtXMzlkYUkvWWpRUGJP"
    "N3l1T3lxc2JOak5ydXBtdWI2T2dLcVRVbkVCT1hIaTV5UDRkc01YSC9ZeXpLUk84S1FQc2tZemh1Q2tudWwzN1Y0V2ExaXdnPQp0cnVz"
    "dGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxOTQzMTg1CWZpbGU6ZGF0YS50eHQKY2R6dExCSzY0S0lKN01zK3VLNFMzMDBtR3lTaTdl"
    "WUdrL0c3Sm1VWHUyR0hpM1o2OEJqY2tRb1RPSVhYZnJVcDVrcUNVdXhtdDlXUjZ2TFE4bHkyRGc9PQo="
)


def rewrite_line(encoded: str, index: int, replacement: str) -> str:
    lines = base64.b64decode(encoded).decode().splitlines()
    lines[index] = replacement
    return base64.b64encode(("\n".join(lines) + "\n").encode()).decode()


class Ed25519Tests(unittest.TestCase):
    def test_rfc_8032_vector_verifies(self):
        self.assertTrue(verifier.ed25519_verify(RFC_PUBLIC, b"", RFC_SIGNATURE))

    def test_openssl_signature_verifies_and_rejects_changes(self):
        self.assertTrue(verifier.ed25519_verify(OPENSSL_PUBLIC, OPENSSL_MESSAGE, OPENSSL_SIGNATURE))
        self.assertFalse(verifier.ed25519_verify(OPENSSL_PUBLIC, OPENSSL_MESSAGE + b"!", OPENSSL_SIGNATURE))
        self.assertFalse(verifier.ed25519_verify(OPENSSL_OTHER_PUBLIC, OPENSSL_MESSAGE, OPENSSL_SIGNATURE))
        flipped = bytearray(OPENSSL_SIGNATURE)
        flipped[40] ^= 1
        self.assertFalse(verifier.ed25519_verify(OPENSSL_PUBLIC, OPENSSL_MESSAGE, bytes(flipped)))

    def test_malformed_inputs_are_rejected(self):
        self.assertFalse(verifier.ed25519_verify(OPENSSL_PUBLIC[:31], OPENSSL_MESSAGE, OPENSSL_SIGNATURE))
        self.assertFalse(verifier.ed25519_verify(OPENSSL_PUBLIC, OPENSSL_MESSAGE, OPENSSL_SIGNATURE[:63]))
        out_of_range = OPENSSL_SIGNATURE[:32] + (verifier.Q).to_bytes(32, "little")
        self.assertFalse(verifier.ed25519_verify(OPENSSL_PUBLIC, OPENSSL_MESSAGE, out_of_range))


class MinisignTests(unittest.TestCase):
    def test_tauri_signature_verifies(self):
        verifier.verify(FIXTURE_DATA, FIXTURE_SIGNATURE, FIXTURE_PUBLIC_KEY)
        self.assertEqual(verifier.key_id(FIXTURE_PUBLIC_KEY), "2E52BA623C911066")

    def test_changed_file_fails(self):
        with self.assertRaisesRegex(verifier.SignatureError, "does not match the file"):
            verifier.verify(FIXTURE_DATA + b"x", FIXTURE_SIGNATURE, FIXTURE_PUBLIC_KEY)

    def test_signature_from_another_key_fails(self):
        with self.assertRaisesRegex(verifier.SignatureError, "expected 73B03A45DB934B17"):
            verifier.verify(FIXTURE_DATA, FIXTURE_SIGNATURE, verifier.app_public_key())

    def test_changed_trusted_comment_fails(self):
        forged = rewrite_line(FIXTURE_SIGNATURE, 2, "trusted comment: timestamp:1\tfile:data.txt")
        with self.assertRaisesRegex(verifier.SignatureError, "trusted comment"):
            verifier.verify(FIXTURE_DATA, forged, FIXTURE_PUBLIC_KEY)

    def test_garbage_is_a_signature_error(self):
        for signature in ("not base64!", base64.b64encode(b"one line").decode()):
            with self.subTest(signature=signature):
                with self.assertRaises(verifier.SignatureError):
                    verifier.verify(FIXTURE_DATA, signature, FIXTURE_PUBLIC_KEY)

    def test_app_public_key_comes_from_tauri_config(self):
        self.assertEqual(verifier.key_id(verifier.app_public_key()), "73B03A45DB934B17")


class CommandLineTests(unittest.TestCase):
    def test_exit_status_reports_every_signature(self):
        with tempfile.TemporaryDirectory() as scratch:
            good = Path(scratch, "good.txt")
            good.write_bytes(FIXTURE_DATA)
            Path(scratch, "good.txt.sig").write_text(FIXTURE_SIGNATURE)
            bad = Path(scratch, "bad.txt")
            bad.write_bytes(FIXTURE_DATA + b"x")
            Path(scratch, "bad.txt.sig").write_text(FIXTURE_SIGNATURE)
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                ok = verifier.main([f"{good}.sig", "--public-key", FIXTURE_PUBLIC_KEY])
                failed = verifier.main([f"{good}.sig", f"{bad}.sig", "--public-key", FIXTURE_PUBLIC_KEY])
            missing = verifier.main([str(Path(scratch, "absent.txt.sig")), "--public-key", FIXTURE_PUBLIC_KEY])

        self.assertEqual((ok, failed, missing), (0, 1, 1))
        self.assertIn("good.txt.sig: verified with key 2E52BA623C911066", output.getvalue())
        self.assertIn("bad.txt.sig: FAILED", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
