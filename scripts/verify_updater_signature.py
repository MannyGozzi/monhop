#!/usr/bin/env python3
"""Verify Tauri updater signatures against the public key built into MonHop.

Pure Python, so it runs on every build runner: the Ed25519 check follows the RFC 8032 reference,
and the minisign framing matches what tauri-plugin-updater verifies on installed copies.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
from pathlib import Path
import sys
from typing import Optional, Sequence

CONFIG = Path(__file__).resolve().parents[1] / "apps" / "monhop-desktop" / "tauri.conf.json"

P = 2**255 - 19
Q = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


class SignatureError(ValueError):
    pass


def _recover_x(y: int, sign: int) -> Optional[int]:
    if y >= P:
        return None
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P)
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P:
        return None
    return P - x if (x & 1) != sign else x


def _decompress(encoded: bytes):
    y = int.from_bytes(encoded, "little")
    sign, y = y >> 255, y & ((1 << 255) - 1)
    x = _recover_x(y, sign)
    return None if x is None else (x, y, 1, x * y % P)


def _add(a, b):
    e = (a[1] - a[0]) * (b[1] - b[0]) % P
    f = (a[1] + a[0]) * (b[1] + b[0]) % P
    g = 2 * a[3] * b[3] * D % P
    h = 2 * a[2] * b[2] % P
    return ((f - e) * (h - g) % P, (h + g) * (f + e) % P, (h - g) * (h + g) % P, (f - e) * (f + e) % P)


def _multiply(scalar: int, point):
    result = (0, 1, 1, 0)
    while scalar:
        if scalar & 1:
            result = _add(result, point)
        point = _add(point, point)
        scalar >>= 1
    return result


def _equal(a, b) -> bool:
    return (a[0] * b[2] - b[0] * a[2]) % P == 0 and (a[1] * b[2] - b[1] * a[2]) % P == 0


_GY = 4 * pow(5, P - 2, P) % P
_GX = _recover_x(_GY, 0)
BASE = (_GX, _GY, 1, _GX * _GY % P)


def ed25519_verify(public: bytes, message: bytes, signature: bytes) -> bool:
    if len(public) != 32 or len(signature) != 64:
        return False
    a = _decompress(public)
    r = _decompress(signature[:32])
    s = int.from_bytes(signature[32:], "little")
    if a is None or r is None or s >= Q:
        return False
    h = int.from_bytes(hashlib.sha512(signature[:32] + public + message).digest(), "little") % Q
    return _equal(_multiply(s, BASE), _add(r, _multiply(h, a)))


def _lines(encoded: str, what: str) -> list[str]:
    try:
        return base64.b64decode(encoded.strip(), validate=True).decode("utf-8").splitlines()
    except (binascii.Error, UnicodeDecodeError) as error:
        raise SignatureError(f"{what} is not base64 minisign text") from error


def _field(line: str, length: int, what: str) -> bytes:
    try:
        value = base64.b64decode(line, validate=True)
    except binascii.Error as error:
        raise SignatureError(f"{what} is not base64") from error
    if len(value) != length:
        raise SignatureError(f"{what} has the wrong length")
    return value


def key_id(public_key: str) -> str:
    """The minisign key ID as minisign prints it."""
    return _field(_lines(public_key, "public key")[1], 42, "public key")[2:10][::-1].hex().upper()


def verify(data: bytes, signature: str, public_key: str) -> None:
    """Raise SignatureError unless `signature` is a valid minisign signature of `data` by `public_key`."""
    public = _field(_lines(public_key, "public key")[1], 42, "public key")
    lines = _lines(signature, "signature")
    if len(lines) < 4 or not lines[2].startswith("trusted comment: "):
        raise SignatureError("signature is not minisign text")
    signed = _field(lines[1], 74, "signature")
    algorithm, signer, raw = signed[:2], signed[2:10], signed[10:]
    if public[:2] != b"Ed" or algorithm not in (b"Ed", b"ED"):
        raise SignatureError("unsupported signature algorithm")
    if signer != public[2:10]:
        raise SignatureError(f"signed by key {signer[::-1].hex().upper()}, expected {public[2:10][::-1].hex().upper()}")
    message = hashlib.blake2b(data).digest() if algorithm == b"ED" else data
    if not ed25519_verify(public[10:], message, raw):
        raise SignatureError("signature does not match the file")
    trusted = lines[2][len("trusted comment: ") :].encode("utf-8")
    if not ed25519_verify(public[10:], raw + trusted, _field(lines[3], 64, "trusted comment signature")):
        raise SignatureError("trusted comment signature does not match")


def app_public_key(config: Path = CONFIG) -> str:
    return json.loads(config.read_text())["plugins"]["updater"]["pubkey"]


def main(arguments: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Verify updater signatures against MonHop's built-in public key.")
    parser.add_argument("signatures", nargs="+", type=Path, metavar="FILE.sig", help="each signature sits next to its file")
    parser.add_argument("--public-key", help="base64 minisign public key (default: tauri.conf.json)")
    args = parser.parse_args(arguments)
    public_key = args.public_key or app_public_key()
    failed = False
    for signature in args.signatures:
        target = signature.with_suffix("")
        try:
            if signature.suffix != ".sig":
                raise SignatureError("expected a .sig file")
            verify(target.read_bytes(), signature.read_text(), public_key)
        except (OSError, SignatureError) as error:
            print(f"{signature.name}: FAILED ({error})", file=sys.stderr)
            failed = True
        else:
            print(f"{signature.name}: verified with key {key_id(public_key)}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
