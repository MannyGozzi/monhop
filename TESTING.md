# MonHop verification

Run the workspace formatter, warning-free Clippy and all tests after each integrated iteration. Run dependency vulnerability and license checks separately. No test may use real keystrokes in logs or enable unbounded input suppression.

## Windows checkpoint evidence

The latest reported Windows gate, on 2026-09-11 at 788c24e60898fe5589b8450e11a86cadbfb182d3, passed 140 workspace Rust tests, two patched-runtime tests, six JavaScript tests, 18 Python tests, formatter, Clippy and locked debug/release builds. The corrected dependency report check passed without tracked writes for 475 packages, with 182 Windows and 184 Mac runtime entries. Audit retained seven documented nonblocking warnings, and license/source/ban checks passed with duplicate-version warnings. The controlled physical fixture remained waiting for Start on its original aec60f0 build.

On 2026-09-10, the diagnostic executable enumerated three 2560x1440 displays, including negative origins. It recognized the physical Wi-Fi adapter and rejected the WSL virtual adapter. Read-only route checks rejected a public peer and a different subnet, while accepting an on-link private address without creating a socket.

The three-second passive diagnostic exited after 3.07 seconds. The user was idle and all event counts were zero, so this proves initialization/deadline/teardown, not observation of real physical input. No injection or suppression test was run against the user's desktop. DPAPI round-trip, tamper rejection and size bounds passed using non-secret test data.

Mutual TLS loopback tests use only literal 127.0.0.1:0 addresses and fixed ping/pong bytes. Correct peer pins succeed. Wrong server pins, wrong client pins and missing client certificates fail. Malformed fingerprints and key/certificate mismatch are rejected. These tests do not validate the physical NIC, a real LAN, persisted pairing, input forwarding or latency between machines.

Both native crates pass Apple-Silicon target checks. A full workspace Apple-target check on Windows stops in ring's native build because the Apple C toolchain/SDK is unavailable. Subsequent Mac-native evidence is recorded below. Cross-checks alone do not establish runtime support.

## Mac checkpoint evidence

On 2026-09-10, the workspace built and linked on arm64 macOS 26.6.2 with Rust 1.98.1 and the installed Xcode SDK. The final full Mac gate passed formatter, Clippy, 70 tests (3 native tests explicitly ignored), release build, cargo-audit 0.22.2 and cargo-deny 0.20.2. Three additional pure example trace tests passed separately. The typed native marker callback test was explicitly run and passed without posting input. The existing duplicate-getrandom warning remained. No dependencies or lockfile entries were changed.

The diagnostic enumerated the built-in 3456x2234 display at 1728x1117 logical points, scale 2 and 120 Hz. Compile-time C assertions against the installed SDK verified `CGDisplayIsMain` and the signed arm64 / unsigned x86_64 four-byte `boolean_t` declarations. This checks both ABI declarations, not Intel runtime behavior.

Permission preflight reported Accessibility and listen-event access from the Codex/ChatGPT-launched process. Read-only System Settings inspection showed ChatGPT enabled under Accessibility, no MonHop or Terminal entry, and no Input Monitoring entries. This proves only that launch context. No permission was changed or requested. Standalone MonHop/Terminal attribution and a real denied-permission run remain pending.

The first ten-second passive capture counted 178 keyboard events, with no pointer, scroll or marked synthetic events. Physical provenance was not separately confirmed. A second capture was idle and exited in 10.63 seconds including Cargo/process startup. Neither proves physical mouse/scroll capture or the deadline under heavy input.

An opt-in native integration test observed the process's tap while capture was running and verified that it disappeared before the diagnostic returned, while the process remained alive. It passed with a one-second capture. The test takes three tap-inventory snapshots. Apple's inventory API resets reported tap latency minima/maxima, including other taps' diagnostic statistics, without changing input behavior. Run it explicitly, not as part of normal unit tests:

```sh
cargo test -p monhop-platform-macos --test native_capture --locked -- passive_capture_removes_tap_before_returning_in_the_same_process --ignored --exact
```

The separate ignored `denied_permission_rejects_capture_without_installing_a_tap` test must run from a context that genuinely lacks listen-event permission. Do not revoke another application's permission or bypass TCC to obtain that result.

The corrected self-process fixture checks both sides of Shift/Control/Option/Command, complete chord releases, left drag, both scroll axes and Ctrl/right-button cleanup. A native continuation run passed with 24 keyboard, 6 pointer and 2 scroll events in 3.23 seconds. Raising ChatGPT during a second run produced the expected focus failure. The fixture only posted to its own PID and never targeted ChatGPT. Caps Lock, keyboard layout, real application/system shortcuts and the global posting route remain unverified.

The user-operated `--capture` window completed in 10.17 seconds with 91 keyboard, 195 pointer, 299 scroll and zero marked synthetic events. It did not record key identities or text. The independent native cancellation test returned in 0.20 seconds, and natural tap teardown passed in 1.05 seconds. These prove count-only physical activity and teardown in the tested launch context, not individual physical scroll axes, denied standalone permissions or sustained 1000 Hz input. Later source fixes corrected startup-event handling, category labels and final focus checking, with pure regression coverage. The physical receipt predates those final fixes. A final-source delivery rerun correctly aborted on unmarked keyboard input after 2 marked keyboard and 1 pointer event. It is a negative-guard receipt, not a new full-delivery pass.

