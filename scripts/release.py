#!/usr/bin/env python3
"""Cut a MonHop release from the current checkout.

Bumps the workspace version and the exact pins between workspace crates, refreshes the
lockfile and the generated dependency reports, moves the changelog's Unreleased section
under the new version, then commits and tags. Pushing the tag is what starts the release
workflow, so it stays a separate, explicit step.
"""

from __future__ import annotations

import argparse
from datetime import date
import importlib.util
from pathlib import Path
import re
import subprocess
import sys
from typing import List, Optional, Sequence, Tuple


REPO_URL = "https://github.com/MannyGozzi/monhop"
BUMPS = ("patch", "minor", "major")
SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
VERSION_LINE = re.compile(r'^version\s*=\s*"([^"]+)"$')
# Only an exact pin on a sibling workspace crate moves; third-party pins are left alone.
PIN = re.compile(r'(?m)^(?P<head>\s*monhop[A-Za-z0-9_-]*\s*=\s*\{[^\n}]*?version\s*=\s*")=(?P<version>[^"]*)(?P<tail>")')
REPORT_DIRECTORY = "docs/dependencies"


class ReleaseError(RuntimeError):
    """Raised when the checkout cannot produce a release."""


class Runner:
    """Every subprocess a release makes; tests replace it with a recording fake."""

    def __init__(self, workspace: Path) -> None:
        self.workspace = workspace

    def run(self, command: Sequence[str]) -> str:
        result = subprocess.run(
            list(command),
            cwd=self.workspace,
            capture_output=True,
            check=False,
            text=True,
            encoding="utf-8",
        )
        if result.returncode:
            detail = (result.stderr or result.stdout).strip().splitlines()
            raise ReleaseError(f"{' '.join(command)} failed: {detail[-1] if detail else 'no output'}")
        return result.stdout

    def git(self, *arguments: str) -> str:
        return self.run(["git", *arguments])

    def cargo(self, *arguments: str) -> str:
        return self.run(["cargo", *arguments])

    def dependency_reports(self, *arguments: str) -> str:
        script = self.workspace / "scripts" / "dependencies.py"
        return self.run([sys.executable, str(script), *arguments])


def load_changelog():
    """Imported late so --help still works before the changelog module lands."""
    path = Path(__file__).resolve().with_name("changelog.py")
    spec = importlib.util.spec_from_file_location("changelog", path)
    if spec is None or spec.loader is None:
        raise ReleaseError("scripts/changelog.py is unavailable")
    module = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(module)
    except (FileNotFoundError, OSError) as error:
        raise ReleaseError("scripts/changelog.py is unavailable") from error
    return module


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (FileNotFoundError, OSError) as error:
        raise ReleaseError(f"{path.name} is unavailable") from error


def write_text(path: Path, text: str) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def parse_version(version: str) -> Tuple[int, int, int]:
    match = SEMVER.match(version)
    if match is None:
        raise ReleaseError(f"'{version}' is not a patch, minor, major, or X.Y.Z version")
    major, minor, patch = (int(part) for part in match.groups())
    return (major, minor, patch)


def next_version(current: str, bump: str) -> str:
    """Compute the release version; an explicit one must still move forwards."""
    major, minor, patch = parse_version(current)
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    if parse_version(bump) <= (major, minor, patch):
        raise ReleaseError(f"{bump} is not greater than the current {current}")
    return bump


def workspace_version_line(text: str) -> Tuple[List[str], int, str]:
    """The first bare version line inside [workspace.package]; the next table ends the search."""
    lines = text.split("\n")
    inside = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("["):
            inside = stripped == "[workspace.package]"
            continue
        match = VERSION_LINE.match(stripped)
        if inside and match is not None:
            return lines, index, match.group(1)
    raise ReleaseError("Cargo.toml has no [workspace.package] version")


def read_workspace_version(text: str) -> str:
    return workspace_version_line(text)[2]


def rewrite_workspace_version(text: str, version: str) -> str:
    lines, index, _ = workspace_version_line(text)
    line = lines[index]
    indent = line[: len(line) - len(line.lstrip())]
    lines[index] = f'{indent}version = "{version}"'
    return "\n".join(lines)


def rewrite_pins(text: str, version: str) -> Tuple[str, int]:
    changed = 0

    def replace(match: "re.Match[str]") -> str:
        nonlocal changed
        if match.group("version") == version:
            return match.group(0)
        changed += 1
        return f'{match.group("head")}={version}{match.group("tail")}'

    return PIN.sub(replace, text), changed


def manifest_paths(workspace: Path) -> List[Path]:
    return sorted(workspace.glob("apps/*/Cargo.toml")) + sorted(workspace.glob("crates/*/Cargo.toml"))


def pin_rewrites(workspace: Path, version: str) -> Tuple[List[Tuple[Path, str]], int]:
    """Plan every pin rewrite before anything is written, so a dry run can print the files."""
    rewrites: List[Tuple[Path, str]] = []
    pins = 0
    for path in manifest_paths(workspace):
        updated, changed = rewrite_pins(read_text(path), version)
        if changed:
            rewrites.append((path, updated))
            pins += changed
    return rewrites, pins


