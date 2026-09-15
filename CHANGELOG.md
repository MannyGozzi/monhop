# Changelog

All notable changes to MonHop are recorded here. The format follows Keep a Changelog and versions follow semantic versioning.

## [Unreleased]

## [0.1.1] - 2026-09-14

### Added

- Share one keyboard and mouse with another computer on the same network: the pointer crosses the seam between their screens and whatever you type follows it.
- Pair up to sixteen computers, keep one of them in use at a time, and switch between them from the home screen.
- A guided setup that picks the network connection, pairs the two computers with a short code, arranges their displays, and turns sharing on by itself once both sides apply the layout.
- Drag displays into the arrangement you actually sit in front of, name it, and mark a display as not in use so the pointer skips over it.
- Remembered arrangements: a layout comes back by itself the next time the same displays are connected, and the home screen offers to rearrange when none of them fits.
- Screen dimming, with a darkness slider, a dim-now button, and a system-wide shortcut that dims the screens you are looking at even when the keyboard belongs to the other computer.
- Automatic updates, with an off switch, a check-now button, and a restart-to-update button; a downloaded build installs when you quit.
- A settings page holding the update controls, the startup switch, and the version, license, and links to the source, the release notes, and support.
- Start MonHop when you log in, offered during setup and switchable afterwards; started that way it waits behind the menu bar or tray icon instead of opening a window.
- A menu bar and tray icon that shows whether sharing is on and offers the same controls without opening the window.
- A frosted-glass window that follows the system light and dark appearance, or the one you choose.
- A record of the last dropped connection on each computer's card, and log snapshots you can open from the window.
- A landing page describing MonHop, with the download for each platform.

### Changed

- Input leaves the moment it is captured instead of waiting for the next tick, so a fast mouse no longer arrives in visible bursts.
- Dimming reaches 99 percent and moves one percent at a time; it used to stop at 90 percent and move in steps of five.
- Each computer's status and its sharing switch sit in the card header, a computer is renamed by clicking its name, and the drop record moved into the card's details.
- Setting up a computer is one page instead of a ladder of separate steps, and the test window it used to ask for is gone.

### Fixed

- Publish downloads only after both Mac installers, the Windows installer, and all update files are assembled together.
- Crossing slowly no longer sticks the pointer at the seam, and a display the layout leaves out no longer swallows it.
- A brief stall on the network now holds the session and says it is reconnecting, instead of dropping it and starting over.
- Pausing, switching computers, or quitting tells the other computer at once, so it no longer reports a drop that never happened.
- Sharing recovers after a dropped connection instead of failing every couple of seconds until the app is restarted.
- Quitting on a Mac with the keyboard, the dock, or a logout now releases input and installs a waiting update, which only the tray quit used to do.
- A dimming shortcut pressed just as the shortcut is turned off no longer dims the screens.
- Log snapshots on Windows open even when the temporary folder is redirected elsewhere.
- Forgetting a computer removes its saved record even when the system keychain refuses to delete it.

### Security

- Sharing binds only to the network connection you select and to the computer you paired with, and input arriving from that computer is still checked before it is used.
- The only thing that leaves the local network is the update check: it runs when you allow it, never while a sharing session is live, and nothing installs before its signature is verified against the key built into the app.
- Starting the app installs no input hooks, changes no system permissions, and registers nothing to run at startup on its own.
- The encryption library named in a published advisory is pinned to its fixed version.

[Unreleased]: https://github.com/MannyGozzi/monhop/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/MannyGozzi/monhop/releases/tag/v0.1.1
