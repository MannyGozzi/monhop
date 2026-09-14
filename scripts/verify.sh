#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p tauri-runtime-wry --lib --locked navigation_tests
npm ci --no-fund --no-audit
npm run --silent lint
npm run --silent fmt:check
npm run --silent test:ui
python3 -m unittest discover -s scripts/tests -p test_icons.py
python3 -m unittest discover -s scripts/tests -p test_changelog.py
python3 -m unittest discover -s scripts/tests -p test_release.py
if [[ "$(uname -s)" == Darwin ]]; then
  python3 -m unittest discover -s scripts/tests -p test_macos_signing.py
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
