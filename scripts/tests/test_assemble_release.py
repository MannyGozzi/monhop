import importlib.util
import json
from datetime import datetime, timezone
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "assemble_release.py"
SPEC = importlib.util.spec_from_file_location("assemble_release", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
assemble_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assemble_release)


VERSION = "1.2.3"
REPOSITORY = "MannyGozzi/monhop"


class AssembleReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="monhop-assemble-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.artifacts = self.root / "artifacts"
        self.output = self.root / "output"
        self.notes = self.root / "notes.md"
        self.notes.write_text("### Fixed\n- Safer updates.\n", encoding="utf-8")
        self.write_artifacts()

    def asset(self, target, name, contents):
        path = self.artifacts / target / "nested" / "bundle" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)
        return path

    def write_artifacts(self):
        self.arm_dmg = self.asset(
            "monhop-aarch64-apple-darwin", f"MonHop_{VERSION}_aarch64.dmg", b"arm dmg"
        )
        self.arm_archive = self.asset("monhop-aarch64-apple-darwin", "MonHop.app.tar.gz", b"arm archive")
        self.arm_signature = self.asset("monhop-aarch64-apple-darwin", "MonHop.app.tar.gz.sig", b"arm-signature\n")
        self.intel_dmg = self.asset(
            "monhop-x86_64-apple-darwin", f"MonHop_{VERSION}_x64.dmg", b"intel dmg"
        )
        self.intel_archive = self.asset("monhop-x86_64-apple-darwin", "MonHop.app.tar.gz", b"intel archive")
        self.intel_signature = self.asset("monhop-x86_64-apple-darwin", "MonHop.app.tar.gz.sig", b"intel-signature\n")
        self.windows_setup = self.asset(
            "monhop-x86_64-pc-windows-msvc", f"MonHop_{VERSION}_x64-setup.exe", b"windows setup"
        )
        self.windows_signature = self.asset(
            "monhop-x86_64-pc-windows-msvc", f"MonHop_{VERSION}_x64-setup.exe.sig", b"windows-signature\n"
        )

    def assemble(self, output=None):
        assemble_release.assemble_release(
            self.artifacts,
            output or self.output,
            VERSION,
            self.notes,
            REPOSITORY,
            now=datetime(2026, 9, 14, 12, 34, 56, tzinfo=timezone.utc),
        )

    def test_assembles_exact_assets_and_a_tag_pinned_feed(self):
        self.output.mkdir()
        self.assemble()

        expected = {
            f"MonHop_{VERSION}_aarch64.dmg": b"arm dmg",
            f"MonHop_{VERSION}_aarch64.app.tar.gz": b"arm archive",
            f"MonHop_{VERSION}_aarch64.app.tar.gz.sig": b"arm-signature\n",
            f"MonHop_{VERSION}_x64.dmg": b"intel dmg",
            f"MonHop_{VERSION}_x64.app.tar.gz": b"intel archive",
            f"MonHop_{VERSION}_x64.app.tar.gz.sig": b"intel-signature\n",
            f"MonHop_{VERSION}_x64-setup.exe": b"windows setup",
            f"MonHop_{VERSION}_x64-setup.exe.sig": b"windows-signature\n",
        }
        self.assertEqual(
            {path.name: path.read_bytes() for path in self.output.iterdir() if path.name != "latest.json"},
            expected,
        )

        feed = json.loads((self.output / "latest.json").read_text(encoding="utf-8"))
        self.assertEqual(feed["version"], VERSION)
        self.assertEqual(feed["notes"], "### Fixed\n- Safer updates.\n")
        self.assertEqual(feed["pub_date"], "2026-09-14T12:34:56Z")
        expected_platforms = {
            "darwin-aarch64": (f"MonHop_{VERSION}_aarch64.app.tar.gz", "arm-signature"),
            "darwin-aarch64-app": (f"MonHop_{VERSION}_aarch64.app.tar.gz", "arm-signature"),
            "darwin-x86_64": (f"MonHop_{VERSION}_x64.app.tar.gz", "intel-signature"),
            "darwin-x86_64-app": (f"MonHop_{VERSION}_x64.app.tar.gz", "intel-signature"),
            "windows-x86_64": (f"MonHop_{VERSION}_x64-setup.exe", "windows-signature"),
            "windows-x86_64-nsis": (f"MonHop_{VERSION}_x64-setup.exe", "windows-signature"),
        }
        self.assertEqual(set(feed["platforms"]), set(expected_platforms))
        for platform, (filename, signature) in expected_platforms.items():
            self.assertEqual(
                feed["platforms"][platform],
                {
                    "url": f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/{filename}",
                    "signature": signature,
                },
            )

    def test_every_installer_updater_and_signature_is_required(self):
        required = (
            self.arm_dmg, self.arm_archive, self.arm_signature,
            self.intel_dmg, self.intel_archive, self.intel_signature,
            self.windows_setup, self.windows_signature,
        )
        for asset in required:
            with self.subTest(asset=str(asset.relative_to(self.artifacts))):
                original = asset.read_bytes()
                asset.unlink()
                try:
                    with self.assertRaises(assemble_release.AssemblyError):
                        self.assemble()
                    self.assertFalse(self.output.exists())
                finally:
                    asset.write_bytes(original)

    def test_cli_assembles_from_nested_artifact_paths(self):
        result = subprocess.run(
            [
                sys.executable,
                str(MODULE_PATH),
                "--artifacts",
                str(self.artifacts),
                "--output",
                str(self.output),
                "--version",
                VERSION,
                "--notes-file",
                str(self.notes),
                "--repository",
                REPOSITORY,
            ],
            capture_output=True,
            check=False,
            text=True,
        )

        self.assertEqual((result.returncode, result.stdout), (0, f"assembled release assets for v{VERSION}\n"))
        self.assertEqual((self.output / f"MonHop_{VERSION}_x64-setup.exe").read_bytes(), b"windows setup")
        self.assertEqual(json.loads((self.output / "latest.json").read_text(encoding="utf-8"))["version"], VERSION)

    def test_missing_empty_duplicate_symlink_and_invalid_signature_are_refused_before_copying(self):
        cases = []

        missing = self.root / "missing"
        shutil.copytree(self.artifacts, missing)
        (missing / "monhop-aarch64-apple-darwin" / "nested" / "bundle" / "MonHop.app.tar.gz").unlink()
        cases.append((missing, "missing"))

        empty = self.root / "empty"
        shutil.copytree(self.artifacts, empty)
        (empty / "monhop-x86_64-pc-windows-msvc" / "nested" / "bundle" / f"MonHop_{VERSION}_x64-setup.exe").write_bytes(b"")
        cases.append((empty, "empty"))

        duplicate = self.root / "duplicate"
        shutil.copytree(self.artifacts, duplicate)
        duplicate_path = duplicate / "monhop-x86_64-apple-darwin" / "another" / "MonHop.app.tar.gz"
        duplicate_path.parent.mkdir()
        duplicate_path.write_bytes(b"duplicate")
        cases.append((duplicate, "duplicate"))

        invalid_signature = self.root / "invalid-signature"
        shutil.copytree(self.artifacts, invalid_signature)
        (invalid_signature / "monhop-aarch64-apple-darwin" / "nested" / "bundle" / "MonHop.app.tar.gz.sig").write_bytes(b"\xff")
        cases.append((invalid_signature, "invalid UTF-8 signature"))

        for artifacts, name in cases:
            output = self.root / f"output-{name}"
            with self.subTest(name=name):
                with self.assertRaises(assemble_release.AssemblyError):
                    assemble_release.assemble_release(artifacts, output, VERSION, self.notes, REPOSITORY)
                self.assertFalse(output.exists())

        symlink = self.root / "symlink"
        shutil.copytree(self.artifacts, symlink)
        source = symlink / "monhop-x86_64-pc-windows-msvc" / "nested" / "bundle" / f"MonHop_{VERSION}_x64-setup.exe"
        linked = source.with_name("linked.exe")
        source.rename(linked)
        source.symlink_to(linked.name)
        output = self.root / "output-symlink"
        with self.assertRaises(assemble_release.AssemblyError):
            assemble_release.assemble_release(symlink, output, VERSION, self.notes, REPOSITORY)
        self.assertFalse(output.exists())

    def test_wrong_version_stale_output_and_invalid_repository_are_refused(self):
        wrong_version = self.root / "wrong-version"
        shutil.copytree(self.artifacts, wrong_version)
        with self.assertRaises(assemble_release.AssemblyError):
            assemble_release.assemble_release(wrong_version, self.output, "1.2.4", self.notes, REPOSITORY)
        self.assertFalse(self.output.exists())

        self.output.mkdir()
        stale = self.output / "old-release.txt"
        stale.write_text("old", encoding="utf-8")
        with self.assertRaises(assemble_release.AssemblyError):
            assemble_release.assemble_release(self.artifacts, self.output, VERSION, self.notes, REPOSITORY)
        self.assertEqual(list(self.output.iterdir()), [stale])

        invalid_output = self.root / "invalid-repository-output"
        with self.assertRaises(assemble_release.AssemblyError):
            assemble_release.assemble_release(self.artifacts, invalid_output, VERSION, self.notes, "https://github.com/MannyGozzi/monhop")
        self.assertFalse(invalid_output.exists())


if __name__ == "__main__":
    unittest.main()
