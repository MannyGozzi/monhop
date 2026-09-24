> Product name: MonHop (renamed from MonHop on 2026-09-11). The text below keeps the original name.

You are building a production-quality cross-platform application called **MonHop**.

Do not merely give me an architecture proposal. Begin implementing the repository, compiling code, running tests, fixing errors, and iterating toward a usable vertical slice.

## Goal

Build a seamless, extremely low-latency keyboard and mouse sharing system between:

* Windows 11 desktop
* Apple Silicon M5 Pro Mac running current macOS
* Multiple monitors on either computer
* One physical keyboard and mouse, normally attached to the Windows desktop
* Architecture should eventually allow either machine to be the physical input host

The experience should feel like all monitors belong to one computer.

If I move the mouse across a configured monitor edge from a Windows-owned monitor onto a Mac-owned monitor:

1. The Windows cursor stops being the active cursor.
2. The macOS cursor appears exactly at the corresponding crossing position.
3. Subsequent physical mouse input controls macOS.
4. Keyboard input goes to macOS.
5. Modifier semantics immediately become macOS-native.
6. Moving back across the configured edge returns control to Windows.
7. The transition should be visually and perceptually seamless.

Support arbitrary multi-monitor arrangements on both computers.

Example topology:

[Windows monitor 1] [Windows monitor 2] [Mac monitor 1]
[Mac monitor 2]

The topology must be configurable rather than assuming one monitor per computer.

## Non-negotiable security requirements

This software is intentionally LOCAL ONLY.

It must not communicate with the public Internet at runtime.

There must be:

* No telemetry
* No analytics
* No cloud APIs
* No account system
* No update checker beyond the user-controlled signed update check described under "Updates"
* No crash reporting service
* No remote relay
* No remote server
* No public DNS lookups
* No HTTP requests beyond that update check
* No automatic dependency/network activity at runtime

Updates: MonHop may ask one pinned HTTPS release host for a newer signed build, only when the user has automatic updates on or presses Check now, never while a sharing session is active. The request carries the platform and current version and nothing else. A downloaded build is installed only after its signature verifies against the public key built into the app, and only on a restart the user sees.
Start at login: onboarding opts the user in, and a Settings switch turns it off or on. The registration is written only when the user applies a layout or flips that switch, never merely from starting the app, and it is removed when the last paired computer is forgotten. A login launch starts MonHop hidden in the menu bar or tray and reconnects to the last used computer.

* No clipboard synchronization in the initial version
* No file transfer
* No shell commands received over the network
* No arbitrary RPC mechanism
* No remote code execution functionality

Build-time dependency downloading is obviously acceptable. Runtime Internet access is not.

Design the runtime so we can reasonably audit that network communication is restricted to the paired machine.

### Interface pinning

A user must select which physical network interface MonHop may use.

For example:

* Ethernet
* A particular Wi-Fi adapter
* A future direct Ethernet/USB-C network connection

The application must bind its listening and outbound sockets specifically to the selected local interface/address.

Do NOT simply bind to 0.0.0.0.

At connection time:

1. Determine the selected interface IP address and subnet.
2. Verify the peer address is directly on that local subnet/on-link.
3. Reject peers that would require routing through another network.
4. Reject public Internet addresses.
5. Reject connections arriving from another interface.
6. If the selected interface disappears or its network configuration materially changes, terminate the input session and fail closed.
7. Never automatically fall back to another interface.

This is specifically intended to prevent MonHop from suddenly using a VPN, another Wi-Fi network, WAN route, hotspot, etc.

Provide an optional configuration such as:

network:
interface: "Ethernet"
peer: "192.168.50.12"
allow_discovery: false

Default `allow_discovery` to false.

Manual pairing by local IP is perfectly acceptable and preferred for the first version.

We can add local discovery later.

If discovery is eventually implemented, it must be link-local only, such as mDNS/Bonjour with no gateway routing, and it must remain optional.

## Encryption and identity

All input traffic must be authenticated and encrypted.

Do NOT invent cryptography.

Use a well-audited implementation of a modern protocol.

Preferred architecture:

* QUIC
* TLS 1.3
* rustls
* mutual device authentication
* certificate/public-key pinning after pairing

Each MonHop installation should generate its own long-lived cryptographic device identity locally.

