# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

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

[Unreleased]: https://github.com/MannyGozzi/monhop/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.2
[0.1.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.1