The Windows example requires exact `--run` and an in-window Start click. It never calls `SendInput`, and `NOT_VERIFIED` remains the injection result even if passive categories pass. Its runtime state model is also exercised by the pure tests. Mac-host Windows target checks prove compilation only, not Windows linking, focus behavior or physical observation. Raw Input has a fourteen-second bound; an early close must join that worker before native window teardown. UI deadline checks can be delayed by Windows titlebar modal moves.

Mac physical-interface and PF_ROUTE diagnostics have pure malformed-route/adapter tests plus explicit native read-only checks. No peer endpoint is created. The explicit CoreWLAN query linked and ran successfully but returned attachment-known=false on physical en0/index15. No scan or Location permission request was made and no SSID/BSSID was printed. Unknown attachment still rejects network authorization. A guarded endpoint library now composes these components, but its physical setup has not been validated.

Local command receipts are in the ignored `artifacts/mac-20260910/` and `artifacts/readiness-20260910/` directories. Earlier receipts are retained, not relabeled as final validation.

## Windows physical-input receipt

The user-operated fixture on c7d2b8b2db25fe22a39af19d1d9a90d53f5d6d98 observed keyboard, pointer and scroll window categories, but reported `passive=FAIL focus=lost`. Raw Input counted 56 keyboard, 1,813 mouse, 1,749 motion, 18 button, 56 scroll and zero MonHop-marked events. Raw Input also observes background activity, so these counts do not override the focus failure. Injection remains `NOT_VERIFIED` and was not attempted. Individual scroll axes and message provenance were not established.

The 3,577.737-second process lifetime included waiting for Start and is not capture-duration evidence. The fixture now prints `WAITING_FOR_START`, `CAPTURING` and `FINISHED`, records the first focus-loss source and milliseconds since accepted Start, and times the Raw Input worker including setup/teardown independently of the pre-Start wait. Only source categories, aggregate categories and timing are emitted. These source changes require a new Windows-native run. Browser coordination should pause only during active capture, not throughout an idle Start window.

## Native readiness checkpoint

The final Mac gate for this continuation passed 90 tests with seven opt-in native tests ignored, workspace formatter, warning-free Clippy, release build, vulnerability audit and permissive-license/source/dependency-ban checks. The existing duplicate-getrandom warning remains. Both native example test suites are now part of `cargo test --workspace`; their build targets have distinct names so they cannot overwrite each other. The Windows target all-target check and example Clippy passed on the Mac. Windows linking and runtime tests remain separate.

The final route parser passed the explicit physical direct-route test in 0.02 seconds using the current Mac address and its on-link router. This queries the kernel only and sends no peer packet. It also rejects the VPN-selected route instead of forcing another route. A route diagnostic pass is not network attachment identity, peer authentication or permission to forward input.

## Network watcher checkpoint

The watcher checkpoint passed 124 Rust tests with eight opt-in native tests ignored, five dependency-report tests, formatter, warning-free Clippy, locked debug/release builds, vulnerability audit and license/source checks. Reports remain current for 115 packages. These are component checks, not live session validation. Error-result helper tests are not native registration or cancellation fault injection.

The macOS watcher uses checked `SCDynamicStoreSetDispatchQueue` registration. Eight native start/drop cycles passed in 0.03 seconds. This proves registration and bounded caller teardown, not actual cable, Wi-Fi, VPN or suspend/resume notification delivery. An earlier artificial Dynamic Store notification probe was rejected by macOS before delivery. It was removed because that API requires write authorization and can temporarily mutate a value. No elevation, entitlement or network-setting workaround was used.

The Windows watcher passed eight native start/drop cycles in 0.07 seconds on the selected, present/up physical Wi-Fi interface18 at b46dc672f5c9dbd265aa25af2c2b0e02d94d8f32. The environment variable was restored afterward. This does not demonstrate actual network-change delivery or cancellation-failure behavior. It also has pure notification-filter and permanent-latch tests. After selecting a real present/up physical adapter, this exact opt-in test starts and drops the observer eight times without changing network settings:

```powershell
$env:MONHOP_TEST_INTERFACE_INDEX = '<selected physical index>'
cargo test -p monhop-platform-windows --lib --locked network_watch::tests::native_selected_interface_start_stop_is_bounded -- --exact --ignored --nocapture
```

Watchers are now retained by the guarded endpoint library, but neither is wired into executable startup or input forwarding. Source/registration tests do not demonstrate end-to-end fail-local recovery. Real network changes must be user-operated in a controlled two-machine test, with no automatic reconnect or interface fallback.

## Packet metadata checkpoint