Initial pairing should explicitly authenticate the two devices.

A reasonable pairing workflow:

1. User enters the peer's local IP.
2. Devices establish an unauthenticated pairing connection.
3. Each generates/exchanges its public identity.
4. Derive a human-verifiable short authentication string from the complete handshake transcript.
5. Display the same several-word or fingerprint code on both machines.
6. User confirms they match.
7. Persist the peer identity.
8. Every future connection MUST authenticate against the pinned identity.
9. Certificate/key mismatch must fail closed and require explicit re-pairing.

Do not silently trust a new certificate.

Store private identity material using OS-protected facilities where practical:

* macOS Keychain
* Windows DPAPI/Credential Manager or appropriate Windows cryptographic storage

Never log private keys.

Never log actual keystrokes.

Logs should contain operational metadata only, such as:

"peer connected"
"latency 0.83 ms"
"monitor transition win-display-2 -> mac-display-1"

but NEVER:

"key pressed: P"
"typed: password123"

Use constant-time/audited crypto implementations and zeroize sensitive temporary buffers where appropriate.

## Transport design

Optimize this for extremely low latency on a LAN.

Target:

* Typical added input latency under 2 ms on a good local Ethernet network
* Excellent behavior over normal home Wi-Fi
* Smooth operation with 1000 Hz gaming mice
* No perceivable keyboard latency

Use separate logical handling for:

### Reliable state-changing events

These MUST NOT be lost:

* key down
* key up
* mouse button down
* mouse button up
* active-machine transition
* monitor transition
* modifier state synchronization
* emergency input release
* connection state

Use an ordered reliable QUIC stream or equivalent.

### High-frequency motion events

Mouse movement may use QUIC datagrams if that produces materially lower latency.

Lost stale motion packets should not stall newer mouse motion.

Every motion packet should include enough ordering information to discard old/reordered motion.

Scrolling needs careful evaluation. Prefer correctness over theoretical performance.

Disable unnecessary buffering and batching that increases latency.

Measure performance rather than guessing.

Build a latency/diagnostics view showing:

* RTT
* event send rate
* event receive rate
* dropped stale motion events
* transport reconnects
* active computer
* active monitor

Do not expose keystroke values in diagnostics.

## Architecture

Use Rust for the performance-sensitive core.

Create a clean workspace approximately like:

monhop/
crates/
monhop-core/
monhop-protocol/
monhop-transport/
monhop-platform-windows/
monhop-platform-macos/
monhop-ui/
apps/
monhop/
docs/
tests/

Adjust this if a better organization becomes obvious.

The hot input path must not depend on a webview or JavaScript.

A lightweight Rust UI such as egui is acceptable.

A Tauri UI is also acceptable ONLY if the actual input capture, input injection, monitor management, networking, encryption and state machine all live in native Rust and do not pass through JavaScript.

Prefer the simplest architecture with the fewest privileged components.

## Licensing and ownership

I want to own all original application code.

Use only third-party dependencies with permissive, GPL-3.0-compatible licenses, preferably:

* MIT
* Apache-2.0
* BSD
* ISC
* Zlib

Avoid GPL, AGPL, SSPL and other copyleft dependencies unless you stop and explain why they are required.

Generate:

* THIRD_PARTY_LICENSES.md
* dependency license report
* SBOM
* list of every runtime dependency

Pin dependency versions appropriately.

Keep our own code copyrightable and separated cleanly from dependencies.

## Windows input implementation

Use supported current Windows APIs.

Investigate and implement the best combination of:

* Raw Input / WM_INPUT for high-frequency physical keyboard/mouse data
* GetRawInputData / GetRawInputBuffer
* WH_KEYBOARD_LL / WH_MOUSE_LL only where interception/suppression is required
* SendInput for destination-side input synthesis
* Windows monitor APIs for display enumeration and geometry

The hook callback must do almost no work.

It should never:

* block on networking
* allocate excessively
* perform crypto
* write logs synchronously
* acquire contended locks

Push events immediately into a lock-free or extremely low-overhead queue and return.

When Windows is NOT the active destination and the physical mouse/keyboard are attached to Windows, suppress the corresponding local input so it does not simultaneously affect Windows.

