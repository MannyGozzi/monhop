# Two-machine development protocol

The Mac agent drives the Windows development PC over SSH. The Windows agent (Claude Code on the PC) is
reached two ways: headless, one task per SSH call, or through a mailbox that an interactive session on
the PC reads and writes. SSH is the only channel between the machines.

## What never enters this repository

Addresses, usernames, host keys, SSH keys, tokens and machine names stay out of every committed file,
including messages that later get pasted into commits. The Mac keeps the connection in `~/.ssh/config`
under a host alias. `scripts/pc/pc.conf` (git-ignored) names that alias and the PC's checkout path.
Mailbox files live under `.claude/state/mail/` on the PC, which is git-ignored. The PC's SSH server was
set up by hand, outside the repository. It is key-only, accepts only the Mac's key, answers only the
Mac's LAN address, and listens on IPv4 only.

## When the PC changes code

- The PC never pulls on its own schedule. The Mac pushes `main` first. Every request then names the exact
  commit, and `pc.sh sync <commit>` fast-forwards the PC's `main` to it.
- `sync` refuses when the PC checkout has local changes, cannot fast-forward, or ends on any other commit.
- The PC never commits, pushes, rebases or edits tracked files. A fix found on the PC goes to the Mac as a
  mailbox message with the diff, and the Mac lands it.
- Every message and every result names the commit it applies to.

## Mac side: `scripts/pc/pc.sh`

| Command | What it does |
|---|---|
| `run [file]` | Runs PowerShell from a file or stdin in the PC checkout. |
| `sync <commit>` | Fast-forwards the PC to a pushed commit and proves `HEAD` matches. |
| `build` | Builds the workspace in release mode with the checkout's own toolchain. |
| `install` | Stops MonHop, copies the release binaries over the installed app, checks their hashes, launches it. |
| `launch` | Starts the installed MonHop in the signed-in desktop session. |
| `log [lines]` | Prints the tail of the PC's MonHop log. |
| `ask <commit> <prompt-file> [model]` | Syncs, then runs the PC's Claude Code headless on the prompt and prints its answer. Read-only by default; `PC_ASK_MODE=bypassPermissions` lets a task run commands. |
| `tell <file\|->` | Leaves a message in the PC mailbox for the interactive Windows agent. |
| `listen [seconds]` | Prints every new message the Windows agent posts. Keep it running under a monitor. |

A process started directly over SSH never appears on the desktop, so `launch` starts MonHop through a
one-shot scheduled task in the signed-in session. SSH logons on Windows have no Credential Manager access,
so tools on the PC must use file-based credentials.

## Windows side

- `scripts\pc\tell-mac.ps1 -Text '...'` (or `-File <path>`) posts a message to the Mac. Post one when a
  task finishes, when MonHop misbehaves on the PC, before and after changing anything outside the
  checkout, and when a physical test has a result.
- `scripts\pc\inbox.ps1` prints messages from the Mac that have not been shown yet. `-Follow` keeps
  printing new ones; an interactive session keeps it running under a monitor.
- A headless task from `ask` answers in its final output; it does not also post to the mailbox.

## Message rules

One message per intent, self-contained, naming its commit. Log excerpts are verbatim with their time
window. Never include credentials, key material, typed text, addresses or machine names.
