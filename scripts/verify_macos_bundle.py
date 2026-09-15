#!/usr/bin/env python3
"""Verify the signed macOS bundle permissions required for Wi-Fi recognition."""

import argparse
import plistlib
from pathlib import Path
import re
import subprocess
import sys
from typing import Callable, Optional, Sequence


IDENTIFIER = "com.manuelgozzi.monhop"
LOCATION_ENTITLEMENT = "com.apple.security.personal-information.location"
RUNTIME_FLAG = 0x10000


class BundleVerificationError(RuntimeError):
    pass


Runner = Callable[[Sequence[str]], subprocess.CompletedProcess]


def run_codesign(command: Sequence[str], runner: Runner, purpose: str) -> subprocess.CompletedProcess:
    try:
        result = runner(command)
    except (OSError, subprocess.SubprocessError) as error:
        raise BundleVerificationError(f"codesign {purpose} failed.") from error
    if result.returncode != 0:
        raise BundleVerificationError(f"codesign {purpose} failed.")
    return result


def command_runner(command: Sequence[str]) -> subprocess.CompletedProcess:
    return subprocess.run(command, capture_output=True, check=False, text=True)


def read_info(bundle: Path) -> dict:
    try:
        with (bundle / "Contents" / "Info.plist").open("rb") as source:
            info = plistlib.load(source)
    except (OSError, plistlib.InvalidFileException) as error:
        raise BundleVerificationError("app bundle has no readable Info.plist.") from error
    if not isinstance(info, dict):
        raise BundleVerificationError("app bundle Info.plist is not a dictionary.")
    return info


def require_usage_description(info: dict, name: str) -> None:
    if not isinstance(info.get(name), str) or not info[name].strip():
        raise BundleVerificationError(f"app bundle {name} must be a nonempty string.")


def output(result: subprocess.CompletedProcess) -> str:
    return "\n".join(part for part in (result.stdout, result.stderr) if isinstance(part, str))


def require_hardened_runtime(metadata: str) -> None:
    match = re.search(r"\bflags=0x([0-9a-fA-F]+)\b", metadata)
    if match is None:
        raise BundleVerificationError("codesign metadata has no flags value.")
    if int(match.group(1), 16) & RUNTIME_FLAG == 0:
        raise BundleVerificationError("app bundle is missing the hardened runtime flag.")


def entitlement_plist(result: subprocess.CompletedProcess) -> dict:
    for candidate in (result.stdout, result.stderr):
        if not isinstance(candidate, str):
            continue
        start = candidate.find("<?xml")
        if start < 0:
            start = candidate.find("<plist")
        end = candidate.rfind("</plist>")
        if start < 0 or end < start:
            continue
        try:
            value = plistlib.loads(candidate[start : end + len("</plist>")].encode("utf-8"))
        except plistlib.InvalidFileException:
            continue
        if isinstance(value, dict):
            return value
    raise BundleVerificationError("signed app has no readable entitlements.")


def verify_bundle(bundle: Path, runner: Runner = command_runner) -> None:
    info = read_info(bundle)
    if info.get("CFBundleIdentifier") != IDENTIFIER:
        raise BundleVerificationError(f"app bundle identifier must be {IDENTIFIER}.")
    require_usage_description(info, "NSLocationUsageDescription")
    require_usage_description(info, "NSLocationWhenInUseUsageDescription")

    target = str(bundle)
    run_codesign(("codesign", "--verify", "--deep", "--strict", target), runner, "verification")
    metadata = run_codesign(("codesign", "-dvvv", target), runner, "metadata")
    require_hardened_runtime(output(metadata))
    entitlements = run_codesign(
        ("codesign", "--display", "--entitlements", "-", "--xml", target), runner, "entitlement display"
    )
    if entitlement_plist(entitlements).get(LOCATION_ENTITLEMENT) is not True:
        raise BundleVerificationError("signed app location entitlement must be true.")


def main(arguments: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Verify required macOS bundle metadata and code-signing properties.")
    parser.add_argument("bundle", type=Path, metavar="MonHop.app")
    args = parser.parse_args(arguments)
    try:
        verify_bundle(args.bundle)
    except BundleVerificationError as error:
        print(f"macOS bundle verification failed: {error}", file=sys.stderr)
        return 1
    print("macOS bundle verification passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
