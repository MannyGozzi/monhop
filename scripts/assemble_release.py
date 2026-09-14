#!/usr/bin/env python3
"""Assemble the complete assets and updater feed for one MonHop release."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shutil
import sys
from typing import Dict, Optional, Sequence, Tuple


SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
REPOSITORY = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$")


class AssemblyError(RuntimeError):
    """Raised when artifacts cannot make a complete release."""


FileMappings = Tuple[Tuple[str, str], ...]
Target = Tuple[str, FileMappings]


def mac_target(architecture: str) -> Target:
    return (
        f"monhop-{architecture}-apple-darwin",
        (
            (f"MonHop_{{version}}_{'aarch64' if architecture == 'aarch64' else 'x64'}.dmg", ""),
            ("MonHop.app.tar.gz", f"MonHop_{{version}}_{'aarch64' if architecture == 'aarch64' else 'x64'}.app.tar.gz"),
            ("MonHop.app.tar.gz.sig", f"MonHop_{{version}}_{'aarch64' if architecture == 'aarch64' else 'x64'}.app.tar.gz.sig"),
        ),
    )


TARGETS = (
    mac_target("aarch64"),
    mac_target("x86_64"),
    (
        "monhop-x86_64-pc-windows-msvc",
        (
            ("MonHop_{version}_x64-setup.exe", ""),
            ("MonHop_{version}_x64-setup.exe.sig", ""),
        ),
    ),
)


def require_directory(path: Path, label: str) -> None:
    if path.is_symlink():
        raise AssemblyError(f"{label} must not be a symlink: {path}")
    if not path.is_dir():
        raise AssemblyError(f"{label} is not a directory: {path}")


def validate_output(output: Path) -> None:
    if output.is_symlink():
        raise AssemblyError(f"output must not be a symlink: {output}")
    if output.exists():
        require_directory(output, "output")
        if any(output.iterdir()):
            raise AssemblyError(f"output directory is not empty: {output}")
    elif not output.parent.is_dir():
        raise AssemblyError(f"output parent is not a directory: {output.parent}")


def find_asset(directory: Path, name: str) -> Path:
    matches = []
    for current, _, files in os.walk(directory, followlinks=False):
        for filename in files:
            if filename == name:
                matches.append(Path(current) / filename)
    if not matches:
        raise AssemblyError(f"missing required asset {name} in {directory}")
    if len(matches) != 1:
        raise AssemblyError(f"required asset {name} is duplicated in {directory}")

    path = matches[0]
    if path.is_symlink():
        raise AssemblyError(f"required asset must not be a symlink: {path}")
    if not path.is_file():
        raise AssemblyError(f"required asset is not a regular file: {path}")
    if path.stat().st_size == 0:
        raise AssemblyError(f"required asset is empty: {path}")
    return path


def read_utf8(path: Path, label: str) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise AssemblyError(f"{label} is not valid UTF-8: {path}") from error


def release_time(now: Optional[datetime]) -> str:
    current = now or datetime.now(timezone.utc)
    return current.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def expected_files(version: str) -> Tuple[Target, ...]:
    targets = []
    for directory, target_files in TARGETS:
        files = []
        for source, destination in target_files:
            source = source.format(version=version)
            files.append((source, destination.format(version=version) if destination else source))
        targets.append((directory, tuple(files)))
    return tuple(targets)


def validate_inputs(
    artifacts: Path,
    output: Path,
    version: str,
    notes_file: Path,
    repository: str,
) -> Tuple[Dict[str, Path], Dict[str, str], str]:
    if SEMVER.fullmatch(version) is None:
        raise AssemblyError(f"version must be strict X.Y.Z: {version!r}")
    if REPOSITORY.fullmatch(repository) is None:
        raise AssemblyError(f"repository must be owner/repo: {repository!r}")
    require_directory(artifacts, "artifacts")
    validate_output(output)
    notes = read_utf8(notes_file, "notes file")

    assets: Dict[str, Path] = {}
    signatures: Dict[str, str] = {}
    for directory, files in expected_files(version):
        target_directory = artifacts / directory
        require_directory(target_directory, "artifact directory")
        for source, destination in files:
            asset = find_asset(target_directory, source)
            assets[destination] = asset
            if source.endswith(".sig"):
                signature = read_utf8(asset, "signature").strip()
                if not signature:
                    raise AssemblyError(f"signature is empty: {asset}")
                signatures[destination] = signature
    return assets, signatures, notes


def platform_entry(repository: str, version: str, filename: str, signature: str) -> Dict[str, str]:
    return {
        "url": f"https://github.com/{repository}/releases/download/v{version}/{filename}",
        "signature": signature,
    }


def latest_feed(
    repository: str,
    version: str,
    notes: str,
    signatures: Dict[str, str],
    now: Optional[datetime],
) -> Dict[str, object]:
    arm = f"MonHop_{version}_aarch64.app.tar.gz"
    intel = f"MonHop_{version}_x64.app.tar.gz"
    windows = f"MonHop_{version}_x64-setup.exe"
    return {
        "version": version,
        "notes": notes,
        "pub_date": release_time(now),
        "platforms": {
            "darwin-aarch64": platform_entry(repository, version, arm, signatures[f"{arm}.sig"]),
            "darwin-aarch64-app": platform_entry(repository, version, arm, signatures[f"{arm}.sig"]),
            "darwin-x86_64": platform_entry(repository, version, intel, signatures[f"{intel}.sig"]),
            "darwin-x86_64-app": platform_entry(repository, version, intel, signatures[f"{intel}.sig"]),
            "windows-x86_64": platform_entry(repository, version, windows, signatures[f"{windows}.sig"]),
            "windows-x86_64-nsis": platform_entry(repository, version, windows, signatures[f"{windows}.sig"]),
        },
    }


def assemble_release(
    artifacts: Path,
    output: Path,
    version: str,
    notes_file: Path,
    repository: str,
    now: Optional[datetime] = None,
) -> None:
    """Validate all inputs, then copy known assets and write one updater feed."""
    assets, signatures, notes = validate_inputs(artifacts, output, version, notes_file, repository)
    if not output.exists():
        output.mkdir()
    for destination, source in assets.items():
        shutil.copyfile(source, output / destination)
    feed = latest_feed(repository, version, notes, signatures, now)
    (output / "latest.json").write_text(json.dumps(feed, indent=2) + "\n", encoding="utf-8")


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Assemble MonHop release assets and latest.json.")
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--notes-file", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    try:
        assemble_release(args.artifacts, args.output, args.version, args.notes_file, args.repository)
    except AssemblyError as error:
        print(f"release assembly failed: {error}", file=sys.stderr)
        return 1
    print(f"assembled release assets for v{args.version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