The Mac gate at aec60f0a2a7e22604964855425f77eee55a3cdb4 passed 149 Rust tests with nine opt-in native tests ignored, five Python tests, formatter, warning-free all-target Clippy, locked debug/release builds, vulnerability audit and license/source checks. Dependency reports remain current for 115 packages. Windows-target platform checks and Clippy also passed on the Mac, without claiming Windows-native execution.

The new receivers own their configured UDP sockets and issue one nonblocking receive per call. Pure tests reject missing, duplicate, truncated, malformed or inconsistent ancillary metadata and invalid payload bounds. Both OSes accept legal empty UDP datagrams at this layer, without authorizing protocol delivery. The Mac requires `IP_PKTINFO` and checks consistency if optional destination/interface messages are present.

The explicit Mac localhost metadata probe passed with an exact 127.0.0.1 bind and loopback-interface pin on both sockets. It sent only fixed diagnostic bytes between two sockets in the same process, then verified source address, destination and arrival index. The probe has a one-second receive deadline. This establishes the native receive ABI on loopback, not a physical-interface boundary or peer session. Eight updated Mac watcher start/drop cycles also passed in 0.02 seconds, including shared-signal revocation after each drop. The receipt is `artifacts/readiness-20260910/native-packet-boundary.json`.

Run the matching Windows probe only after the normal build gates, not by enabling all ignored native tests:

```powershell
cargo test -p monhop-platform-windows --lib --locked udp_receive::tests::native_loopback_metadata_matches_bound_addresses -- --exact --ignored --nocapture
```

The matching Windows localhost metadata probe passed at aec60f0a2a7e22604964855425f77eee55a3cdb4 with exit0, 0.00s rounded test time and 0.848s command time. Eight updated Windows watcher cycles also passed in 0.08 seconds, asserting shared revocation after each Drop. Neither probe used peer traffic or changed network settings. The new physical Start-window run remains pending.

The final asynchronous-component Mac gate passed 149 Rust tests, with ten opt-in native tests ignored, five Python tests and all locked build, format, Clippy, audit, license and report checks. Windows-target platform checks also passed. The package count remains 115; only two local-package dependency edges to the already pinned Tokio changed.

The asynchronous companion moves the same configured socket into Tokio. Its separate ignored test exercises a waiting receiver, a later fixed-byte send, a new task receiving after stale readiness, and truncated-datagram error propagation. It also exercises bounded write readiness. This is not an endpoint or an input test. Native evidence is recorded separately from the regular workspace suite.

The Mac asynchronous localhost probe passed natively, including a real empty-socket `WouldBlock`, later data waking a different reader task, destination/interface checks, and truncation returning `InvalidData`. A temporary negative control that skipped readiness re-registration failed with the expected reader timeout in 1.00 seconds. The source was restored byte-for-byte and the same probe passed again. A parent task owns the deadline, so its timer cannot accidentally wake the stalled reader and mask the bug. The receipt is `artifacts/readiness-20260910/native-async-udp.json`. This remains localhost-only evidence. The matching Windows asynchronous probe passed on c165b7c0e7a327c1f6d7dc3ca61fecd88350d7fb, with 0.00s rounded test time and 2.708s command time.

Actual physical arrival filtering, blocked wrong-interface packets, route/attachment changes and idle-endpoint revocation remain unverified. No new connect/listen or sharing command is exposed.

## Guarded transport checkpoint

The Mac guarded-library checkpoint passed 170 Rust tests, with 11 opt-in native tests ignored, five Python tests and the locked workspace build, format, Clippy, release, audit, license and dependency-report gates. Reports cover 115 resolved packages. Windows-target platform checking and Clippy passed locally. Windows passed the same checkpoint on 4756398b16a8d02e4ef9c243d656a2815f2836da: 133 Rust tests, four ignored native probes, five Python tests and all nine gates. Its exact localhost guarded probe passed in 0.02 seconds, with 0.183 seconds total command time.

The explicit Mac localhost probe passed mutual TLS, fixed diagnostic data and revocation using exact-bound, loopback-pinned sockets. A cold listener with no traffic or protocol timers also woke and stopped. This tests the private guarded socket adapter, not the physical-interface factory or real network-change notifications. Run only this opt-in probe, not all ignored tests:

```sh
cargo test -p monhop-transport --lib --locked guarded_endpoint::socket::tests::native_loopback_guarded_quic_delivers_and_revokes -- --exact --ignored --nocapture
```

Regression tests cover hard send errors, partial sends, revocation during a send and writable-readiness failures while retaining multiple connection handles. A saved incoming handshake is rejected if it registers after endpoint-driver cleanup. The original hard-error path left the retained connection open and failed the one-second deadline. The corrected path revokes through the endpoint receive driver and closes all retained handles. Idle tests stop peer traffic, drain pending packets and observe a fresh waiting receive. Disabling only the revocation observer registration makes both the cold-listener and idle-connection tests fail their deadlines; byte-for-byte restored source passes.

An earlier isolated negative control accidentally shared Cargo's target directory with the main checkout. Cargo reused the control binary during a nominal restored-source run. The failed receipt is retained as build-cache contamination, not a product failure or pass. Cleaning the affected workspace packages and rebuilding the original source corrected the experiment. Separate workspaces now use separate target directories.