Injected MonHop events must NEVER be recaptured and retransmitted.

Use appropriate injected-event flags and/or a MonHop-specific `dwExtraInfo` marker so the software can distinguish our synthetic input.

Do not create feedback loops.

Respect Windows security boundaries.

`SendInput` can be limited by UIPI when injecting into a higher-integrity application. Do not bypass Windows security.

Document this limitation.

A future optional elevated helper can be investigated, but it should NOT be required for the initial implementation and must never attempt to bypass the Windows secure desktop/UAC boundary.

## macOS input implementation

Use supported native macOS mechanisms such as:

* Core Graphics
* Quartz event services
* CGEventTap
* CGEvent
* CGEventPost
* NSScreen/Core Graphics display APIs as appropriate
* Accessibility trust APIs to detect/request required permission

The application should have a proper Apple Silicon `.app` bundle.

On first launch, detect whether Accessibility permission has been granted.

If not, show a clear UI explaining exactly why MonHop needs Accessibility permission and provide the normal macOS workflow for granting it.

Never attempt to circumvent TCC/macOS privacy protections.

### Guided setup inside the app

Make setup exceptionally clear without requiring the user to read a separate guide. Use a minimal, clean, step-by-step tutorial with small looping animations showing approximately what the user should see and do.

For macOS permissions, explain why each permission is needed, provide a button that opens the relevant System Settings pane where supported, and offer a read-only "Check again" action. Explain which application or executable the user must authorize for the actual launch context. Opening Settings does not mean permission was granted. Never change permission toggles automatically.

Guide the user through permissions, physical network-interface selection, explicit peer verification on both machines, and a controlled input test. Each step must show its verified state and the next action. Explain unavailable functionality instead of presenting an actionable control for an unimplemented step. Keep sharing disabled until the native safety requirements are satisfied.

These requirements apply to the eventual setup UI. They do not waive the M2-M4 physical-machine gates or advance graphical topology, reverse integration, or M7 polish ahead of the working vertical slice.

When macOS is the physical-input source, the event tap should be able to capture/suppress events when another machine owns the active pointer.

Injected MonHop events must be identifiable so the macOS event tap does not retransmit them.

Keep the event-tap callback extremely fast.

If macOS disables an event tap because the callback became unresponsive, detect it, re-enable it safely, and record a metadata-only diagnostic event.

## Canonical keyboard representation

Do NOT transmit characters as the primary keyboard protocol.

Create a platform-neutral physical key representation, ideally based on USB HID keyboard usages or an equivalent canonical scan-code model.

Transmit:

* physical key
* key down/up
* repeat status if needed
* current modifier state
* monotonic sequence number/timestamp

The receiving platform maps the physical key into its native keyboard event.

Initially optimize for a standard US keyboard, but architect mappings cleanly enough that other layouts can be added.

Preserve:

* letters
* numbers
* punctuation
* function keys
* arrows
* Home/End/PageUp/PageDown
* Insert/Delete
* numpad
* Caps Lock
* Shift
* Control
* Alt/Option
* Windows/Command
* common media keys where practical

Keep left/right modifier identity when the OS allows it.

## Destination-aware modifier semantics

This is extremely important.

The active monitor determines the destination OS, and the keyboard should immediately feel native to that destination.

With a Windows keyboard physically connected to the Windows PC:

When the pointer is on Windows:

* Ctrl -> Control
* Alt -> Alt
* Shift -> Shift
* Windows key -> Windows/Super

When the pointer is on macOS:

* Ctrl -> Control
* Alt -> Option
* Shift -> Shift
* Windows key -> Command

Therefore:

Windows-key + C while the pointer is on the Mac should generate Command-C on macOS.

The same physical keyboard shortcut should feel appropriate for whichever OS currently owns the pointer.

If a Mac keyboard is eventually the physical source, implement the symmetric mapping:

* Command -> Windows/Super when Windows is active
* Option -> Alt
* Control -> Control
* Shift -> Shift

Represent modifiers semantically in the core rather than scattering translation logic throughout platform code.

Add unit tests for this.

## Held key and modifier transitions

Never leave a destination with stuck keys.

Maintain an authoritative pressed-state set containing:

* keyboard keys
* modifiers
* mouse buttons

When transferring active control from machine A to machine B:

