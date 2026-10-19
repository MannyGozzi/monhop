# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

### Changed

- MonHop's window is redesigned around one surface instead of the Home, Set up and Settings pages. A sidebar lists this computer, each paired computer with its own connection state, Add a computer and Settings. The middle shows your displays as one arrangement, and the right side shows whatever you select. Getting this computer ready, pairing a new computer, arranging displays and choosing who controls whom all happen in place, without switching pages.
- Arranging displays has its own Arrange mode with a bar for Apply, saved arrangements and the displays in use. Sharing still pauses while you arrange and resumes when you apply.
- Each computer now has one switch to share with it, in place of the separate sharing buttons on Home and Set up.
- Clipboard, screen dimming, mouse and trackpad, keyboard, updates, startup, the notch island, appearance and diagnostics now all live in Settings. Start at login appears only there.
- The window shows how to take your keyboard and mouse back at any time: hold both Control keys and Esc for two seconds.
- The window opens larger, at 1180 by 760, and still works down to its smallest size, where the details slide over the arrangement.

## [0.2.11] - 2026-10-17

### Fixed

- A computer that joins or leaves while the notch island is open now fades in or out in its own row instead of sliding toward the notch.
- The notch island's status dot changes color smoothly when the status changes, and the status text no longer jumps to its new color when the status changes twice in quick succession.
- The notch island now closes when a row disappears while you hold its switch with the pointer already outside, instead of staying open until the pointer passes over it again.
- An automatic update check no longer cuts off the other computer while it is in the middle of connecting, so sharing resumes on time instead of a few seconds late.

## [0.2.10] - 2026-10-17

### Changed

- The notch island now opens like macOS does. Its rows come into focus one after another from a soft blur, its content blurs away before it closes, and status changes cross through a light blur instead of popping. When the rows change while it is open, rows that stay glide into place instead of the whole island redrawing.

### Fixed

- Automatic updates now arrive when you share all the time. Before, a scheduled check only ran when sharing happened to let go of the network, which a working setup almost never does, and a missed check waited six hours. Now MonHop looks every minute for a moment when no computer is connected: while sharing is paused, no computer is switched on, or the other computer has not answered for two minutes. It never checks while a computer is connected. Time the computer slept counts toward the six hours, and a check that fails, such as one at login before Wi-Fi is up, is tried again after 30 minutes.
- Check now and Restart to update work while MonHop is still waiting for the other computer, instead of asking you to pause sharing first. Sharing resumes a moment later.

## [0.2.9] - 2026-10-17

### Added

- A notch island on Macs with a camera notch. MonHop's status sits around the camera. Connections, dimming and clipboard transfers show beside it, and pointing at it opens sharing, dimming and clipboard controls without taking focus from the app you are in. It is on by default and can be turned off in Settings. It shows only while the notched display is in use, so a closed laptop on an external display or a Mac without a notch shows nothing and has no setting for it. Windows is unchanged.

### Changed

- The window's motion stays continuous while MonHop updates it. The refresh icon, header pill, update progress, Login Items row, pairing steps and confirm buttons glide instead of jumping, and with Reduce Motion on every change is instant.

### Fixed

- The Mac window no longer reopens blank after macOS ends its page to save memory; the page reloads.
- Start MonHop when you log in now works on a Mac. macOS reports a login item that was never registered as "not found", and MonHop read that as already registered, so it never registered one. If the switch is on, turn it off and on once after updating.
- Pairing again with a computer you already paired keeps its sharing switch as you left it. Before, pairing again switched a computer you had turned off back on and started sharing with it.
- When macOS's Local Network privacy setting blocks MonHop, a failed pairing on a Mac now says so and points to System Settings, instead of blaming the code or the network.

### Security

- The TLS library behind sharing and update checks is updated to rustls 0.23.46 (RUSTSEC-2026-0346 to 0349), and the updater no longer includes zip extraction, which MonHop's own updates never used (RUSTSEC-2026-0367 to 0369).

## [0.2.8] - 2026-10-13

### Changed

- MonHop for Mac is now signed by Gozzi LLC and notarized by Apple, so it opens with a double-click instead of the "unidentified developer" block. After updating to this version, macOS may ask once more for Accessibility, Input Monitoring and Keychain access, because earlier versions were not signed with a stable identity. Later updates keep those permissions.

## [0.2.7] - 2026-10-09

### Added

- Pair with a short code: one computer shows a 12-character code such as 7KQ4-M9XR-2HTW, you type it on the other, and both switch to Paired showing the same picture, one color and three symbols. It replaces copying a long code to each computer and comparing two 64-character fingerprints. A code works once, for two minutes, and is checked with SPAKE2 bound to the encrypted connection, so another device on the network gets a single guess per code and cannot test guesses afterwards.
- Each paired computer's card shows that pairing's picture, labeled "Pairing picture" and the same on both computers. Pointing at it explains what to do if the two computers show different pictures.

### Changed

- Minimizing MonHop hides it to the tray on Windows and to the menu bar on macOS. On a Mac the Dock icon shows only while the window is open.
- Pairing a new computer needs this version on both computers. Computers already paired keep working without pairing again, and sharing still works with 0.2.6.

### Fixed