The native receipt is `artifacts/readiness-20260910/native-guarded-quic.json`. Neither passing tests nor the public `VerifiedPeer` type supplies human confirmation, permissions, physical route/attachment evidence or fail-local input recovery. Sharing and CLI connect/listen remain disabled.

## Automated gates

### Protected identity checkpoint

The final identity checkpoint gate passed 114 Rust tests (seven opt-in native tests ignored), five dependency-generator tests, formatter, warning-free Clippy, locked debug/release builds, vulnerability audit, license/source checks and current reports for 115 resolved packages. Only the existing duplicate-getrandom warning remains.

The CLI accepts only explicit `identity --create` and `identity --show` operations. Default startup and help were run without touching credentials. Targeted tests passed for the bounded/versioned record, restart-equivalent load, wrong key/certificate, existing and corrupt records, missing/mismatched readback, denied/locked store mapping, exclusive file publication and temporary-file cleanup. Windows adapter all-target checking and Clippy also passed on the Mac. Cross-checking the entire Windows app from macOS is blocked by ring's C build requiring Windows SDK headers; native Windows builds remain the authority.

The Mac debug executable created and read back one actual MonHop Keychain identity. Separate `--show` processes retained the same full fingerprint before and after a rejected duplicate `--create`. This did not confirm a peer or enable input. The receipt is `artifacts/readiness-20260910/mac-identity-native.json`. The Windows release executable also passed missing-record detection, explicit creation, fresh-process full-fingerprint matching, duplicate rejection and unchanged final readback at b46dc672f5c9dbd265aa25af2c2b0e02d94d8f32. Both real identities must be preserved. Native storage-failure cases, denied/locked Keychain behavior and packaged-app authorization remain unverified. Before enabling pairing:

| Check | Required native evidence |
|---|---|
| Initial creation | Explicit action stores one OS-protected record and prints only its public fingerprint after readback |
| Process restart | A separate `identity --show` process returns the same complete fingerprint |
| Duplicate creation | A second `--create` fails and a following `--show` returns the unchanged fingerprint |
| macOS authorization | Observe the actual executable's standard Keychain prompt/ACL, denied access and locked-store behavior without automating approval or changing unrelated permissions |
| Invalid storage | Controlled non-secret fixture rejects corruption and wrong key/certificate without regenerating or replacing a record; never corrupt the user's live identity for a test |
| Peer trust | Full human verification on both machines, wrong peer rejection and explicit re-pairing remain unimplemented |

Generating an identity does not confirm another machine and cannot enable input. These checks must not read, enumerate, modify or remove unrelated Keychain items or credentials.

| Gate | Required evidence |
|---|---|
| Protocol | Roundtrip, every truncated length, malformed kinds/flags/UTF-8, bounded topology, finite numbers, deterministic fuzz corpus |
| Topology | Negative origins, mixed scaling, portrait, proportional edges, gaps, ambiguous links, hysteresis |
| Modifiers | Left/right Ctrl/Shift/Alt/Meta retained, Windows Meta maps to Command, Mac Command maps to Windows |
| Ownership | Shift/Meta/button-held crossings, ordinary held keys not copied, stale acknowledgement rejected, timeout/disconnect/overflow release |
| Emergency | Both Ctrl keys + Escape held two seconds, local operation without network, no key-value logs |
| Network | Off-subnet/private/public/broadcast rejection, gateway route rejection, physical interface enforcement, sticky revocation on interface/network change |
| Identity | Wrong pin rejected, private key protected roundtrip, explicit trust confirmation, no unauthenticated input channel |
| Injection | Synthetic marker is filtered, OS permission failure reported, all managed input released on failure |

## Desktop setup checks

The initial cf49b0b Mac setup checkpoint passed 179 Rust tests with 11 opt-in native tests ignored, six JavaScript tests, 13 Python tests, locked debug/release builds, formatter, Clippy, audit, license/source checks and current reports for 475 packages. This does not establish Windows-native app support or input sharing.

The native-arm64 development bundle passed complete ad-hoc signature/resource verification. A build with deliberately conflicting inherited target-directory and Windows-target settings still produced and verified the intended Mac bundle. All four bundled license/SBOM resources matched their source bytes. The final bundled executable stayed alive for a ten-second launch-only check with one on-screen window in every sample, empty stderr and no app-process Internet sockets observed. The process was then terminated by the harness. This checks window creation, not rendered native content, permission attribution or all WebKit subprocess traffic. No UI command, capture, injection or identity action was invoked. The receipt is `artifacts/readiness-20260910/desktop-native-smoke.json`.

The setup UI's six pure JavaScript tests cover truthful permission state, diagnostic-only interface selection, unknown attachment rejection, vanished selections and refusal to treat reported sharing/pairing flags as usable. Seven desktop Rust tests check local navigation, exact IPC capabilities, fixed settings destinations and bundle-path attribution. Two tests exercise the patched runtime's malformed-navigation rejection. None creates a native window or requests permission.