1. Reconcile currently pressed state.
2. Ensure keys/buttons that must be released on A are released.
3. Transfer appropriate held modifier/button state to B when doing so is semantically correct.
4. Continue processing subsequent physical releases correctly.
5. On disconnect, timeout, crash recovery or peer failure, synthesize releases for every MonHop-managed pressed key/button on the destination.

Test ugly cases such as:

* Hold Shift and cross screens
* Hold Windows key and cross from Windows to Mac
* Hold Command-equivalent and return
* Mouse button held during crossing
* Disconnect while Ctrl is held
* Application crashes while a button is held
* Network cable pulled during a drag

No stuck modifiers.

## Mouse behavior

Mouse movement should feel native and immediate.

Support:

* horizontal motion
* vertical motion
* left/right/middle click
* extra mouse buttons
* vertical wheel
* horizontal wheel
* high-resolution scrolling where available
* 125 Hz, 500 Hz and 1000 Hz mice

Investigate whether the most natural cross-platform feel comes from:

* transmitting raw relative deltas
* transmitting OS-accelerated deltas
* maintaining a virtual pointer
* some combination

Do not blindly choose one.

Build a proof-of-concept and test both Windows -> macOS and macOS -> Windows.

Prioritize subjective continuity and correct edge-crossing over architectural purity.

Do not let high-frequency motion generate excessive allocations.

## Monitor model

Create a canonical monitor topology model.

Each node reports something equivalent to:

Machine
Displays[]
stable identifier
name
native pixel width/height
logical width/height
origin
scale factor
refresh rate if available
primary flag

Correctly handle:

* Windows display scaling
* macOS Retina scaling
* monitors with different resolutions
* monitors with different scale factors
* monitors above/below one another
* negative desktop coordinates
* portrait monitors
* monitors physically arranged in arbitrary layouts

The user needs to be able to arrange monitor rectangles visually in settings.

Each monitor belongs to one machine.

The combined configured topology becomes a virtual desktop graph.

Example:

+----------------+----------------+----------------+
| Windows 1440p  | Windows 4K     | Mac Retina     |
+----------------+----------------+----------------+
| Mac External   |
+----------------+

When crossing a shared boundary, map the crossing point proportionally along the corresponding destination edge.

Example:

If the pointer crosses 72% down the right edge of a Windows display and the adjoining Mac display has a different height, enter the Mac display roughly 72% down its left edge.

Allow explicit gaps so unrelated monitor edges do not trigger transitions.

### Remembered layouts and visible connections

Persist each confirmed computer's display arrangement and crossing edges separately from whether sharing is active. Remember layouts across app restarts, laptop undocking and temporary disconnection. Match returning displays by stable physical identity, not just enumeration order or an old numeric display index. If identity is ambiguous, ask for confirmation instead of attaching an old crossing edge to a different display.

The graphical setup must show monitor rectangles grouped by their owning computer and clearly highlight the exact edge segments that connect them. Show direction and the mapped entry point when previewing a crossing. Unconnected edges, gaps, offline computers and last-known layouts must be visually distinct from verified live displays. Never invent remote monitors before authenticated exchange.

Display addition, removal, rotation, resolution or scaling changes invalidate affected mappings and stop the active session before more input is forwarded. Refresh both computers' display snapshots over the authenticated connection and validate a matching topology revision before enabling crossings again. Preserve the saved arrangement while disconnected, but never use stale geometry for live injection. This is MonHop layout configuration, not permission to change OS display settings or Monitor Ctrl dimming.

These graphical controls remain M5 work after the M4 physical-machine slice behaves correctly. Persistence and display-change revocation needed for M4 must be designed into that slice.

## Enable and disable without losing setup

Provide one obvious native-backed sharing control with clear Off, Connecting, On and Needs attention states. Turning sharing off must immediately revoke forwarding and suppression, restore local ownership and release managed input without waiting for the peer. Keep confirmed pairings, selected physical-interface preferences and saved monitor layouts. Forget device and Reset layout are separate, explicit actions.

Starting the app, waking the laptop, returning to a previous network or reconnecting a display must not silently enable sharing. Losing the selected network ends the active session while preserving configuration. Roaming between access points or bands of the same network does not. A later explicit enable rechecks permissions, network attachment and route, the stored peer identity and the current display topology. Saved preferences never constitute a live authorization lease.

