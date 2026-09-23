# MonHop

Private, local-only keyboard and mouse sharing between the computers on your desk running Windows or macOS, in any pairing (Windows to Mac, Mac to Windows, Mac to Mac, Windows to Windows). Each computer pairs with up to 16 others; the keyboard and mouse reach one of them at a time, chosen from Home, and every pair keeps its own display arrangements. Original code is copyright Manuel Gozzi and licensed under the GNU General Public License, version 3 or later. See LICENSE and docs/PROJECT_SCOPE.md.

This is a development checkpoint, not a working cross-machine input switch yet. The Windows executable provides native diagnostics and starts with sharing disabled. It does not install startup entries, alter the firewall, request elevation or enable remote input automatically. Monitor Ctrl remains a separate application.

The Mac workspace now builds and links natively with Rust 1.98.1. Display enumeration and passive capture work in the tested Codex/ChatGPT launch context. This is not proof of standalone app permissions or cross-machine sharing.

## Landing site

The public landing page is a separate static site under `site/` with its own package and build; see `site/README.md`. It never ships with the app.

## Releases

Releases are cut with `scripts/release.py` and published by the tag-driven GitHub workflow into this repository's releases, which is also where installed apps look for signed updates. `CHANGELOG.md` holds what changed; `docs/RELEASING.md` is the runbook.

## Setup app

The desktop follows the system light or dark appearance, with Flip-inspired charcoal surfaces, Flip’s red accent and reduced-motion support. First launch opens Welcome. Guided setup walks through Get ready → Connect → Arrange, with fixed Back/Continue controls, one primary action per step and expandable details. Applying the arrangement turns sharing on; there is no separate test step. Returning users see a dashboard with remembered computer names and inherited display previews. The dashboard loads bounded local presentation metadata. Normal startup also checks permissions, connected interfaces and displays using read-only OS queries. These checks do not open protected keys, request permissions, bind sockets or enable input. Cached computers are not live connections or trusted pairing records. Reconnect is an explicit guided action. Entering Connect after explicitly choosing a physical network prepares pairing once for that network context and reads protected identity storage, which can show a standard Keychain prompt. A new computer needs a full-fingerprint comparison once; a remembered computer connects with one press from either side. A separate, explicitly started 60-second test can deliver input only into MonHop’s own test window on the receiving computer, in either direction. Desktop-wide sharing remains blocked.

Connect opens a standing, authenticated setup link on the selected physical network: either computer can press Connect first, the dialing side retries every two seconds, heartbeats detect loss, and displays refresh while connected. Arrange the seam on **either** computer, then choose **Apply on both computers**. The other computer validates the proposal against its current identities and monitor geometry and acknowledges automatically; both save the same seams only after that acknowledgment. Each machine retains its own network selection, permissions and protected pairing. A rejected or unacknowledged proposal is reported as not applied. Sync never starts input. Both apps must use the same protocol-version-9 build. The link closes itself after fifteen idle minutes and whenever the network changes.

The app logo uses a detailed, self-contained vector in `apps/monhop-desktop/icons/icon.svg` and an optically simplified small mark in `apps/monhop-desktop/ui/assets/monhop-mark.svg`. Regenerate PNG, ICO, ICNS and the antialiased tray mask with `python3 scripts/generate-icons.py` using the pinned Tauri CLI. Add `--check` to detect stale exports without changing them.

Closing or hiding the main window keeps MonHop in the macOS menu bar or Windows notification-area tray. Its menu provides Show, Hide, Stop and Quit. The icon is neutral while input is local, green for an active session, and amber while starting, stopping or needing attention. Quit waits for native cleanup and in-flight local metadata writes. A failed tray installation leaves the main window visible and retains close-to-quit behavior. No login/startup service is installed.

Accessibility and Input Monitoring results distinguish allowed, not allowed and unknown. Local Network reports request attempts separately, because macOS provides no general access-status API. Access and Network check automatically on first use and when returning to them or from Settings. Concurrent checks are coalesced, busy operations are not interrupted, and failed checks wait for the refresh icon. Refresh controls have keyboard-accessible tooltips. Network selection is temporary until a reviewed sharing layout is explicitly saved. Saved computer names can be changed without touching the protected pairing record; the current protected store still supports one confirmed peer. History is bounded to 16 computer cards and never stores private keys or an enabled state.