The browser preview rendered the four steps, kept pairing/test actions disabled and paused both tutorial animations. Its error/warning console was empty and no horizontal overflow was observed. Brave's existing Dark Reader extension changed preview colors, so that screenshot is not evidence of the native app's colors. No browser setting was changed.

The Windows Tao library passed target checking on the Mac. Windows later passed the bounded startup check below. Full WebView2 and COM behavior remains a native verification requirement. The application initializes STA before creating its constrained environment and retains it for the process lifetime. Its own reference is balanced if Tauri returns, but normal Tauri shutdown exits the process directly. No claim is made that app or upstream COM destructors execute on normal exit.

The complete dependency graph has 475 packages. Audit exits successfully with seven non-blocking warnings, including glib unsoundness on an inactive Linux-only path and six maintenance warnings. The supported Windows/macOS normal and build graphs contain no glib. Vendored runtime source also emits upstream compiler warnings; they are not suppressed or described as warning-free. No advisory or runtime network exception was added.

Windows and Mac now use the same dependency-report generator, with explicit UTF-8 Cargo decoding and normalized license line endings. Missing archive license files use exact-version, VCS-matched, hash-checked upstream supplements. Windows `-Check` does not rewrite tracked reports or the historical native DLL receipt. Source archives include both patched runtime trees, and executable packaging rejects a stale native DLL receipt. The read-only PowerShell path was subsequently verified on the Windows test host.

Windows built cf49b0b successfully and passed 140 workspace Rust tests, two patched-runtime tests, six JavaScript tests on Node 24.19.0, 13 Python tests, format, Clippy, release, audit and license checks. Verification then failed before report checking because PowerShell discovered two Python commands and tried to invoke their paths together. The interpreter-selection fix passed natively on 12afb14 with the same two discovered paths. Its report check exposed a separate host-dependent package-list mismatch, so later gates and desktop startup were not run. Mac UI tests used Node 22; no Node 22 Windows result is claimed.

Windows' bounded report comparison confirmed identical license text for every retained section and identical checked-out/committed report bytes. Only package membership differed: Mac-host build tools contributed base64 0.21.7 and swift-rs, while Windows-host build tools contributed vswhom, vswhom-sys and winreg. Cargo's `normal` tree includes host-built procedural macro dependencies even when a different target is selected. Runtime inventories now exclude procedural macros. License notices instead use an explicitly conservative union of supported-target filtered metadata graphs, including normal/build edges and inactive feature-unified packages, but excluding dev-only edges. They are license coverage, not a claim that every listed package runs. Windows reproduced the corrected reports byte-for-byte on 788c24e without regeneration.

The corrected generator passed 18 Python tests on the Mac. The generated runtime lists contain 182 Windows and 184 Mac packages. The conservative notice union covers 338 packages including seven original workspace packages, and preserves every earlier third-party notice's text. Four additional exact upstream MIT-compatible provenance records cover missing archive notices without changing the lockfile or enabling dependencies. All 22 supplement records matched their archive, lockfile, VCS and license hashes.

On 788c24e, the Windows desktop survived a 10.236-second native startup check. Its main window was visible from the first 0.530-second sample through the last sample, normal close exited zero, and stdout/stderr were empty. This does not verify rendered content, IPC/CSP, permissions or engine traffic. The original physical test window and protected identity were preserved.

### Installed Mac Wi-Fi setup checkpoint

The explicit Wi-Fi authorization addition passed 183 Rust tests with 11 native tests ignored, eight JavaScript tests, 18 Python tests and the full locked build/verification gates. The lockfile still resolves 475 packages. CoreLocation 0.3.2 was already locked and is now a direct Mac runtime dependency, increasing that runtime list to 185. Its added supplement matches the exact crate archive, upstream VCS and existing license-text hash. No new license exception was added.

A source review caught the input-permission action returning a snapshot without the new Wi-Fi category. Both status-returning actions now use the enriched snapshot. Category mapping and duplicate-request tests never invoke CoreLocation. The isolated Brave fixture exercised explicit check, simulated denial, disabled repeat requests, the fixed Location Settings command and pairing staying disabled, with no horizontal overflow. It performed no native permission action.

The native-arm64 bundle was installed at `/Applications/MonHop.app`. Source and installed files matched byte-for-byte, the complete ad-hoc signature verified, both Location purpose keys were present, and all four license/SBOM resources matched source. LaunchServices startup survived ten samples with an on-screen window and no app-process Internet sockets observed. The app was left running for manual setup. Receipts are `artifacts/readiness-20260910/mac-installed-app.json` and `mac-installed-startup.json`.

No Location request, input-permission prompt, peer connection or input action was automated. Actual prompt/callback behavior, Settings destination, installed-app TCC attribution, granted attachment reads and full web-engine traffic remain unverified. The requested persistent Off/On control and monitor/seam editor remain requirements, not enabled functionality.

