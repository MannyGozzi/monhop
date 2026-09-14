import contextlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "release.py"
SPEC = importlib.util.spec_from_file_location("release", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


WORKSPACE_MANIFEST = """[workspace]
members = ["crates/monhop-core", "apps/monhop-desktop"]

[workspace.metadata.example]
version = "9.9.9"

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
log = { version = "=0.4.34" }
"""

DESKTOP_MANIFEST = """[package]
name = "monhop-desktop"
version.workspace = true

[dependencies]
monhop-core = { path = "../../crates/monhop-core", version = "=0.1.0" }
monhop-transport = { version = "=0.1.0", path = "../../crates/monhop-transport" }
other-crate = { version = "=0.1.0", default-features = false }
tauri = "=2.11.5"
"""

CORE_MANIFEST = """[package]
name = "monhop-core"
version.workspace = true

[dependencies]
monhop-protocol = { path = "../monhop-protocol", version = "=0.1.0" }
"""

CHANGELOG = """# Changelog

## [Unreleased]

### Added

- Something a person notices.

[Unreleased]: https://github.com/MannyGozzi/monhop/compare/main...HEAD
"""


class ChangelogStub:
    """Stands in for scripts/changelog.py, which another script owns."""

    def __init__(self, ready=True):
        self.ready = ready
        self.cuts = []

    def notes(self, text, version):
        return "- Something a person notices.\n"

    def check(self, text):
        if not self.ready:
            raise ValueError("the Unreleased section has no entries")

    def cut(self, text, version, date, repo_url):
        self.cuts.append((version, date, repo_url))
        return f"# Changelog\n\n## [Unreleased]\n\n## [{version}] - {date}\n\n- cut\n\n[{version}]: {repo_url}\n"


class RecordingRunner(release.Runner):
    """Real git in the temporary repository; cargo and the report step only recorded."""

    def __init__(self, workspace, reports=()):
        super().__init__(workspace)
        self.commands = []
        self.reports = reports

    def git(self, *arguments):
        self.commands.append(("git",) + arguments)
        return super().git(*arguments)

    def cargo(self, *arguments):
        self.commands.append(("cargo",) + arguments)
        return ""

    def dependency_reports(self, *arguments):
        self.commands.append(("dependencies",) + arguments)
        if not arguments:
            for name, text in self.reports:
                (self.workspace / "docs" / "dependencies" / name).write_text(text, encoding="utf-8")
        return ""


class VersionTests(unittest.TestCase):
    def test_each_bump_moves_its_own_component(self):
        self.assertEqual(release.next_version("0.1.0", "patch"), "0.1.1")
        self.assertEqual(release.next_version("0.1.9", "minor"), "0.2.0")
        self.assertEqual(release.next_version("0.1.9", "major"), "1.0.0")
        self.assertEqual(release.next_version("1.2.3", "patch"), "1.2.4")

    def test_an_explicit_version_is_accepted_when_it_moves_forwards(self):
        self.assertEqual(release.next_version("0.1.0", "0.4.0"), "0.4.0")
        self.assertEqual(release.next_version("0.1.0", "0.1.1"), "0.1.1")

    def test_an_explicit_version_that_stands_still_or_goes_back_is_refused(self):
        for current, target in (("1.0.0", "1.0.0"), ("1.0.0", "0.9.9"), ("0.1.0", "0.1.0"), ("0.2.0", "0.1.9")):
            with self.assertRaises(release.ReleaseError):
                release.next_version(current, target)

    def test_a_malformed_version_is_refused(self):
        for target in ("1.2", "v1.2.3", "1.2.3-rc1", "1.2.3.4", "01.2.3", "next", ""):
            with self.assertRaises(release.ReleaseError):
                release.next_version("0.1.0", target)
        with self.assertRaises(release.ReleaseError):
            release.next_version("0.1", "patch")


class ManifestTests(unittest.TestCase):
    def test_the_workspace_version_is_read_and_rewritten_in_its_own_table(self):
        self.assertEqual(release.read_workspace_version(WORKSPACE_MANIFEST), "0.1.0")
        rewritten = release.rewrite_workspace_version(WORKSPACE_MANIFEST, "0.2.0")
        self.assertEqual(rewritten, WORKSPACE_MANIFEST.replace('version = "0.1.0"', 'version = "0.2.0"'))
        self.assertIn('version = "9.9.9"', rewritten)
        self.assertIn('log = { version = "=0.4.34" }', rewritten)

    def test_a_manifest_without_the_workspace_table_is_refused(self):
        with self.assertRaises(release.ReleaseError):
            release.read_workspace_version(CORE_MANIFEST)

    def test_only_workspace_crate_pins_are_rewritten(self):
        rewritten, count = release.rewrite_pins(DESKTOP_MANIFEST, "0.2.0")
        self.assertEqual(count, 2)
        self.assertEqual(
            rewritten,
            """[package]
name = "monhop-desktop"
version.workspace = true

[dependencies]
monhop-core = { path = "../../crates/monhop-core", version = "=0.2.0" }
monhop-transport = { version = "=0.2.0", path = "../../crates/monhop-transport" }
other-crate = { version = "=0.1.0", default-features = false }
tauri = "=2.11.5"
""",
        )

    def test_rewriting_pins_that_already_match_changes_nothing(self):
        rewritten, count = release.rewrite_pins(DESKTOP_MANIFEST, "0.1.0")
        self.assertEqual((rewritten, count), (DESKTOP_MANIFEST, 0))


class ReleaseRunTests(unittest.TestCase):
    def git(self, root, *arguments):
        subprocess.run(["git", *arguments], cwd=str(root), check=True, capture_output=True, text=True)

    def repository(self):
        root = Path(tempfile.mkdtemp(prefix="monhop-release-"))
        self.addCleanup(shutil.rmtree, str(root), ignore_errors=True)
        origin, work, hooks = root / "origin.git", root / "work", root / "hooks"
        for directory in (origin, work, hooks):
            directory.mkdir()
        self.git(origin, "init", "--bare", "-b", "main")
        self.git(work, "init", "-b", "main")
        for key, value in (
            ("user.email", "release@example.invalid"),
            ("user.name", "Release Test"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", str(hooks)),
        ):
            self.git(work, "config", key, value)

        files = {
            "Cargo.toml": WORKSPACE_MANIFEST,
            "Cargo.lock": 'version = 4\n\n[[package]]\nname = "monhop-core"\nversion = "0.1.0"\n',
            "CHANGELOG.md": CHANGELOG,
            "apps/monhop-desktop/Cargo.toml": DESKTOP_MANIFEST,
            "crates/monhop-core/Cargo.toml": CORE_MANIFEST,
            "docs/dependencies/LICENSES.md": "# Dependency license report\n",
        }
        for name, text in files.items():
            path = work / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        self.git(work, "add", "--", *files)
        self.git(work, "commit", "-m", "Initial")
        self.git(work, "remote", "add", "origin", str(origin))
        self.git(work, "push", "-u", "origin", "main")
        return work, origin

    def release(self, work, bump="patch", push=False, dry_run=False, runner=None, changelog=None):
        runner = runner or RecordingRunner(work)
        changelog = changelog or ChangelogStub()
        self.output = io.StringIO()
        with contextlib.redirect_stdout(self.output):
            code = release.run_release(
                work, bump, push=push, dry_run=dry_run, runner=runner, changelog=changelog, today="2026-09-14"
            )
        return code, runner, changelog

    def show(self, work, *arguments):
        result = subprocess.run(
            ["git", *arguments], cwd=str(work), check=True, capture_output=True, text=True
        )
        return result.stdout.strip()

    def test_a_release_rewrites_versions_commits_and_tags_without_pushing(self):
        work, origin = self.repository()
        runner = RecordingRunner(work, reports=(("LICENSES.md", "# Dependency license report\n\n| monhop-core | 0.1.1 |\n"),))
        code, runner, changelog = self.release(work, runner=runner)

        self.assertEqual(code, 0)
        self.assertEqual(release.read_workspace_version((work / "Cargo.toml").read_text()), "0.1.1")
        self.assertIn('version = "=0.1.1"', (work / "apps/monhop-desktop/Cargo.toml").read_text())
        self.assertIn('version = "=0.1.1"', (work / "crates/monhop-core/Cargo.toml").read_text())
        self.assertIn('other-crate = { version = "=0.1.0"', (work / "apps/monhop-desktop/Cargo.toml").read_text())
        self.assertEqual(changelog.cuts, [("0.1.1", "2026-09-14", release.REPO_URL)])
        self.assertIn("## [0.1.1] - 2026-09-14", (work / "CHANGELOG.md").read_text())

        self.assertIn(("cargo", "update", "--workspace", "--offline"), runner.commands)
        self.assertIn(("dependencies",), runner.commands)
        self.assertIn(("dependencies", "--check"), runner.commands)
        self.assertEqual(self.show(work, "log", "-1", "--format=%s"), "Release v0.1.1")
        self.assertEqual(self.show(work, "tag", "--list"), "v0.1.1")
        self.assertEqual(self.show(work, "cat-file", "-t", "v0.1.1"), "tag")
        self.assertEqual(self.show(work, "status", "--porcelain"), "")
        self.assertEqual(
            sorted(self.show(work, "show", "--name-only", "--format=", "HEAD").split()),
            [
                "CHANGELOG.md",
                "Cargo.toml",
                "apps/monhop-desktop/Cargo.toml",
                "crates/monhop-core/Cargo.toml",
                "docs/dependencies/LICENSES.md",
            ],
        )
        self.assertFalse([command for command in runner.commands if "push" in command])
        self.assertEqual(self.show(origin, "tag", "--list"), "")

    def test_push_sends_main_and_the_tag_to_origin(self):
        work, origin = self.repository()
        code, runner, _ = self.release(work, push=True)

        self.assertEqual(code, 0)
        self.assertIn(("git", "push", "origin", "main"), runner.commands)
        self.assertIn(("git", "push", "origin", "v0.1.1"), runner.commands)
        self.assertEqual(self.show(origin, "tag", "--list"), "v0.1.1")
        self.assertEqual(self.show(origin, "log", "-1", "--format=%s", "main"), "Release v0.1.1")

    def test_a_dry_run_checks_the_preconditions_and_writes_nothing(self):
        work, origin = self.repository()
        code, runner, changelog = self.release(work, bump="minor", dry_run=True)

        self.assertEqual(code, 0)
        self.assertIn("0.1.0 -> 0.2.0", self.output.getvalue())
        self.assertIn("v0.2.0", self.output.getvalue())
        self.assertIn("nothing was written", self.output.getvalue())
        self.assertEqual(changelog.cuts, [])
        self.assertEqual(release.read_workspace_version((work / "Cargo.toml").read_text()), "0.1.0")
        self.assertEqual(self.show(work, "status", "--porcelain"), "")
        self.assertEqual(self.show(work, "log", "-1", "--format=%s"), "Initial")
        self.assertEqual(self.show(work, "tag", "--list"), "")
        self.assertFalse([command for command in runner.commands if command[0] in ("cargo", "dependencies")])

    def test_a_release_off_main_is_refused(self):
        work, _ = self.repository()
        self.git(work, "checkout", "-b", "release-prep")
        with self.assertRaisesRegex(release.ReleaseError, "release-prep"):
            self.release(work)

    def test_a_dirty_working_tree_is_refused(self):
        work, _ = self.repository()
        (work / "CHANGELOG.md").write_text(CHANGELOG + "- stray\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "uncommitted"):
            self.release(work)

    def test_a_checkout_behind_origin_is_refused(self):
        work, _ = self.repository()
        (work / "docs/dependencies/LICENSES.md").write_text("# newer\n", encoding="utf-8")
        self.git(work, "commit", "-am", "Later work")
        self.git(work, "push", "origin", "main")
        self.git(work, "reset", "--hard", "HEAD~1")
        with self.assertRaisesRegex(release.ReleaseError, "origin/main"):
            self.release(work)

    def test_an_empty_unreleased_section_is_refused(self):
        work, _ = self.repository()
        with self.assertRaisesRegex(release.ReleaseError, "CHANGELOG.md is not ready"):
            self.release(work, changelog=ChangelogStub(ready=False))

    def test_a_tag_that_already_exists_locally_is_refused(self):
        work, _ = self.repository()
        self.git(work, "tag", "v0.1.1")
        with self.assertRaisesRegex(release.ReleaseError, "already exists in this checkout"):
            self.release(work)

    def test_a_tag_that_already_exists_on_origin_is_refused(self):
        work, _ = self.repository()
        self.git(work, "tag", "v0.1.1")
        self.git(work, "push", "origin", "v0.1.1")
        self.git(work, "tag", "-d", "v0.1.1")
        with self.assertRaisesRegex(release.ReleaseError, "already exists on origin"):
            self.release(work)

    def test_main_reports_a_refused_release_as_a_failure(self):
        work, _ = self.repository()
        self.git(work, "checkout", "-b", "release-prep")
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors):
            code = release.main(
                ["patch", "--workspace", str(work)], runner=RecordingRunner(work), changelog=ChangelogStub()
            )
        self.assertEqual(code, 1)
        self.assertIn("release-prep", errors.getvalue())

    def test_main_cuts_a_release_from_its_arguments(self):
        work, _ = self.repository()
        runner, changelog = RecordingRunner(work), ChangelogStub()
        with contextlib.redirect_stdout(io.StringIO()):
            code = release.main(["minor", "--workspace", str(work)], runner=runner, changelog=changelog)

        self.assertEqual(code, 0)
        self.assertEqual(release.read_workspace_version((work / "Cargo.toml").read_text()), "0.2.0")
        self.assertEqual(self.show(work, "tag", "--list"), "v0.2.0")
        self.assertEqual(self.show(work, "log", "-1", "--format=%s"), "Release v0.2.0")


if __name__ == "__main__":
    unittest.main()