On macOS, the Network step also reports Location Services authorization separately from Wi-Fi attachment identity. **Request location access** explicitly asks for standard When In Use authorization, while **Open Location Settings** opens the corresponding privacy pane. Complete Apple's prompt yourself; the status refreshes when you return. A refresh icon remains available. No coordinates are requested, no scan starts, and permission alone does not authorize a network or peer. The automatic check creates a temporary Location manager only to read authorization categories. It never requests permission or starts location updates.

Build the local Mac development bundle with the pinned build-only CLI and Node.js 22 or 24 available for UI tests:

```sh
cargo install tauri-cli --version 2.11.4 --locked
bash scripts/macos-signing.sh setup # one-time, explicit local signing-key creation
cargo build --workspace --locked
bash scripts/verify.sh
bash scripts/build-macos-app.sh
target/aarch64-apple-darwin/release/bundle/macos/MonHop.app/Contents/MacOS/monhop-desktop --check-ui
open target/aarch64-apple-darwin/release/bundle/macos/MonHop.app
```

The UI's JavaScript is linted with oxlint and formatted with oxfmt, both pinned to exact versions in package.json. Run `pnpm install --frozen-lockfile` once, then `pnpm run lint` and `pnpm run fmt:check` (or `pnpm run fmt` to rewrite); `scripts/verify.sh` and `scripts/verify.ps1` already run both.

The one-time signing setup creates a dedicated development certificate and private key in your login Keychain. It does not change certificate trust, TCC permissions or MonHop's peer identity. Subsequent builds reuse that identity rather than generating a new one. Inspect it with `bash scripts/macos-signing.sh inspect`. Missing or invalid signing state is an error, never an automatic replacement.

The packaging script checks license reports, builds for the native Mac architecture in this workspace's target directory, signs the complete bundle with the local identity and verifies its resources and signing requirement. It prints the exact app path (Intel Macs use x86_64-apple-darwin). There is no timestamp-server request or notarization upload. This is a local development build, not a notarized distribution.

Older ad-hoc builds changed their code identity on each rebuild. macOS can leave MonHop's Accessibility switch enabled while rejecting the replacement executable. After upgrading from one of those builds, quit MonHop, remove only its old entry from the affected privacy pane, add `/Applications/MonHop.app`, enable it, then reopen and check again. The app includes these recovery steps for permissions that macOS reports as denied. Do not reset unrelated permissions. A stable signature does not grant access, and permission persistence still requires actual native verification. Only click **Request access** when ready to handle Apple's prompts yourself.

The explicit `--check-ui` diagnostic opens the real native webview, checks bundled HTML, CSS, JavaScript, system-theme styling, the native bridge and disabled sharing controls, then checks layout at normal and minimum window sizes before exiting. It also checks Welcome and guided setup, expands and collapses the local status accordion, checks smooth motion (or reduced-motion behavior), and exercises native tray creation plus Hide/Show. It also invokes one read-only status refresh and checks that the permission and network results render. It prints category results and never requests permissions, reads protected identities or starts a connection. Failure or an incomplete render returns nonzero, with a ten-second render deadline. This checks read-only IPC execution, not granted permissions, input delivery or compositor output. Normal startup does not run the diagnostic.

The verified development bundle can be installed in `/Applications/MonHop.app` and launched from Finder. Installation does not add a login item, change existing permissions or access stored identities; MonHop registers itself at login only when you apply a layout with the onboarding switch on or turn the switch on in Settings. Verify permissions from that installed app, not a Cargo or agent-launched diagnostic.

Windows builds `monhop-desktop.exe` with the workspace. WebView2 must already be installed because runtime downloads are disabled. The shell exposes identity, pairing, display inspection and Stop commands. Native input enablement is rejected and has no webview capability grant. The dedicated trial window has only Start, status, Stop and Close commands, with no pairing or general sharing capability. Its pinned runtime patches remove Tao's initial raw-input registration and deny malformed navigation URLs. CSP, fixed navigation and disabled crash uploading are source controls, not proof that the complete native web engine never uses the network. Native engine behavior and IPC/permission enforcement remain required tests.

