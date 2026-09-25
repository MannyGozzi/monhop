# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

### Fixed

- MonHop no longer quits during a long network stall. A bug in the network library's queue for unsent packets crashed the app after a few seconds without a connection.

## [0.2.1] - 2026-09-24

### Fixed

- A short Wi-Fi stall no longer drops sharing for five seconds before it reconnects. When both computers paused at the same moment, one could wait for a signal it had already received.

## [0.2.0] - 2026-09-23

Both computers can control each other. **Breaking:** update both computers, pair them again, then arrange once. Saved layouts and the network choice from earlier versions are discarded.

### Added

- Either computer's keyboard and mouse can cross to the other and back. Touching the computer being controlled (a key, a click, a scroll or a deliberate mouse movement) hands it back to its own keyboard and mouse at once.
- Two switches on Home, "<A> can control <B>" and "<B> can control <A>", both on by default. Flipping one updates both computers; sharing reconnects for about two seconds.
- Mac trackpad gestures work on the other computer. On Windows a pinch zooms, a three-finger swipe up or down opens Task View, and left or right switches virtual desktops. While you control the other computer, the Mac no longer zooms or opens Mission Control behind it.

### Changed

- Home and Set up are redesigned: one compact card for the computer in use, a small Sharing pill with a green live outline, the connection status only in the top bar, and completed setup steps collapsed to one line.
- Switching between Home and Set up animates smoothly; the page transition no longer blurs the whole window.
- Sections that appear or disappear on Home, such as the control switches when sharing starts or pauses, glide in and out instead of popping. The header logo is white like the product name.
- Input stalls less on the computer being controlled. The periodic display check no longer runs on the thread that injects input, macOS no longer naps MonHop while sharing, Windows keeps its 1 ms timer while MonHop's window is hidden, and MonHop's packets ask Wi-Fi for voice priority.
- After a display change, the computer with the lower device id chooses the layout, and only after both computers' displays have held still for a second. The window comes forward only when you need to arrange.
- The pointer crosses to the other computer only when you push through the edge on purpose. Brushing the edge, reaching for a scrollbar or corner, or a quick flick stays on this computer.
- A lost input packet is recovered in about half the time.
- The display arrangement and display names update on both computers as soon as either changes.
- The menu-bar and tray icon is white while sharing, dimmed when idle, and red when sharing needs attention.

### Fixed

- Sharing no longer drops for five seconds when the Mac's Wi-Fi moves to another access point or band of the same network. Joining a network with a different name still ends it.
- Windows no longer ends sharing when its Wi-Fi starts roaming within the same network. It checks the network again when the roam finishes, and still ends sharing at once on a disconnect.
- When the Mac's input capture stops on its own, the other computer is told at once and reconnects, instead of waiting five seconds. The Mac also picks up capture renewals immediately and checks its input permission off the capture thread, so a brief hiccup no longer ends sharing.
- Double- and triple-clicks arrive on the other computer as one multi-click, even when the mouse moves slightly between the clicks.
- While the Mac controls the other computer, slow mouse movement and trackpad scroll no longer move the Mac's own cursor or scroll the app under it.
- Holding the pointer against the far edge of the other computer's screen no longer ends the session.
- Pausing sharing tells the other computer at once instead of leaving it waiting five seconds.
- The pointer no longer sticks at the edge when it comes back beside a shared monitor.
- The Sharing pill's outline animation stays on the edge while the pill resizes.
- A display change on the Mac ends sharing as a display change and resyncs, instead of as a failure with a ten-second wait.
- The computer whose displays did not change no longer reports that the other computer failed.
- A layout proposal interrupted by a brief disconnect or another display change is made again instead of leaving both computers waiting for you to arrange.
- Two computers can no longer bounce between the setup link and sharing when each remembered a different layout for its own displays.
- A display list that cannot be read for a moment while monitors reconnect is waited out instead of reported as an error.
- A computer that presents a different identity than the one paired now says to pair again, instead of retrying every two seconds forever.

### Removed

- The controlled test window, the "Input computer" choice and free arrangement mode.

## [0.1.4] - 2026-09-21

### Changed

