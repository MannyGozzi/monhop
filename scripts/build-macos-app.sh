#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Build the macOS app on a Mac.' >&2
  exit 1
fi
if [[ "$(cargo tauri --version)" != 'tauri-cli 2.11.4' ]]; then
  echo 'Install the pinned build-only tool: cargo install tauri-cli --version 2.11.4 --locked' >&2
  exit 1
fi
python3 scripts/dependencies.py --check
workspace="$PWD"
host=$(rustc -vV | sed -n 's/^host: //p')
case "$host" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) echo 'A native macOS Rust toolchain is required.' >&2; exit 1 ;;
esac
cd apps/monhop-desktop
# The updater artifacts are signed with the MonHop update key; the CLI only reads the key's content.
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  key_file="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.tauri/monhop-updater.key}"
  if [[ -f "$key_file" ]]; then
    TAURI_SIGNING_PRIVATE_KEY=$(<"$key_file")
    export TAURI_SIGNING_PRIVATE_KEY
  else
    echo "No updater signing key at $key_file: set TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH." >&2
    exit 1
  fi
fi
# The Tauri build is forced ad hoc before the explicit stable-signing step below.
env -u APPLE_CERTIFICATE -u APPLE_CERTIFICATE_PASSWORD -u APPLE_SIGNING_IDENTITY \
  -u APPLE_ID -u APPLE_PASSWORD -u APPLE_TEAM_ID \
  -u APPLE_API_KEY -u APPLE_API_ISSUER -u APPLE_API_KEY_PATH \
  CARGO_TARGET_DIR="$workspace/target" CARGO_BUILD_TARGET="$host" \
  cargo tauri build --target "$host" --bundles app --ci -- --locked
bundle="$workspace/target/$host/release/bundle/macos/MonHop.app"
"$workspace/scripts/macos-signing.sh" sign "$bundle"
printf 'Verified stable-signed development app: %s\n' "$bundle"