## Current checkpoint

| Milestone | Implemented | Still required |
|---|---|---|
| M1 | Workspace, strict binary protocol, topology, physical HID/modifier mapping, held-state and pointer-ownership models | Integrate the returned state actions with live native input |
| M2 | Windows and Mac native adapters, diagnostic CLI, Mac anchored cursor and self-process delivery fixture | Full native test matrix on both OSes, standalone Mac TCC, Caps Lock/layout/system shortcuts and high-rate behavior |
| M3 | Physical NIC/route diagnostics, opt-in native network-change watchers, metadata-validating UDP receive components, explicit OS-protected identity persistence, network policy, P-256 identities, mutually pinned TLS 1.3 QUIC, a guarded endpoint and explicit full-fingerprint pairing with protected peer persistence | Physical pairing on both machines, native trust failure/restart/forget validation, real network-change delivery and the input-session coordinator |
| M4 | Lifecycle coordinator, receiver-first readiness, bounded recovery, saved layouts, a standing setup link with automatic layout sync, and bidirectional sessions in which either computer's keyboard and mouse can control the other | Physical two-machine qualification of take-back, simultaneous crossing, cleanup, loss and latency |

The user-requested visual arrangement is available for preparation. The user-requested inert dashboard and explicit tray controls do not enable input or bypass qualification. The setup shell supplements the native Rust CLI with explicit pairing and sharing. Its native permissions and web-engine behavior still need verification against SECURITY.md. The input path stays in Rust.

Confirmed device trust is retained separately from connections. The native coordinator saves the network, the display crossings and which directions of control are allowed, without saving an enabled state. Its saved layout can only be reused after a fresh authenticated inspection matches both identities and exact display geometry. The setup UI displays detected monitor names and starts from each computer’s OS arrangement. Drag either computer as one group to choose its contact with the other; each computer's displays keep their own OS arrangement. Only touching, nonoverlapping edge segments between the two computers become reciprocal crossings. Applied layouts carry each display’s position by stable display ID, and the arrangement can be saved under a name, reloaded, overwritten or deleted per pair of computers; the named library lives beside the saved setup and never enables input. Every applied layout is also remembered there automatically for the exact displays both computers showed, so a display configuration seen before switches back in by itself; when none fits, sharing continues with what survives of the previous layout where it still validates, and Home shows a dismissable banner offering to rearrange. Names are cosmetic; saved reuse and live checks compare IDs and geometry. Mirrored overlaps and old stretched whole-edge proposals require a new valid arrangement. Save/load never enables input. Their required behavior is in `docs/PROJECT_SCOPE.md`: stopping sharing preserves setup, changed networks or displays revoke the session, and returning hardware never silently enables stale settings.

The desktop coordinator now separates display inspection from input enablement. Stop invalidates pending inspection/enable approvals, pairing mutations cannot overlap sharing, and Quit waits for outstanding native actions and retained input releases. Closing the main window hides it when the tray is available; Metadata inspection retains cancellation through handshake and connection drain. The preparation UI uses explicit inspection, Stop and saved-layout actions. Opening a page never starts those actions. Protocol version 3 added receiver-first readiness: heartbeat processing continues during native startup, the receiver retains ownership before its worker starts, and capture waits for receiver readiness. Protocol version 5 adds the standing setup link and pairing codes that carry each computer’s platform, so two computers on the same OS pick a deterministic dialer. Protocol version 6 carries input-session heartbeats as QUIC datagrams in their own sequence space, keeps them flowing every 30 ms whether or not the last one was answered, and ends a session after half a second without any reply while capping each input-suppression lease at 120 ms. Both apps need the same protocol build. Stop preserves saved setup but invalidates the live inspection, and incomplete native cleanup continues to block new sessions and app exit. Protocol version 9 makes every session bidirectional: either computer's keyboard and mouse can cross to the other, touching the computer being controlled hands it back to its own keyboard and mouse at once, and two switches on Home turn either direction off.