- A display change goes straight to the layout update instead of retrying the old session first. The computer whose displays changed no longer waits ten seconds before reconnecting.
- The display-change banner appears only when the layout had to be adapted or when no saved layout fits. Switching to a remembered layout shows a brief inline "Updating the layout" line and nothing else.
- Use and Pause are one play/stop button that animates between states. Layout switches animate in the arrangement picture.
- Computer cards show the arrangement picture alone; the duplicate display lists are gone. Remembered layouts no longer carry a Remembered badge.

### Fixed

- Two computers on different MonHop versions both report the version mismatch instead of one of them dialing indefinitely.
- When another app holds the dimming shortcut at startup, MonHop keeps trying every few seconds and takes the shortcut over as soon as that app lets go, instead of giving up until the next launch. The Home card says so while it waits.

## [0.1.3] - 2026-09-21

Your computer stays connected while its displays change. The computer with the keyboard chooses the layout and sends it over; the other follows.

### Added

- Every paired computer lists the layouts made for it, remembered and saved, with the ones that fit right now marked. Forget any of them, even while that computer is away.
- Computer cards show each computer's displays: live while connected, last seen otherwise.

### Changed

- After a display change, the computer with the keyboard picks the remembered or adapted layout and applies it on both computers automatically. The other computer no longer guesses on its own, so the two never disagree.
- Pausing and choosing a computer act at once instead of waiting for the next check, and a layout that no longer fits no longer costs a ten-second retry delay.
- Clearer connection words: Connected, Sharing input, Connecting, Arranging displays. Expected transitions no longer read as failures.

### Fixed

- Reconnecting no longer stalls for up to two minutes when one computer is arranging while the other tries to share. Both computers now reach the same conclusion and join the same step.
- A layout whose crossings were all removed by a display change still shows the computer's displays instead of an empty card.

### Installation notes

- Install this version on both computers. The connection carries an arranging signal that older builds do not understand, and a computer on an older build says so instead of connecting.

## [0.1.2] - 2026-09-21

### Fixed

- Plugging a monitor back in brings back the layout you arranged for it. MonHop now recognizes a display by the monitor itself rather than the number the operating system assigns it, which changes on every reconnect on Apple silicon Macs.
- A monitor that appears while you share joins a free arrangement where the operating system places it, instead of being left out until you arrange again.
- A monitor that only moved in the operating system's arrangement keeps every crossing made for it.

## [0.1.1] - 2026-09-14

Use one keyboard and mouse across Mac and Windows. Pair once, then switch computers by crossing the screen edge.

### Added

- **Guided setup.** Pair with a short code and arrange your screens. Saved layouts return when those displays reconnect.
- **Multiple computers.** Remember sixteen paired computers and choose one to control.
- **Screen dimming.** Choose your preferred darkness and dim with a keyboard shortcut.
- **Everyday controls.** Menu bar and tray access, light and dark themes, and optional launch at login.
- **Update controls.** Check manually, opt into automatic checks, or restart to install.

### Changed

- **Smoother movement.** Mouse input arrives continuously instead of in batches.
- **Finer dimming.** One-percent adjustments, up to 99 percent darkness.

### Fixed

- macOS builds now include the permission needed to request Location access and recognize Wi-Fi networks during setup.
- Slow crossings no longer stick the pointer or send it into excluded screens.
- Sharing recovers after brief interruptions. Pausing or switching computers no longer reports a dropped connection.
- Quitting cancels unfinished downloads and waits for installation to finish. Failed Mac replacements leave the installed app intact.
- Failed Windows installer launches no longer leave MonHop stuck in shutdown. Stalled downloads stop instead of blocking sharing indefinitely.

### Security

- Sharing stays on your selected local connection with a paired computer.
- Updates require a valid signature. Opening MonHop does not capture input or change system permissions.
- Automatic checks are off until you opt in, including after older or unreadable settings. Update checks and downloads cannot run alongside sharing.

### Installation notes

- Apple silicon and Intel Macs require macOS 14+. Windows requires 64-bit Windows 10 or 11 with Microsoft Edge WebView2 Runtime installed.
- Apple notarization and Windows publisher signing are not configured. Your operating system may block or warn about installation.
- macOS permissions may need approval again after an update.

[Unreleased]: https://github.com/MannyGozzi/monhop/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.1
[0.2.0]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.0
[0.1.4]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.4
[0.1.3]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.3
[0.1.2]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.2
[0.1.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.1