Before trusting the setup app, verify its signed bundle, native startup, visible content, effective IPC/CSP restrictions, settings destinations, launch/TCC attribution and web-engine subprocess traffic on each OS. A point-in-time socket listing is not sufficient to prove no-Internet behavior. Do not enable sharing from these checks.

### Blank Mac window regression

The installed e91c6d3 app showed a blank window despite the earlier window-visibility check. Pinned Tauri resolves its embedded `index.html` to `tauri://localhost`; that non-HTTP URL has an empty path. MonHop rejected it before any page could load. The guard now accepts that exact empty path while retaining the origin, credentials, port, query and other-path restrictions. A regression test failed on the original guard and passes after the fix.

The explicit `--check-ui` diagnostic inspects the actual native webview without invoking setup IPC, permissions, identity storage or input. The original guard produced a render-deadline failure and exit 1. With the fix, all eight HTML/CSS/JavaScript/bridge/disabled-control categories passed in 1.30 seconds in the debug executable. WKWebView's navigation-finished callback arrived before module loading completed, so the diagnostic waits for document readiness within the same ten-second deadline. Malformed receipts fail closed, and the first terminal verdict cannot be overwritten. An external 15-second process bound remains necessary if the native event loop itself hangs.

The rebuilt ad-hoc-signed bundle passed the native DOM check in 0.82 seconds. The installed executable passed in 0.59 seconds with the same SHA-256 `3d508865c666566cf436ce7a4f36c0864e60518c868dd8e41a7b5dfccf580553`; every installed file matched the source bundle and signature verification passed. A normal LaunchServices launch then produced the visible setup screen, verified from a capture of only its owned window (`artifacts/readiness-20260910/blank-ui-installed.png`). The old bundle is retained for rollback. No setup control was clicked.

The final source gate passed 186 Rust tests with 11 opt-in native tests ignored, eight JavaScript tests, 18 Python tests, locked debug/release builds, formatter, Clippy, audit, license/source/ban checks and dependency-report verification for 475 packages. Native receipts are retained under `artifacts/readiness-20260910/blank-ui-*`. These checks do not establish permissions, IPC round trips, physical input or cross-machine sharing.

Windows reported the 24bab9a checkpoint passing 145 Rust tests/four ignored, eight JavaScript tests, 18 Python tests and all build/report gates. Its real native webview passed all eight original DOM categories in 0.516 seconds with exit 0, but stderr reported a failure to unregister `Chrome_WidgetWin_0` with error 1412. This is rendered-content evidence, not clean-shutdown evidence; that cleanup diagnostic remains under investigation.

### Compact setup layout

The setup shell now uses compact neutral styling, a fixed sidebar and header, one scrollable content region, and native-report/tutorial disclosures. The sidebar remains visible without horizontal page overflow. Permission commands, native-check labels, unknown/denied handling and unavailable pairing/test controls retain their behavior. No dependencies, IPC commands or native permission actions were added.

The real Brave fixture matrix uses normal 980x698 and minimum 760x538 content areas, including initial, populated Mac granted/denied/unknown, Windows, unexpected sharing reports, long names/errors, failed IPC and browser-only states. The original UI failed all 18 layout cases; populated Mac permission actions were below the visible area. These are isolated fixtures with mocked native calls, not native permission evidence. Fixtures use immutable source-hash directories to prevent cached modules from mixing UI revisions; a fixture-only Dark Reader opt-out prevents the browser extension from recoloring screenshots.

The final immutable-source browser matrix passed all 18 cases, including all primary Mac permission actions visible at minimum size. The failed-check error remained visible at minimum height, and the tutorial switches reported actual paused/running animation states when their control was used. The packaged native check passed in 0.93 seconds and the byte-identical installed executable in 1.00 seconds, with all categories passing at both size bounds. Its SHA-256 is `ed580e89112dc1b65c38dba47df1209083dc43902eca2db3d7a28388be4d7cd7`. The complete bundle signature and source/installed file hashes match. A normal-launch capture of only the installed window was visually inspected. The final full gate passed 186 Rust tests/11 opt-in ignored, eight JavaScript tests, 18 Python tests and locked build/format/Clippy/release/audit/license/report checks. Receipts and rollback bundle are retained under `artifacts/compact-ui-20260911/`.

The explicit native `--check-ui` gate now verifies bounded width and height, compact text/control sizes, visible navigation and the status-check control, and disabled sharing controls at both requested window sizes. The same ten-second deadline covers both checks. Content heights may be shorter than the requested window size because of native chrome, but cannot be taller. This is startup layout proof only, not a populated native snapshot or physical input test.

### Accessibility grant after a development reinstall

The user reported an enabled MonHop Accessibility switch while the installed app returned denied. The API was the correct `AXIsProcessTrusted` query. Native `tccd` logs identified an exact code-requirement mismatch: the saved grant required the previous ad-hoc `f28161a1…` cdhash, while the current installed executable had `37b119d0…`. This is a signing-identity failure, not evidence that the permission check should be bypassed or reported as granted.

