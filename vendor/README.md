# Vendored desktop runtime patches

## Source provenance

The trees were copied from Cargo's local crates.io source cache, with only
Cargo's extracted-cache sentinel omitted. The original archive SHA-256 values are recorded below and verified against Cargo’s cached archives.

| Directory | Crate | Version | Original archive SHA-256 | Preserved license files |
| --- | --- | --- | --- | --- |
| `vendor/tao` | `tao` | `0.35.3` | `d1c93047acf68669466a34690ac58cca7010bd1b201e1ec86f1fd0a75d3dd4a9` | `LICENSE`, `LICENSE.spdx` |
| `vendor/tauri-runtime-wry` | `tauri-runtime-wry` | `2.11.4` | `4e6fac707727b7a2f48e4ded90976324267371073edbb415ffb73bb0458d203f` | `LICENSE_APACHE-2.0`, `LICENSE_MIT` |
| `vendor/tauri-plugin-updater` | `tauri-plugin-updater` | `2.11.0` | `b28d8cabdeb0564f03ae261963de4bc3d98321cd3d213e76a81b7d344e5df606` | `LICENSE_APACHE-2.0`, `LICENSE_MIT`, `LICENSE.spdx` |

## Minimal patch provenance

The patches are limited to these boundaries:

- `tao/src/platform_impl/windows/event_loop.rs`: removes the one startup call to
  `register_all_mice_and_keyboards_for_raw_input` in `EventLoop::new`. The existing
  `set_device_event_filter` implementation is unchanged, so this patch neither
  changes platform defaults nor registers or removes raw input on startup.
- `tauri-runtime-wry/src/lib.rs`: routes navigation through a private parser helper.
  A malformed URL returns `false` without invoking the caller's handler. Parsed URLs
  retain the handler's allow or deny decision. Two unit tests cover both cases.

- `tao/src/event.rs` and `tao/src/platform_impl/macos/{app_delegate,app_state}.rs`:
  defer native termination through a cancellable event and reply to AppKit only
  after asynchronous shutdown finishes, outside callback locks.
- `tauri-runtime-wry/src/lib.rs`: forwards the native termination request to
  Tauri's existing cancellable exit callback.
- `tauri-plugin-updater/src/{updater,error}.rs`: stages Mac updates beside the
  installed app, validates archive paths and bundle structure, and atomically
  exchanges the bundles. A failed preparation or exchange preserves the installed
  app. Windows shutdown follows successful installer launch, not a failed launch.
- The updater manifests replace its macOS AppleScript dependency with the already
  resolved `libc` version for Darwin's atomic exchange operation. The root lockfile
  and dependency reports record that change. License text is unchanged.

## Verification limits

- A unique temporary staging-lock scratch build ran
  `cargo test --offline navigation_tests` for `tauri-runtime-wry`: 2 passed, 0 failed.
  It did not launch a native app or webview.
- The coordinator ran `cargo check -p tao --lib --locked --target
  x86_64-pc-windows-msvc` successfully on the Mac. A native Windows application
  build and startup check remain required.
