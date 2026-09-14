# Vendored desktop runtime patches

## Source provenance

Both trees were copied exactly from Cargo's local crates.io source cache, with only
Cargo's extracted-cache `.cargo-ok` sentinel omitted. Their archive SHA-256 values
match the package checksums in `artifacts/readiness-20260910/desktop-staging/Cargo.lock`.

| Directory | Crate | Version | Original archive SHA-256 | Preserved license files |
| --- | --- | --- | --- | --- |
| `vendor/tao` | `tao` | `0.35.3` | `d1c93047acf68669466a34690ac58cca7010bd1b201e1ec86f1fd0a75d3dd4a9` | `LICENSE`, `LICENSE.spdx` |
| `vendor/tauri-runtime-wry` | `tauri-runtime-wry` | `2.11.4` | `4e6fac707727b7a2f48e4ded90976324267371073edbb415ffb73bb0458d203f` | `LICENSE_APACHE-2.0`, `LICENSE_MIT` |

## Minimal patch provenance

Only these upstream source files differ, excluding `.cargo-ok`:

- `tao/src/platform_impl/windows/event_loop.rs`: removes the one startup call to
  `register_all_mice_and_keyboards_for_raw_input` in `EventLoop::new`. The existing
  `set_device_event_filter` implementation is unchanged, so this patch neither
  changes platform defaults nor registers or removes raw input on startup.
- `tauri-runtime-wry/src/lib.rs`: routes navigation through a private parser helper.
  A malformed URL returns `false` without invoking the caller's handler. Parsed URLs
  retain the handler's allow or deny decision. Two unit tests cover both cases.

No versions, dependency declarations, lockfiles, build scripts, or license text were changed.

## Verification limits

- A unique temporary staging-lock scratch build ran
  `cargo test --offline navigation_tests` for `tauri-runtime-wry`: 2 passed, 0 failed.
  It did not launch a native app or webview.
- The coordinator ran `cargo check -p tao --lib --locked --target
  x86_64-pc-windows-msvc` successfully on the Mac. A native Windows application
  build and startup check remain required.