Test disable during a held modifier or drag, peer loss, sleep/wake, changed Wi-Fi, undock/redock and app restart. Local control must return independently of network acknowledgement, previous settings must remain intact, and stale sessions or layouts must never reactivate themselves.

## Pointer ownership state machine

Build this as an explicit state machine, not scattered booleans.

For example:

LocalActive(machine, display)
Transitioning(from, to)
RemoteActive(machine, display)
Disconnected
Recovering

There must be exactly one authoritative active destination during a healthy session.

Prevent bouncing at monitor edges with a small configurable hysteresis/dead-zone.

A transition should only occur after the pointer meaningfully crosses the configured edge.

When entering the destination display, place its native pointer just inside the mapped edge so immediate opposite-direction motion can return naturally.

## Peer architecture

Both applications should run the same software.

Do not permanently hard-code "server" and "client."

A paired node can advertise:

* its monitors
* platform
* input capabilities
* protocol version
* device ID

One node is temporarily the physical input controller because the keyboard/mouse are connected there.

The protocol should support reversing this role in the future.

For version 1, make Windows-as-controller and Mac-as-receiver the first vertical slice, but do not paint the architecture into a corner.

## Protocol

Keep the protocol intentionally tiny.

Messages should be strongly typed and versioned.

Conceptual messages:

Hello
Authenticate
DisplayTopology
InputMotion
InputButton
InputScroll
InputKey
ModifierState
ActivateDisplay
ReleaseAll
Ping
Pong
Disconnect
Error

No generic "execute action" message.

No arbitrary serialized object execution.

Enforce:

* bounded message sizes
* enum validation
* sequence numbers
* protocol versions
* timeouts
* reasonable rate limits
* malformed-packet rejection

Fuzz protocol decoding.

Treat all network input as untrusted even after authentication.

## Performance requirements

The hot path should minimize:

* heap allocation
* copying
* locking
* syscalls
* context switches

Use monotonic high-resolution timestamps.

Add benchmarks for:

* event capture -> serialization
* serialization -> encryption/send
* receive -> decode
* decode -> native injection
* complete synthetic local pipeline

Build a developer latency overlay that can be toggled without logging key identities.

Stress test:

* sustained 1000 Hz mouse movement
* simultaneous keyboard presses
* rapid wheel input
* monitor crossing repeatedly
* reconnect loops

CPU usage when idle should be effectively negligible.

CPU usage during normal mouse movement should remain very low.

## Failure behavior

Fail safe.

If the network link disappears:

* stop forwarding
* release any injected keys/buttons on receiver
* restore control to the physical input machine
* show a nonintrusive disconnected indicator

The physical machine must NEVER become permanently unusable because MonHop lost connection.

Implement an emergency local escape sequence that is intercepted locally and never forwarded.

Use something obscure but reachable, such as holding both Ctrl keys plus Escape for 2 seconds.

Emergency escape must:

* terminate remote-control mode
* release all remote pressed state
* restore local input immediately
* not depend on network connectivity

Make it configurable later.

## Startup behavior

Do not immediately install aggressive global suppression on application launch.

Startup sequence:

1. Start UI/core.
2. Detect permissions.
3. Load trusted peer.
4. Bind selected local interface.
5. Connect/authenticate.
6. Exchange topology/capabilities.
7. Only then enable sharing.

On shutdown:

1. Release remote input state.
2. Restore local state.
3. Tear down hooks/taps.
4. Close transport.
5. Exit cleanly.

## Testing

Create automated tests for all platform-neutral logic.

At minimum:

* protocol round trip
* malformed protocol messages
* monitor edge mapping
* mixed scaling
* negative coordinates
* portrait displays
* modifier mapping
* held modifier transition
* disconnect release behavior
* connection state machine
* peer identity mismatch
* invalid subnet peer rejection
* selected-interface changes
* sequence number handling
* stale mouse datagram dropping

Create platform integration test utilities for Windows and macOS.

Also create a manual `TESTING.md` matrix covering:

Windows -> Windows local
Windows -> macOS remote
macOS -> macOS local
macOS -> Windows remote

Test common shortcuts:

