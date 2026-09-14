#!/usr/bin/env python3
"""Read and cut the Keep a Changelog sections of CHANGELOG.md.

`notes` feeds the release body to CI, `check` refuses a release with nothing recorded, and `cut`
dates the Unreleased section and rewrites the compare links so the newest version comes first.
"""

import argparse
from pathlib import Path
import re
import sys


DEFAULT_FILE = Path(__file__).resolve().parent.parent / "CHANGELOG.md"
UNRELEASED = "Unreleased"

HEADING = re.compile(r"^## \[([^\]]+)\]", re.MULTILINE)
# A section ends at the next version heading or at the link reference block, whichever comes first.
SECTION_END = re.compile(r"^(?:## |\[[^\]]+\]:)", re.MULTILINE)
UNRELEASED_LINK = re.compile(r"^\[Unreleased\]: .*$", re.MULTILINE)
BULLET = re.compile(r"^[-*] \S", re.MULTILINE)


def _label(version):
    name = str(version).strip()
    if name.lower() == UNRELEASED.lower():
        return UNRELEASED
    if name[:1] in ("v", "V"):
        name = name[1:]
    if not name:
        raise ValueError("A version is required.")
    return name


def _section(text, label):
    for match in HEADING.finditer(text):
        if match.group(1) != label:
            continue
        newline = text.find("\n", match.end())
        body_start = len(text) if newline < 0 else newline + 1
        stop = SECTION_END.search(text, body_start)
        return match.start(), body_start, stop.start() if stop else len(text)
    raise ValueError("CHANGELOG.md has no [{}] section.".format(label))


def notes(text, version):
    """Return the body of the `## [version]` section, stripped."""
    _, body_start, body_end = _section(text, _label(version))
    return text[body_start:body_end].strip()


def check(text):
    """Raise when the Unreleased section records no changes."""
    if not BULLET.search(notes(text, UNRELEASED)):
        raise ValueError(
            "The Unreleased section of CHANGELOG.md records no changes; add a bullet before releasing."
        )


def cut(text, version, date, repo_url):
    """Date the Unreleased section as `version` and open a fresh empty one above it."""
    label = _label(version)
    try:
        _section(text, label)
    except ValueError:
        pass
    else:
        raise ValueError("CHANGELOG.md already has a [{}] section.".format(label))

    start, body_start, _ = _section(text, UNRELEASED)
    text = "{}## [{}]\n\n## [{}] - {}\n{}".format(
        text[:start], UNRELEASED, label, date, text[body_start:]
    )

    base = str(repo_url).rstrip("/")
    links = "[{}]: {}/compare/v{}...HEAD\n[{}]: {}/releases/tag/v{}".format(
        UNRELEASED, base, label, label, base, label
    )
    text, replaced = UNRELEASED_LINK.subn(lambda _match: links, text, count=1)
    if replaced != 1:
        raise ValueError("CHANGELOG.md has no [Unreleased] link reference to rewrite.")
    return text


def main(argv=None):
    parser = argparse.ArgumentParser(description="Read and cut the MonHop changelog.")
    commands = parser.add_subparsers(dest="command", required=True)

    read = commands.add_parser("notes", help="Print the notes of one section.")
    read.add_argument("version", help="A version, with or without a leading v, or 'unreleased'.")
    read.add_argument("--file", type=Path, default=DEFAULT_FILE)

    empty = commands.add_parser("check", help="Fail when the Unreleased section is empty.")
    empty.add_argument("--file", type=Path, default=DEFAULT_FILE)

    release = commands.add_parser("cut", help="Date the Unreleased section as a version.")
    release.add_argument("version")
    release.add_argument("date", help="The release date, as YYYY-MM-DD.")
    release.add_argument("--repo-url", required=True, help="The repository URL the links point at.")
    release.add_argument("--file", type=Path, default=DEFAULT_FILE)

    args = parser.parse_args(argv)
    try:
        text = args.file.read_text(encoding="utf-8")
        if args.command == "notes":
            print(notes(text, args.version))
        elif args.command == "check":
            check(text)
        else:
            args.file.write_text(cut(text, args.version, args.date, args.repo_url), encoding="utf-8")
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