On macOS, active capture and sharing deadlines use sleep-inclusive continuous time. Windows keeps its sleep-inclusive performance clock, and its network watch owns an explicitly registered hidden-window power observer so a short suspend still revokes the session. Explicit network preparation and capture own sleep/wake observers that permanently revoke the current session, without waiting for a peer. Native delivery checks the same revocation latch before new input and still permits managed releases. Stop reaches that latch immediately, and accepted sharing approvals are single-use. These paths have simulated-suspend and callback-lifetime tests, not physical sleep/wake qualification. Saved monitor IDs remain proposals requiring review because stable physical monitor identity is not yet carried across the protocol.

## Two-way sharing

Both computers must install the same current build, then arrange once on either computer and apply on both. After that, either computer's mouse crosses any arranged seam and its keyboard follows; crossing back works the same way from either side. Touching the computer being controlled (a key, a click, a scroll or a deliberate mouse movement) hands it back to its own keyboard and mouse at once, releasing anything the other computer was holding. Home shows two switches, "<this computer> can control <the other>" and the reverse; turning one off makes every seam a wall for that computer's mouse. The last switch that is on cannot be turned off; Pause stops sharing. A switch change reconnects for about two seconds while both computers commit it. The emergency chord (Left Ctrl + Right Ctrl + Esc for 2 s) works on both computers.

## Pair the computers

Pairing verifies and remembers a device. It does not turn on keyboard or mouse sharing.

1. **A1:** On each computer, finish Get ready (access and one recognized physical network), then continue to Connect. Pairing opens by itself. Approve only the standard MonHop Keychain prompt if macOS asks. Existing identities are reused. Creation is a separate action shown only if no identity exists.
2. **A2:** Exchange each computer's public code. Choose **Copy** and paste it into the other computer's code field, then choose **Check code** on both machines. Codes start with `LKM2:` and carry the platform, an address and the public certificate, never the private key; older `LKM1:` codes still read. Copy writes only this code to the system clipboard. It does not read or synchronize clipboard contents.
3. **A3:** Compare the full fingerprint pair on both physical screens. Select **They match** yourself on both, then **Connect** on each, in either order. MonHop decides which computer dials (Windows dials a Mac; two computers on the same OS use the lower fingerprint) and the dialing side retries until the other computer is ready. These roles do not choose which computer supplies the keyboard and mouse.
4. **A4:** Both apps must report **Connected**. A saved-computer message alone does not prove a completed connection check. **Forget computer** requires separate confirmation and preserves the local identity. After a restart, Connect reads the saved pairing again and connects with one press.

Pairing uses the existing mutually pinned TLS 1.3 QUIC transport on UDP 24872, bound to the exact selected interface and peer. No discovery, wildcard bind, VPN fallback or firewall change is added. A pairing attempt lasts at most two minutes of network activity; within that window the dialing computer retries every two seconds and stops as soon as you cancel. Cancel/deadline can close transport even while a normal Keychain prompt is open. Finish that prompt before starting another action. Unknown save outcomes require an explicit reload or forget, never automatic replacement.

On macOS, **Request network access** is available after inspecting and comparing the other computer's code. It exact-binds a temporary UDP socket to the selected physical interface and connects it to that on-link peer without sending data, using Apple's documented best-effort permission request. Waiting for incoming UDP alone need not show a prompt. Allow MonHop if prompted, then explicitly connect. **Open Settings** opens **System Settings → Privacy & Security → Local Network**. Request attempted does not mean permission granted, and successful pairing is only evidence of that connection. Startup and status checks never make this request. Verify the prompt and app attribution from the installed Finder-launched app. VPN routes remain rejected.

The app stores one bounded, versioned peer record in macOS Keychain `ConfirmedPeer` or Windows `confirmed-peer.dpapi`, bound to this computer's local identity. Successful pairing requires mutual certificate possession, exact pairing messages and protected-store readback receipts. The localhost fixtures do not establish physical-network readiness. Input sharing remains disabled until the M2/M3/M4 native safety tests pass.

## Run the Windows diagnostics

After building, use `target/release/monhop.exe`. Running it without arguments only prints status.

```powershell
.\target\release\monhop.exe displays
.\target\release\monhop.exe interfaces
.\target\release\monhop.exe permissions
.\target\release\monhop.exe capture --seconds 10
```

