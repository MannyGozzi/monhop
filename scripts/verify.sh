#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p tauri-runtime-wry --lib --locked navigation_tests
pnpm install --frozen-lockfile
pnpm run --silent lint
pnpm run --silent fmt:check
pnpm run --silent test:ui
python3 -m unittest discover -s scripts/tests -p test_icons.py
python3 -m unittest discover -s scripts/tests -p test_changelog.py
python3 -m unittest discover -s scripts/tests -p test_release.py
python3 -m unittest discover -s scripts/tests -p test_assemble_release.py
python3 -m unittest discover -s scripts/tests -p test_tauri_ci.py
if [[ "$(uname -s)" == Darwin ]]; then
  python3 -m unittest discover -s scripts/tests -p test_macos_signing.py
  python3 -m unittest discover -s scripts/tests -p test_verify_macos_bundle.py
  python3 -m unittest discover -s scripts/tests -p test_native_quit.py
  cargo test -p tauri-plugin-updater --lib --locked
fi
cargo build --workspace --release --locked
if ! command -v cargo-audit >/dev/null || ! command -v cargo-deny >/dev/null; then
  echo 'Install the pinned build-only audit tools documented in README.md, then rerun.' >&2
  exit 1
fi
cargo audit
cargo deny check licenses bans sources
python3 -m unittest discover -s scripts -p test_dependencies.py
python3 scripts/dependencies.py --check
