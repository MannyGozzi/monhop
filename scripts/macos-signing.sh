#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'macOS signing is available only on macOS.' >&2
  exit 1
fi

root=$(cd "$(dirname "$0")/.." && pwd -P)
source_file="$root/scripts/macos-signing.swift"
if [[ ! -f "$source_file" ]]; then
  echo 'The MonHop macOS signing helper is missing.' >&2
  exit 1
fi

exec /usr/bin/xcrun swift "$source_file" "$@"