Capture accepts 1..30 seconds, counts categories only, and leaves input local. There is no command to inject, suppress, connect, listen, or install startup in this checkpoint. Native injector APIs exist for the controlled integration harness that must be exercised on each OS.

`check-peer --interface INDEX --local IPv4 --peer IPv4` validates a selected adapter and the actual route without opening a socket. Use values from `interfaces`. Public, off-subnet, gateway-routed, virtual and unknown-attachment selections fail. Passing this check does not establish a peer or authorize a live input session.

## Explicit local identity actions

```sh
monhop identity --create
monhop identity --show
```

Use the built executable or `cargo run -p monhop --locked -- identity --create`. Creation generates a local P-256 identity, stores it through the OS and validates a readback before printing its full public SHA-256 fingerprint. `--show` loads an existing identity without creating one. Neither action pairs a peer or enables input. Startup, help and ordinary diagnostics do not access identity storage.

macOS uses fixed MonHop records in the current user's default file-based Keychain, with a creating-application access policy and no synchronization. These explicit actions may show Apple's standard Keychain dialog. Never automate its approval or broaden access. The launch executable's authorization and persistence across rebuilds must be tested on the actual Mac.

Windows writes only a current-user DPAPI-protected blob to `LocalAppData/MonHop/identity.dpapi`. It rejects network drives and reparse paths, bounds reads, and publishes a new file without overwriting an existing one. Storage errors, corruption and duplicate creation fail instead of silently regenerating or replacing keys. Private DER is never displayed or written unprotected. Real Mac Keychain and Windows DPAPI identities both retained their complete fingerprints across separate processes and duplicate rejection. Preserve those records. Native storage-failure cases and Mac denied/locked/packaged-app authorization remain unverified.

## Controlled Mac delivery test

```sh
cargo run -p monhop-platform-macos --example native_window --locked -- --run
```

This separate example opens its own AppKit window, submits fixed events only to its own process and checks what arrives. It aborts on focus loss and attempts managed-input cleanup before closing. Without the exact `--run` argument, it does not open the window or inject. The normal CLI remains inert.

The Mac injector clamps motion to display rectangles captured when it is created. It must be recreated after monitor changes. Live display-change detection and revocation are not implemented yet.

The fixture checks both sides of the four modifiers, combined Command/Option flags, a drag, both scroll axes and held-input cleanup. A corrected native run observed 24 keyboard, 6 pointer and 2 scroll events and passed in 3.23 seconds. This establishes self-process event delivery, not successful application shortcut execution. Caps Lock, keyboard layout, system shortcuts, physical input feel and the normal system-wide posting route remain unverified. See TESTING.md for the exact evidence and limits.

The explicit `--capture` mode opens a separate count-only physical-input window for ten seconds. It installs no injector, consumes ordinary window events, cancels on focus loss, and joins the capture worker before closing. A user-operated test observed 91 keyboard, 195 pointer and 299 scroll events in 10.17 seconds. This is not a measured 1000 Hz stress test.

```sh
cargo run -p monhop-platform-macos --example native_window --locked -- --capture
```

## Controlled Windows capture test

```powershell
cargo run -p monhop-platform-windows --example windows_native_window --locked -- --run
```

Click Start in its own window, then provide ordinary physical keyboard, mouse and scroll activity. No `SendInput` call is made. Its output distinguishes window-category observations from aggregate Raw Input and always labels injection `NOT_VERIFIED`. Closing the window cancels the UI check and waits for the bounded capture worker before teardown. The UI deadline is checked between messages, so a titlebar modal move can delay UI completion. Actual Windows behavior must be tested on Windows.

The first user-operated Windows run observed keyboard, pointer and scroll categories but failed because the window lost focus. That is not a passing controlled-input test. The fixture now reports capture phases, the first focus-loss source/time and worker elapsed time separately from time spent waiting for Start. No application names, window identifiers, key identities or text are printed.

Mac network diagnostics enumerate physical adapters and inspect the kernel route without contacting the peer. Only explicit `interfaces` and `check-peer` diagnostics read physical Wi-Fi attachments through CoreWLAN. Ordinary enumeration does not. Those CLI diagnostics never scan or request Location permission, and SSID/BSSID values are never printed. Earlier CLI checks could not read the Wi-Fi attachment. The installed app now reports the selected Wi-Fi as recognized after the user granted Location access. Each pairing attempt still rechecks the current attachment and route. Ethernet attachment identity is not implemented.

