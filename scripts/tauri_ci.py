#!/usr/bin/env python3
"""Run Tauri in CI without passing empty optional Apple credentials."""

from __future__ import annotations

import os
import subprocess
import sys
from typing import Callable, Mapping, MutableMapping, Optional, Sequence


CERTIFICATE = "APPLE_CERTIFICATE"
CERTIFICATE_PASSWORD = "APPLE_CERTIFICATE_PASSWORD"
SIGNING_IDENTITY = "APPLE_SIGNING_IDENTITY"
NOTARIZATION = ("APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID")
APPLE_CREDENTIALS = (CERTIFICATE, CERTIFICATE_PASSWORD, SIGNING_IDENTITY, *NOTARIZATION)


class ConfigurationError(RuntimeError):
    """Raised when optional Apple credentials are only partly configured."""


def configured(environment: Mapping[str, str], name: str) -> bool:
    return bool(environment.get(name))


def tauri_environment(environment: Mapping[str, str]) -> MutableMapping[str, str]:
    """Keep nonempty Apple settings, while keeping unencrypted certificate passwords explicit."""
    cleaned = {
        name: value
        for name, value in environment.items()
        if not (name.startswith("APPLE_") and not value)
    }
    for name in APPLE_CREDENTIALS:
        cleaned.pop(name, None)

    certificate = configured(environment, CERTIFICATE)
    identity = configured(environment, SIGNING_IDENTITY)
    password = environment.get(CERTIFICATE_PASSWORD, "")
    if certificate != identity:
        raise ConfigurationError("APPLE_CERTIFICATE and APPLE_SIGNING_IDENTITY must be set together")
    if password and not certificate:
        raise ConfigurationError("APPLE_CERTIFICATE_PASSWORD requires APPLE_CERTIFICATE and APPLE_SIGNING_IDENTITY")
    if certificate:
        cleaned[CERTIFICATE] = environment[CERTIFICATE]
        cleaned[CERTIFICATE_PASSWORD] = password
        cleaned[SIGNING_IDENTITY] = environment[SIGNING_IDENTITY]

    notarization = [configured(environment, name) for name in NOTARIZATION]
    if any(notarization) and not all(notarization):
        raise ConfigurationError("APPLE_ID, APPLE_PASSWORD, and APPLE_TEAM_ID must be set together")
    if all(notarization):
        for name in NOTARIZATION:
            cleaned[name] = environment[name]
    return cleaned


def run_tauri(
    arguments: Sequence[str],
    environment: Mapping[str, str],
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> int:
    """Run Cargo without capturing its output, so Tauri logs stream to the job."""
    result = runner(["cargo", "tauri", *arguments], env=environment, check=False)
    return result.returncode


def main(
    argv: Optional[Sequence[str]] = None,
    environment: Optional[Mapping[str, str]] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> int:
    try:
        child_environment = tauri_environment(environment if environment is not None else os.environ)
    except ConfigurationError as error:
        print(f"tauri CI configuration error: {error}", file=sys.stderr)
        return 1
    return run_tauri(argv if argv is not None else sys.argv[1:], child_environment, runner)


if __name__ == "__main__":
    raise SystemExit(main())
