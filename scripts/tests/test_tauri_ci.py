import contextlib
import importlib.util
import io
from pathlib import Path
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "tauri_ci.py"
SPEC = importlib.util.spec_from_file_location("tauri_ci", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
tauri_ci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tauri_ci)


class Completed:
    def __init__(self, returncode):
        self.returncode = returncode


class RecordingRunner:
    def __init__(self, returncode=0):
        self.returncode = returncode
        self.calls = []

    def __call__(self, command, **kwargs):
        self.calls.append((command, kwargs))
        return Completed(self.returncode)


class TauriCiTests(unittest.TestCase):
    def test_empty_apple_settings_are_omitted_and_updater_settings_survive(self):
        environment = {
            "PATH": "test-path",
            "TAURI_SIGNING_PRIVATE_KEY": "updater-key",
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "",
            "APPLE_CERTIFICATE": "",
            "APPLE_CERTIFICATE_PASSWORD": "",
            "APPLE_SIGNING_IDENTITY": "",
            "APPLE_ID": "",
            "APPLE_PASSWORD": "",
            "APPLE_TEAM_ID": "",
            "APPLE_PROVIDER_SHORT_NAME": "",
        }

        child = tauri_ci.tauri_environment(environment)

        self.assertEqual(
            child,
            {
                "PATH": "test-path",
                "TAURI_SIGNING_PRIVATE_KEY": "updater-key",
                "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "",
            },
        )

    def test_complete_certificate_settings_preserve_an_empty_password(self):
        environment = {
            "APPLE_CERTIFICATE": "certificate",
            "APPLE_CERTIFICATE_PASSWORD": "",
            "APPLE_SIGNING_IDENTITY": "Developer ID Application: Example",
        }

        child = tauri_ci.tauri_environment(environment)

        self.assertEqual(child, environment)
        without_password = dict(environment)
        without_password.pop("APPLE_CERTIFICATE_PASSWORD")
        self.assertEqual(tauri_ci.tauri_environment(without_password)["APPLE_CERTIFICATE_PASSWORD"], "")

    def test_complete_apple_settings_are_preserved(self):
        environment = {
            "TAURI_SIGNING_PRIVATE_KEY": "updater-key",
            "APPLE_CERTIFICATE": "certificate",
            "APPLE_CERTIFICATE_PASSWORD": "certificate-password",
            "APPLE_SIGNING_IDENTITY": "Developer ID Application: Example",
            "APPLE_ID": "build@example.invalid",
            "APPLE_PASSWORD": "app-password",
            "APPLE_TEAM_ID": "TEAMID",
            "APPLE_PROVIDER_SHORT_NAME": "provider",
        }

        self.assertEqual(tauri_ci.tauri_environment(environment), environment)

    def test_partial_certificate_and_notarization_settings_fail_without_values_in_errors(self):
        cases = (
            {"APPLE_CERTIFICATE": "certificate"},
            {"APPLE_SIGNING_IDENTITY": "identity"},
            {"APPLE_CERTIFICATE_PASSWORD": "password"},
            {"APPLE_ID": "build@example.invalid"},
            {"APPLE_ID": "build@example.invalid", "APPLE_PASSWORD": "app-password"},
        )
        for environment in cases:
            with self.subTest(environment=environment):
                with self.assertRaises(tauri_ci.ConfigurationError) as error:
                    tauri_ci.tauri_environment(environment)
                self.assertNotIn(next(iter(environment.values())), str(error.exception))

    def test_cargo_tauri_receives_unchanged_arguments_environment_and_exit_status(self):
        runner = RecordingRunner(returncode=17)
        environment = {
            "TAURI_SIGNING_PRIVATE_KEY": "updater-key",
            "APPLE_CERTIFICATE": "certificate",
            "APPLE_CERTIFICATE_PASSWORD": "",
            "APPLE_SIGNING_IDENTITY": "identity",
        }

        code = tauri_ci.main(
            ["build", "--target", "aarch64-apple-darwin", "--ci", "--", "--locked"],
            environment,
            runner,
        )

        self.assertEqual(code, 17)
        self.assertEqual(
            runner.calls,
            [
                (
                    ["cargo", "tauri", "build", "--target", "aarch64-apple-darwin", "--ci", "--", "--locked"],
                    {"env": environment, "check": False},
                )
            ],
        )

    def test_an_empty_injected_environment_is_not_replaced_by_process_environment(self):
        runner = RecordingRunner()

        self.assertEqual(tauri_ci.main(["build"], {}, runner), 0)
        self.assertEqual(runner.calls, [(["cargo", "tauri", "build"], {"env": {}, "check": False})])

    def test_partial_settings_stop_before_cargo_and_do_not_print_values(self):
        runner = RecordingRunner()
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors):
            code = tauri_ci.main(["build"], {"APPLE_ID": "build@example.invalid"}, runner)

        self.assertEqual(code, 1)
        self.assertEqual(runner.calls, [])
        self.assertIn("APPLE_PASSWORD", errors.getvalue())
        self.assertNotIn("build@example.invalid", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