Native UDP receive components validate each datagram's source, destination and arrival-interface metadata. Exact-bound, interface-pinned synchronous localhost probes passed on both machines. The ownership-preserving asynchronous companion passed its wakeup/error probe on both machines, with an additional Mac negative control. These are not physical-network proof or part of CLI startup.

Separate native watcher APIs latch revocation on network changes and never rearm themselves. Dropping a watcher also permanently revokes its shared signal. macOS requires successful Dynamic Store dispatch registration. Windows watches IP Helper changes and selected Wi-Fi attachment events. Neither watcher is started by the CLI. The guarded endpoint library now retains a watcher for its lifetime. Eight registration/teardown cycles passed on each physical OS, but actual cable/Wi-Fi/VPN/suspend change delivery remains unverified. The Mac subscription is deliberately broad and can revoke on unrelated network configuration changes.

## Guarded transport checkpoint

The transport library now prepares an exact, interface-pinned IPv4 socket after registering a native watcher and validating a fresh adapter, attachment and on-link route. It rechecks those observations after setup. Only the selected peer address/port and destination/arrival interface can reach QUIC. Sends cannot change peer, source or interface. Unrelated packets have a bounded discard budget, and native failures permanently revoke the session.

Revocation wakes the single endpoint receive driver, including when no packets arrive. Its terminal error fails local connections and waiting operations without a peer acknowledgement. The native watcher stays on its owner thread, endpoint rebinding is not exposed, and dropping the owner revokes the session. The library does not establish human confirmation, persist peer trust or enable input. No CLI connect/listen command calls it.

The Mac localhost test exercises this guarded socket with mutual TLS, fixed diagnostic data and idle shutdown. The matching Windows localhost probe also passed on 4756398 in 0.02 seconds. Both machines' physical-network authorization and real change delivery remain unverified. The current app can read the selected Mac Wi-Fi attachment, but an actual two-machine guarded handshake still needs validation. No permission, route or VPN fallback has been added.

## Build on Windows

Visual Studio C++ tools and the Windows SDK are required. This checkout uses a project-local Rust toolchain, without changing the user's PATH:

```powershell
.\scripts\bootstrap.ps1 # first checkout only; downloads official Rust build tools
. .\scripts\env.ps1
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run `scripts/verify.ps1` for the complete Windows gate and a read-only dependency-report check. Regenerate tracked reports explicitly with `scripts/dependencies.ps1` after dependency changes. Python 3.9+ and Node.js 22 or 24 are required for the report and UI tests. Build-only audit tools are pinned:

```powershell
cargo install cargo-audit --version 0.22.2 --locked
cargo install cargo-deny --version 0.20.2 --locked
```

The pinned Rust version is in rust-toolchain.toml. The local `.tools` directory is not source code and should not be copied to macOS. On the Mac, use normal rustup and Xcode Command Line Tools, then run `bash scripts/verify.sh`. Follow docs/MAC_HANDOFF.md for the exact next work and verification boundaries.

## Security model

No runtime Internet access, cloud service, public DNS, HTTP, telemetry, clipboard or file transfer, apart from the signed update check: one HTTPS request to the pinned release host, only with automatic updates on or on Check now, never while sharing runs, and nothing is installed before its signature verifies. Sharing will require explicit pairing to one device through one selected physical interface. Public, off-subnet, gateway-routed and virtual-adapter connections are rejected. Identity changes require re-pairing. Input logs contain no key identities or typed content.

The complete contract is SECURITY.md. ARCHITECTURE.md describes module ownership, TESTING.md defines the verification matrix, and THIRD_PARTY_LICENSES.md documents dependency licensing. Platform-specific limitations and unverified behavior are reported as such.

## Milestones

Live Windows-to-Mac sharing must wait for Mac permission/injection tests, complete network enforcement and measured cross-machine behavior. Later topology UI and reverse-direction support do not bypass those gates. See TESTING.md for evidence from this Windows checkpoint and remaining native checks.
