# MonHop macOS continuation

This repository is independent of Monitor Ctrl. The exact requested scope is PROJECT_SCOPE.md. Current capability and gaps are canonical in ../README.md. Do not enable live input sharing based only on green unit tests.

## Start on the Mac

Copy the source archive or repository, excluding `.tools`, `target`, `.git` and Windows binaries. Use the Mac's normal Rust/rustup installation. Install Xcode Command Line Tools if absent. `rust-toolchain.toml` selects Rust 1.98.1. The build-only audit tools are cargo-audit 0.22.2 and cargo-deny 0.20.2.

```sh
cargo build --workspace --locked
bash scripts/verify.sh
cargo run -p monhop -- displays
cargo run -p monhop -- permissions
```

If native permissions are missing, explicitly run `cargo run -p monhop -- permissions --request`, complete Apple's standard prompts and rerun the read-only check. Do not bypass TCC, disable SIP or use an elevated helper. Permission attribution to Terminal versus the built executable must be checked on the actual Mac.

Prefer the controlled window: `cargo run -p monhop-platform-macos --example native_window --locked -- --capture`, then provide physical keyboard/mouse activity there. Only category counts may be printed. Confirm permission denial is an error, the timeout works under heavy input, the event tap is invalidated on exit, and injected events are filtered. No keys or text may be recorded in logs.

The display FFI uses Apple's [`CGDisplayIsMain` declaration](https://developer.apple.com/documentation/coregraphics/cgdisplayismain(_:)?language=objc) and architecture-specific Mach `boolean_t` definitions for [Apple Silicon](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/arm/boolean.h) and [Intel](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/i386/boolean.h). Both declarations passed compile-time checks against the installed Mac SDK on 2026-09-10. This is not Intel runtime evidence. Fractional scroll deltas accumulate into integer pixel events, and recovery clears pending fractions. A user-operated ten-second native window captured scroll-category events. Both individual physical axes, 1000 Hz deadlines and input feel still need the matrix below.

## Next implementation sequence

1. Complete M2 with a controlled native test window on each OS. Exercise the existing injectors there, verify both sides of modifiers, Command/Option shortcuts, Caps Lock, keyboard layout, mouse dragging, both scroll axes and release cleanup. Do not inject into a terminal, password manager or unrelated document. `CGEventPost` has no delivery acknowledgement. The Mac injector now accumulates relative motion from its last submitted absolute point and uses that point for buttons. Motion is clamped to valid display rectangles snapshotted at construction, excluding gaps. Recreate the injector after display changes; live display-change revocation is not implemented yet. It requires an absolute move before relative/button input and a new anchor after recovery. High-rate physical forwarding remains unverified.
2. Complete M3's physical network validation before exposing CLI connect/listen. The guarded endpoint library now combines watcher registration, fresh adapter/attachment/route validation, exact IPv4 binding, native interface pinning, per-packet arrival/peer checks and permanent idle revocation. Its Mac localhost test is not a physical factory or network-change proof. Windows combined-path testing remains pending. Explicit CoreWLAN attachment reads return unknown on the current Mac, and usable Ethernet attachment identity is not implemented. Repeated native watcher start/drop passed on both machines, but actual link-change delivery remains pending. Changes revoke a session permanently until explicit reconnect. Never add wildcard binding or VPN fallback to make a connection work.
3. Validate the explicit `identity --create` / `identity --show` persistence path on both physical machines. Windows DPAPI and Mac file-Keychain providers are wired. Both physical machines passed native creation, separate-process readback and duplicate rejection. Preserve their existing identities. Native storage-failure cases, Mac denied/locked access and packaged-app authorization remain pending. Creation never replaces an existing or corrupt record. Never write unprotected DER keys or automate Keychain approval. Implement explicit certificate exchange and full fingerprint comparison at both machines, or an audited handshake-bound SAS. The existing VerifiedPeer API does not establish human confirmation by itself. Test restart, wrong key, re-pairing and corrupt storage before input is allowed.
4. Build the M4 coordinator: bounded capture queue, canonical physical input, edge transition and current-epoch acknowledgement, outgoing release/incoming modifier-button transfer, authenticated bounded protocol delivery, native injection and independent receiver watchdog. Local suppression must have an expiring enable lease and local both-Ctrl-plus-Escape recovery held for two seconds. Failure or queue overflow returns local ownership without a peer acknowledgement.

```text
explicit local enable + permissions + network lock + verified peer
  -> capture queue -> core ownership / pressed state -> strict protocol -> QUIC
  -> session/epoch/sequence/limits -> injector
disconnect / timeout / overflow / emergency
  -> revoke forwarding -> release managed input -> restore local ownership
```

Use one owner per file when delegating. Run the real commands after the final edits. Test Windows-to-Mac edge switching with modifiers held, cable/Wi-Fi loss, peer termination, suspend/resume, monitor changes, VPN appearance and Monitor Ctrl dimming. Measure actual latency and input feel. Do not proceed to M5 graphical topology, M6 reverse integration or M7 polish before M4 behaves correctly on both physical machines.

The setup UI must eventually provide minimal animated guidance, direct macOS settings buttons, and read-only verification for each setup step. See PROJECT_SCOPE.md. This requirement does not enable sharing or waive the native gates. TESTING.md records current Mac evidence and launch-context permission limits.