def relative(workspace: Path, path: Path) -> str:
    return path.relative_to(workspace).as_posix()


def require_clean_main(runner: Runner) -> None:
    branch = runner.git("rev-parse", "--abbrev-ref", "HEAD").strip()
    if branch != "main":
        raise ReleaseError(f"a release is cut from main, not from {branch}")
    if runner.git("status", "--porcelain").strip():
        raise ReleaseError("the working tree has uncommitted changes; commit or stash them first")
    runner.git("fetch", "origin", "main")
    if runner.git("rev-parse", "HEAD").strip() != runner.git("rev-parse", "FETCH_HEAD").strip():
        raise ReleaseError("HEAD is not origin/main; pull or push before releasing")


def require_absent_tag(runner: Runner, tag: str) -> None:
    if runner.git("tag", "--list", tag).strip():
        raise ReleaseError(f"{tag} already exists in this checkout")
    remote = {f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"}
    for line in runner.git("ls-remote", "--tags", "origin").splitlines():
        if line.split("\t")[-1].strip() in remote:
            raise ReleaseError(f"{tag} already exists on origin")


def changed_report_paths(runner: Runner, workspace: Path) -> List[str]:
    """NUL records avoid shell quoting; a rename's second field names a path that is gone."""
    output = runner.git("status", "--porcelain", "-z", "--", REPORT_DIRECTORY)
    paths = []
    for record in output.split("\0"):
        if len(record) > 3 and record[2] == " " and (workspace / record[3:]).is_file():
            paths.append(record[3:])
    return sorted(paths)


def print_plan(
    workspace: Path,
    current: str,
    version: str,
    today: str,
    rewrites: Sequence[Tuple[Path, str]],
    pins: int,
) -> None:
    print("MonHop release plan")
    print(f"  version  {current} -> {version}")
    print(f"  tag      v{version}")
    print(f"  dated    {today}")
    print("  files")
    print("    Cargo.toml")
    for path, _ in rewrites:
        print(f"    {relative(workspace, path)}")
    print("    Cargo.lock (refreshed offline)")
    print(f"    {REPORT_DIRECTORY} (regenerated)")
    print("    CHANGELOG.md")
    print(f"  {pins} exact pins between workspace crates move to {version}")


def run_release(
    workspace: Path,
    bump: str,
    push: bool = False,
    dry_run: bool = False,
    runner: Optional[Runner] = None,
    changelog=None,
    today: Optional[str] = None,
) -> int:
    runner = runner or Runner(workspace)
    today = today or date.today().isoformat()
    manifest_path = workspace / "Cargo.toml"
    changelog_path = workspace / "CHANGELOG.md"

    require_clean_main(runner)
    manifest = read_text(manifest_path)
    current = read_workspace_version(manifest)
    changelog_text = read_text(changelog_path)
    changelog = changelog or load_changelog()
    try:
        changelog.check(changelog_text)
    except ValueError as error:
        raise ReleaseError(f"CHANGELOG.md is not ready to release: {error}") from error
    version = next_version(current, bump)
    tag = f"v{version}"
    require_absent_tag(runner, tag)
    rewrites, pins = pin_rewrites(workspace, version)

    if dry_run:
        print_plan(workspace, current, version, today, rewrites, pins)
        print("Dry run: nothing was written.")
        return 0

    write_text(manifest_path, rewrite_workspace_version(manifest, version))
    for path, text in rewrites:
        write_text(path, text)
    runner.cargo("update", "--workspace", "--offline")
    runner.dependency_reports()
    runner.dependency_reports("--check")
    write_text(changelog_path, changelog.cut(changelog_text, version, today, REPO_URL))

    staged = [
        "Cargo.toml",
        "Cargo.lock",
        *(relative(workspace, path) for path, _ in rewrites),
        "CHANGELOG.md",
        *changed_report_paths(runner, workspace),
    ]
    runner.git("add", "--", *staged)
    runner.git("commit", "-m", f"Release {tag}")
    runner.git("tag", "-a", tag, "-m", f"MonHop {tag}")
    print(f"Committed and tagged {tag} ({pins} pins rewritten, {len(staged)} files staged).")

    if push:
        runner.git("push", "origin", "main")
        runner.git("push", "origin", tag)
        print(f"Pushed main and {tag}; the release workflow takes it from here.")
    else:
        print("Nothing was pushed. To publish this release run:")
        print("  git push origin main")
        print(f"  git push origin {tag}")
    return 0


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Cut a MonHop release from the current checkout.")
    parser.add_argument("bump", help=f"{', '.join(BUMPS)}, or an explicit X.Y.Z version")
    parser.add_argument("--push", action="store_true", help="push main and the new tag to origin")
    parser.add_argument("--dry-run", action="store_true", help="check the preconditions and print the plan")
    parser.add_argument("--workspace", type=Path, default=Path(__file__).resolve().parents[1], help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None, runner: Optional[Runner] = None, changelog=None) -> int:
    args = parse_args(argv)
    try:
        return run_release(
            args.workspace.resolve(),
            args.bump,
            push=args.push,
            dry_run=args.dry_run,
            runner=runner,
            changelog=changelog,
        )
    except ReleaseError as error:
        print(f"release failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
