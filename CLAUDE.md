# MonHop development contract

User scope is preserved in docs/PROJECT_SCOPE.md. Security and correctness outrank latency and appearance.

Runtime must remain local-only: no telemetry, public DNS, shell execution, clipboard, or files over the network. The one outbound exception is the signed update check: one HTTPS request to the pinned release host, only when the user turned automatic updates on or presses Check now, never while a sharing session is active, and nothing is installed before its signature verifies against the public key built into the app. All sharing sockets must be pinned to one explicitly selected physical interface and an explicitly paired on-link private peer. Never bind wildcard addresses or silently switch interfaces. Authenticated peer input is still untrusted.

No input hooks, suppression, injection, listeners, pairing trust changes, firewall edits, startup registration, or system permission changes merely from starting the app. Input diagnostics are explicit and bounded. Never log key identities or typed text. A failed connection must restore physical input without depending on the network. Secure desktop/UIPI/TCC boundaries are not bypassed.

Keep performance-critical input/transport/state handling entirely in Rust. Screen dimming is MonHop's own desktop `dimming` module (overlay per display, one system-wide chord, persisted preference); its overlays are never displays or input targets.

Every UI animation must be buttery smooth. Anything that enters, leaves, or changes position or size animates instead of popping or jumping. Animate transform and opacity where possible and use the shared motion tokens in apps/monhop-desktop/ui/styles.css. An interrupted animation retargets from its current visual state instead of snapping, one whose target is unchanged never restarts, and `prefers-reduced-motion: reduce` makes every change instant.

Installation retention: replace MonHop in place without backing up earlier installations. Delete obsolete executable copies, rollback payloads, and pending installers when updating. Preserve the current installed app, user settings, and diagnostic logs.

Original code is licensed under GPL-3.0-or-later (LICENSE). Third-party dependencies must have permissive GPL-compatible licenses, except the five package-specific MPL-2.0 allowances explicitly approved by the user and recorded in THIRD_PARTY_LICENSES.md. Pin direct versions and commit Cargo.lock when commits are authorized. Keep all runtime dependencies and license/SBOM reports reproducible from the lockfile.

Verification: cargo fmt --all -- --check; cargo clippy --workspace --all-targets -- -D warnings; cargo test --workspace; cargo audit; scripts/dependencies.ps1. Read scripts/verify.ps1 for the project-local Windows toolchain setup. macOS work must be built and tested on macOS before claiming support.

Current lane ownership and handoffs are in .claude/state and communication.txt. Workers are leaves and never delegate. Primary owns Cargo.lock and root workspace changes. Work directly on `main` unless Manuel explicitly requests another branch. No git commits/pushes/PRs unless requested. Two-machine work follows scripts/pc/README.md: the Mac drives the Windows PC over SSH, and the PC never pulls, commits or pushes on its own.