- Opening MonHop on Windows while it already runs now brings up the running window instead of starting a second copy, which fought the first for the network port and could end a live session. After an update, the new version waits for the old one to finish quitting.
- A single oversized network packet sent to MonHop's port by any device on the network no longer ends a sharing session or stops a pairing.
- In a game that locks the mouse inside its window on Windows, turning toward the other computer no longer hands the mouse over mid-game. While a program locks the mouse, the edge works as a wall, and it opens again once the game lets go, as in its menus.

## [0.2.6] - 2026-10-06

### Added

- Use Ctrl as Command on a Mac: a new switch on the Windows computer's Home screen, off by default. With it on, a Windows keyboard controlling a Mac types Ctrl+C, Ctrl+V, Ctrl+Z and other shortcuts as the Mac's Command shortcuts, and the Windows key becomes Control. Ctrl+Tab still switches tabs, Alt+Tab switches apps, Ctrl+Left, Ctrl+Right, Ctrl+Backspace and Ctrl+Delete move and delete by word, and Ctrl+Alt+0 still dims the Mac.

### Changed

- Both computers need this version to share: it does not connect to 0.2.5 or earlier.

## [0.2.5] - 2026-09-30

A chevron at the pointer when a trackpad swipe turns the page, and a sharing button that says Connecting… until the computers are connected. This version still connects to 0.2.3 and 0.2.4, so computers can update one at a time.

### Added

- A swipe between pages now shows on the computer it lands on: a chevron blooms beside the pointer, pointing the way the page went.

### Fixed

- The sharing button no longer says Sharing before the computers are connected: it reads Connecting… with its dots circling quickly until the connection is up, then settles into Sharing.

## [0.2.4] - 2026-09-30

Middle-click autoscroll and trackpad page swipes across computers, a new sharing button, and a fix for keys held down when sharing starts. This version still connects to 0.2.3, so computers can update one at a time.

### Added

- A Windows mouse controlling a Mac autoscrolls with its middle button like it does on Windows: hold and drag, or click once and move, then click or press Escape to stop. A small ring marks where it started. Turn off Middle-click autoscroll on the Mac's Home screen to keep plain middle clicks and drags.
- A Mac trackpad controlling another computer swipes between pages: a quick two-finger swipe right goes back and left goes forward, while slower sideways scrolling still just scrolls. Turn off Swipe between pages on the Mac's Home screen to keep plain sideways scrolling.

### Changed

- The Start sharing button on Home is frosted glass with two dots for the two computers, ice and emerald. They circle each other quickly while connecting and slowly while sharing, glowing over a breathing green light, and stop side by side as the pause bars when you point at the button. Its words blur from one to the next instead of sliding up.

### Fixed

- A key or mouse button held down when sharing started no longer silently stops the pointer from crossing to another computer: Home names what to release.

## [0.2.3] - 2026-09-29

Several computers, and clipboard sharing. **Update every computer together:** this version cannot connect to 0.2.2 or earlier. Pairings and saved layouts carry over, and an older MonHop started later still finds its own settings untouched.

### Added

- Share with up to eight computers at once. Switch on each paired computer you want on Home; every switched-on computer stays connected, all their displays form one arrangement, and any computer's keyboard and mouse can move across all of them, including straight from one controlled computer to another. Each computer's card shows its own status, displays and control switches, and the top bar says how many computers you are sharing with. A computer shares with another only after you switch it on there too.
- Clipboard sharing, off until you turn it on in the Clipboard card on Home. Text up to 1 MB and images up to 32 MB that you copy go to connected computers that also have it on, over the same encrypted connection as your keyboard and mouse. A green Sharing pill shows when a computer on the other end shares back. Files are never sent, items that password managers mark as private are skipped, and what arrives stays on that computer. On a Mac, macOS asks once whether MonHop may read the clipboard.

### Changed

- Arranging displays works for any number of computers: drag each computer's block, and the pointer crosses wherever two blocks touch. Each computer is drawn in its own color.
- When two computers hold different saved layouts, the newer one wins on both, whichever computer made it.
- A paired computer that is switched off or out of range no longer interrupts sharing with the others, and MonHop reconnects to it by itself when it comes back.

### Fixed

- Dragging with the middle button, and apps that follow mouse movement rather than the pointer position, now work on a Mac controlled from another computer.
- Sharing stays connected, and the saved network is kept, when Windows renumbers the network adapter.

### Security

- Every incoming connection proves its address before any encryption work, each paired computer gets a limited number of connection attempts per second, and an attempt that does not finish within 3 seconds is dropped, so a flood of attempts cannot stall sharing.

## [0.2.2] - 2026-09-24

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

[Unreleased]: https://github.com/MannyGozzi/monhop/compare/v0.2.11...HEAD
[0.2.11]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.11
[0.2.10]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.10
[0.2.9]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.9
[0.2.8]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.8
[0.2.7]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.7
[0.2.6]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.6
[0.2.5]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.5
[0.2.4]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.4
[0.2.3]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.3
[0.2.2]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.2
[0.2.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.1
[0.2.0]: https://github.com/MannyGozzi/monhop/releases/tag/v0.2.0
[0.1.4]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.4
[0.1.3]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.3
[0.1.2]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.2
[0.1.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.1
