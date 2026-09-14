import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "changelog.py"
SPEC = importlib.util.spec_from_file_location("changelog", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
changelog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(changelog)


REPO = "https://github.com/MannyGozzi/monhop"

SAMPLE = """# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

### Added
- Something.

### Fixed
- Something else.

## [0.1.0] - 2026-09-14

### Added
- First release.

[Unreleased]: {repo}/compare/v0.1.0...HEAD
[0.1.0]: {repo}/releases/tag/v0.1.0
""".format(repo=REPO)

EMPTY = """# Changelog

## [Unreleased]

## [0.1.0] - 2026-09-14

### Added
- First release.

[Unreleased]: {repo}/compare/v0.1.0...HEAD
[0.1.0]: {repo}/releases/tag/v0.1.0
""".format(repo=REPO)

CUT = """# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

## [0.2.0] - 2026-09-20

### Added
- Something.

### Fixed
- Something else.

## [0.1.0] - 2026-09-14

### Added
- First release.

[Unreleased]: {repo}/compare/v0.2.0...HEAD
[0.2.0]: {repo}/releases/tag/v0.2.0
[0.1.0]: {repo}/releases/tag/v0.1.0
""".format(repo=REPO)


class NotesTest(unittest.TestCase):
    def test_version_section_excludes_the_heading_and_the_link_block(self):
        self.assertEqual(
            changelog.notes(SAMPLE, "0.1.0"),
            "### Added\n- First release.",
        )

    def test_version_may_carry_a_leading_v(self):
        self.assertEqual(changelog.notes(SAMPLE, "v0.1.0"), changelog.notes(SAMPLE, "0.1.0"))

    def test_unreleased_section_stops_at_the_next_version(self):
        self.assertEqual(
            changelog.notes(SAMPLE, "unreleased"),
            "### Added\n- Something.\n\n### Fixed\n- Something else.",
        )

    def test_missing_section_is_an_error(self):
        with self.assertRaises(ValueError):
            changelog.notes(SAMPLE, "9.9.9")


class CheckTest(unittest.TestCase):
    def test_recorded_changes_pass(self):
        self.assertIsNone(changelog.check(SAMPLE))

    def test_empty_unreleased_is_an_error(self):
        with self.assertRaises(ValueError):
            changelog.check(EMPTY)


class CutTest(unittest.TestCase):
    def test_cut_dates_unreleased_and_reorders_the_links(self):
        self.assertEqual(changelog.cut(SAMPLE, "0.2.0", "2026-09-20", REPO), CUT)

    def test_cut_ignores_a_trailing_slash_on_the_repository_url(self):
        self.assertEqual(changelog.cut(SAMPLE, "0.2.0", "2026-09-20", REPO + "/"), CUT)

    def test_cutting_an_existing_version_is_an_error(self):
        with self.assertRaises(ValueError):
            changelog.cut(SAMPLE, "0.1.0", "2026-09-20", REPO)


class CommandLineTest(unittest.TestCase):
    def changelog_file(self, text):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "CHANGELOG.md"
        path.write_text(text, encoding="utf-8")
        return path

    def test_notes_prints_the_section(self):
        path = self.changelog_file(SAMPLE)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = changelog.main(["notes", "v0.1.0", "--file", str(path)])
        self.assertEqual((code, out.getvalue()), (0, "### Added\n- First release.\n"))

    def test_check_reports_an_empty_unreleased_on_stderr(self):
        path = self.changelog_file(EMPTY)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = changelog.main(["check", "--file", str(path)])
        self.assertEqual(code, 1)
        self.assertIn("Unreleased", err.getvalue())

    def test_cut_rewrites_the_file_in_place(self):
        path = self.changelog_file(SAMPLE)
        code = changelog.main(
            ["cut", "0.2.0", "2026-09-20", "--repo-url", REPO, "--file", str(path)]
        )
        self.assertEqual((code, path.read_text(encoding="utf-8")), (0, CUT))


if __name__ == "__main__":
    unittest.main()