Windows destination:
Ctrl+C
Ctrl+V
Ctrl+Z
Alt+Tab
Win+D
Shift+Arrow
Ctrl+Shift+Escape where OS permissions allow

macOS destination:
Command+C
Command+V
Command+Z
Command+Tab
Command+Space
Option+Arrow
Control+Arrow
Shift+Arrow

Remember that the physical Windows key should act as Command while macOS owns the pointer.

## Security validation

Add a `SECURITY.md` explaining the exact threat model.

Threats to consider:

* malicious device on LAN
* MITM during pairing
* malicious packet from authenticated peer
* replay
* stale peer identity
* accidental WAN routing
* VPN interface appearing
* Wi-Fi network change
* protocol parser bugs
* injected-event feedback loop
* key-state desynchronization
* logs leaking input

Add a runtime "Network Lock" diagnostics page showing:

Selected interface
Local bound address
Peer address
Peer certificate fingerprint
Encryption status
Protocol version

Also add an automated or integration test demonstrating that MonHop rejects a peer outside the selected interface's directly connected subnet.

Provide optional helper scripts/firewall instructions that can lock MonHop even further to the peer IP and MonHop port.

The application itself must still enforce this restriction. Do not rely solely on firewall configuration.

## Dependency discipline

Before adding a dependency, ask:

1. Is it actively maintained?
2. Is it necessary?
3. Is its license acceptable?
4. Does it have runtime networking behavior we don't want?
5. Is there a smaller alternative?

Avoid giant framework dependencies for trivial functionality.

Run:

* cargo fmt
* cargo clippy
* cargo test
* cargo audit or equivalent
* license audit
* dependency tree inspection

No compiler warnings in our code.

## Implementation sequence

Do not try to build every feature simultaneously.

Work in this order.

### Milestone 1: repository + core model

Implement:

* workspace
* protocol
* machine/display model
* virtual monitor topology
* modifier translation
* pointer ownership state machine
* tests

Everything here should compile and be tested.

### Milestone 2: native input prototypes

Windows:

* enumerate displays
* capture keyboard
* capture mouse
* inject keyboard
* inject mouse

macOS:

* enumerate displays
* Accessibility permission detection
* event tap
* keyboard injection
* mouse injection

Build tiny diagnostics so each side can prove capture and injection independently.

### Milestone 3: secure local transport

Implement:

* interface selection
* direct-IP connection
* subnet/on-link enforcement
* TLS 1.3/QUIC
* identity generation
* pairing
* identity pinning
* reconnect
* ping/latency

Prove that a non-local routed peer is rejected.

### Milestone 4: Windows -> Mac vertical slice

Make this actually usable:

Physical keyboard/mouse attached to Windows.

Windows captures input.

Crossing configured edge activates Mac.

Windows suppresses local events.

Encrypted MonHop connection forwards events.

Mac injects them.

Windows key becomes Command.

Moving back returns to Windows.

Disconnect restores Windows immediately.

Do not advance until this feels good.

### Milestone 5: multi-monitor topology

Support all attached displays and graphical arrangement.

### Milestone 6: Mac -> Windows symmetry

Implement reverse direction.

### Milestone 7: polish

* settings UI
* startup behavior
* tray/menu-bar integration
* latency diagnostics
* pairing UX
* packaging
* installers
* security documentation

## First task right now

Start implementing Milestone 1 immediately.

Create the actual repository structure and source files.

Do not respond only with a conceptual architecture.

While implementing, maintain these documents:

* README.md
* ARCHITECTURE.md
* SECURITY.md
* TESTING.md
* THIRD_PARTY_LICENSES.md

At the end of each development iteration:

1. Compile.
2. Run formatter.
3. Run clippy/static analysis.
4. Run tests.
5. Fix failures.
6. Briefly summarize what is working.
7. State the exact next implementation step.
8. Continue implementing if there is no blocker.

When an OS-specific assumption is uncertain, verify it against current official Apple or Microsoft documentation rather than guessing.

The priority order is:

1. Security
2. Correctness
3. Seamless input feel
4. Latency/performance
5. Reliability
6. Maintainability
7. UI appearance

Build this like software I will actually run continuously on my primary Windows workstation and Mac, not a hackathon demo.