On macOS 26.6.2, the native signing test imported a dedicated nonextractable RSA key with signing access restricted to codesign, verified it from a fresh process, and rejected duplicate setup without replacing it. Two app copies with different bundled resources produced different cdhash values and the same certificate-bound designated requirement. Both a wrong-certificate requirement and an ad-hoc replacement failed verification with exit 3. The Keychain search list remained unchanged. No certificate trust or TCC settings were changed. Eight script tests cover certificate validity and negative cases alongside the existing workspace gates.

Physical permission validation remains pending: manually re-add the installed app in the affected privacy pane and check its actual access, then repeat a rebuild/reinstall to prove permission persistence. Static signing checks do not establish a TCC grant, and unit tests never authorize input sharing.

## Native test matrix

| Direction | Required host | Status |
|---|---|---|
| Windows -> Windows local diagnostic | Windows, foreground test window | Physical categories observed, but the controlled run failed focus; injection pending |
| Windows -> macOS remote | Both paired physical hosts | Blocked on M2 native checks and M3/M4 implementation |
| macOS -> macOS local diagnostic | Mac with explicit Accessibility permission | Native build, display enumeration, physical category capture, self-process delivery and tap teardown checked; standalone permissions and full injection semantics pending |
| macOS -> Windows remote | Both paired physical hosts | Not enabled; follows the working Windows-to-Mac slice |

The Windows source should test Ctrl+C/V/Z, Alt+Tab, Win+D, Shift+Arrow and Ctrl+Shift+Escape where OS permissions allow. The Mac destination should test Command+C/V/Z, Command+Tab, Command+Space, Option+Arrow, Control+Arrow and Shift+Arrow. The physical Windows key must produce Command on the Mac. Never run these against a terminal, password manager, production console or an unrelated user document.

Test 125/500/1000 Hz motion, simultaneous typing, both scroll axes, repeated crossings, cables pulled during drag, peer termination with Ctrl/Meta held, suspend/resume, Wi-Fi roaming, VPN appearance, changing monitor scale, and hot-plugging displays. Capture metadata-only RTT/event/stale-motion counts. Hardware latency and subjective continuity must be measured on the real machines.

MonHop must fail local when elevated Windows targets reject SendInput. It must not affect UAC or the secure desktop. macOS tests must follow TCC rather than bypassing it, and must verify tap-disable recovery. Dimming tests cover the chord with local input, the chord forwarded to the other computer while a session routes input there, edge switching while dimmed, the overlay's exclusion from screen capture, and the Home card's switch, darkness slider and Dim now button on both platforms.


## Explicit pairing checkpoint

The new desktop pairing controller has no input command. Startup, pane navigation and native UI smoke checks do not read identity/trust storage or open a pairing socket. Pure tests cover public-code/record/message bounds, wrong/self identities, restart, duplicate/corrupt storage, readback failures, stale confirmation, cancellation around writes and a blocked store write.

Run the bounded opt-in localhost pairing probes separately from workspace tests:

```sh
cargo test -p monhop-desktop --locked pairing::tests::native_pairing_exchange_and_one_sided_failure -- --exact --ignored --nocapture
cargo test -p monhop-desktop --locked pairing::tests::native_pairing_rejects_wrong_identity_trailing_data_and_late_cancel -- --exact --ignored --nocapture
```

These use generated in-memory identities and no user storage. The first repeats TLS pairing, acknowledges both saved receipts, drains Quinn before hard socket revocation, and checks one-sided storage failure. An immediate hard-revocation negative control reproduced a lost-final-ack timeout before the drain fix. The second rejects wrong identity fields, trailing data and cancellation after the simulated local save. They are not proof of physical interface enforcement, native protected-peer persistence, or human confirmation.

Physical acceptance remains required on both installed apps: compare full fingerprints on both screens, complete an actual guarded connection, restart and recheck the saved peer, refuse a changed key, forget/re-pair without changing the local identity, and exercise cancellation, network loss and native storage denial/corruption in isolated test records. Do not corrupt or delete the user's existing local identity for testing. Pairing success must not enable input sharing.


## Sharing readiness and remembered setup checkpoint

The compiled desktop controller exposes explicit metadata inspection, Stop, and bounded local setup save/read commands. Input Enable is denied by both the setup capability and an unconditional native acceptance gate. Startup and UI diagnostics still invoke no sharing or storage command. Closing the app revokes active work and waits for native input ownership and outstanding prompt/worker completion.

Protocol version 3 added an empty, reliable receiver-first Ready message; version 5 adds the standing setup link and platform-carrying pairing codes. The runtime exchanges heartbeats during native construction, moves the existing sequences, rate credit and outstanding health challenge into input processing, and rejects early input. A three-second startup limit is independent of the unchanged 120 ms peer-health deadline. The destination claims native ownership before spawning, retains it across delayed cancellation and failed cleanup, and acknowledges activation only after the native destination reports completion. The source rechecks capture health and exact display topology before announcing readiness.

The new tests use generated in-memory identities, authenticated loopback QUIC, a delayed fake destination, and temporary preference files. They cover both source/connection roles, immediate Ready-plus-activation ordering, cancellation during construction, premature input, heartbeat/sequence/rate continuation, and protocol-version rejection. Local settings cover exact fractional geometry round-trips, wrong identities/source/interface/displays, missing/corrupt files, invalid or one-way layouts, and Stop/restart retaining settings without restoring authorization. These are software regression tests, not physical input acceptance.

Remaining acceptance requires both physical machines: native modifier/layout/drag/scroll delivery, held-input edge switching, lost network/peer, suspend/resume, changed monitors or VPN, emergency recovery, retained setup on reconnect, and measured latency/input feel. Windows is unavailable for this checkpoint, so those checks cannot be credited from loopback results. The functional preparation UI is wired to inspection and saved-layout commands. Input enablement, physical acceptance and later graphical polish remain pending.


## macOS suspend and native cancellation checkpoint

macOS sharing timers now use `mach_continuous_time` with a validated SDK timebase rather than sleep-pausing `Instant`. A zero numerator or denominator fails initialization. Invalid clock readings permanently fail the clock and its clones. Windows retains its sleep-inclusive QPC-backed `Instant`.

The explicit Mac capture/network lifetimes own IOKit sleep/wake observers on private serial dispatch queues. Callbacks latch revocation and acknowledge the two required sleep notifications without waiting for input or network cleanup. The queue finalizer, not ordinary owner Drop, closes the acknowledgement connection and frees the callback context after queued/in-flight work finishes. Observer destruction starts asynchronous cancellation and is not a synchronous completion receipt. A real-libdispatch test covers a gated callback and finalizer order without registering an IOKit observer or sleeping the machine.

Deterministic runtime tests advance continuous time while simulated uptime stays fixed. They cover late lease renewal, actor handoff during suspension, and an exact queued Pong followed by otherwise-valid input after wake. The receiver rejects both stale health renewal and further input, then releases managed input. Failed cleanup retains native ownership. Native power/network revocation shares the injector/capture latch, while a separate relay wakes/closes transport outside the callback. Stop registers and marks that native latch before returning. Successful metadata teardown joins the relay before deliberately revoking its endpoint. Enable consumes its approval revision, and every input-session completion invalidates the live inspection without deleting saved setup.

Current tests do not establish actual sleep/wake delivery, callback acknowledgement latency, resume ordering, physical input behavior, or Windows native execution. Those remain controlled two-machine tests. No new input authorization or acceptance bypass was added.


## Windows power revocation checkpoint

Explicit network preparation owns a dedicated hidden top-level window registered for suspend/resume notifications, including Modern Standby opt-in. Its message handler only marks the permanent revocation latch. The worker checks cancellation between bounded message batches and at most ten-millisecond message waits. Drop revokes before joining the worker. Callback state is thread-local rather than a foreign pointer, and is cleared before normal native teardown. The static window class has process lifetime and carries no session state.

Host-side tests exercise event mapping, registration failure, cancellation before and during startup, and a blocked teardown that cannot report completion early. The installed MSVC Rust target can type-check and lint the Windows implementation on the Mac. Neither those checks nor the fake worker tests establish real Windows registration, message ordering, Modern Standby behavior or window teardown. Those need the physical Windows machine.


## Sharing preparation UI verification

The preparation page requires a chosen physical input source and an explicit authenticated display check. It uses exact decimal display IDs, whole-edge crossings with reciprocal return routes, explicit monitor review and explicit local save/load. Restored layouts are only proposals. Loading a different network invalidates the old pairing candidate and exposes an explicit reload action. Nothing in this UI calls input Enable.

The final local checks passed 403 Rust tests with 14 opt-in tests ignored, 33 JavaScript model tests, nine signing-helper tests and 19 dependency-report tests. Locked debug/release builds, format, Clippy, audit, license and 484-package report checks passed. The real native startup/accordion check passed at 980×720 and 760×560 in 3.95 seconds without permission, identity, network or input operations.

An isolated headless Brave fixture matrix passed eight sharing cases across Mac/Windows presentation, light/dark appearance and both window sizes. It covered explicit source/display selection, exact high-u64 IDs, reciprocal crossings, review invalidation, saved proposals, ready-but-busy completion, lost status/Stop replies, a pending start outliving a failed Stop, and loading a different saved network. Source hashes did not change during testing. These use a fake native bridge and cannot establish physical sharing.

The Windows adapter passes targeted MSVC-target check and Clippy on the Mac. A broader transport cross-check stops in the ring build because the Mac lacks the Windows C runtime header `assert.h`. No dependency or safety check was weakened to work around that. The full Windows workspace still requires a build and native testing on Windows.


### Native animation check foreground requirement

The first signed-bundle installation check stopped at `pending:opening` until its unchanged ten-second deadline. A second run reproduced the failure while the diagnostic app was not active. Activating that exact diagnostic process let the same binary complete both animation checks. The explicit `--check-ui` path now requests its own window focus before sampling frames. Normal startup, permission handling and input remain unchanged. The installer refused replacement on the failed check, preserving the previous app.
