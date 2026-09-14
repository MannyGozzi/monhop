# MonHop writes a bounded event log; read it before guessing why sharing stopped

Since the logging commit (2026-09-13) every build writes `monhop.log` under the app's local-data
folder: Mac `~/Library/Application Support/com.manuelgozzi.monhop/logs/`, Windows
`%LOCALAPPDATA%\com.manuelgozzi.monhop\logs\`. Home shows the path with a "Show" button
(Tauri command `reveal_logs`). Rotation: 3 files x 2 MiB (`apps/monhop-desktop/src/logging.rs`,
own `log::Log` impl; the `log` facade is already in the tree, no new packages). Our crates log at
Debug, libraries only at Warn+. The `--check-ui` smoke run never opens the log.

What it records (and where the lines come from): app banner with version, build commit
(`MONHOP_BUILD_COMMIT` from build.rs), platform, protocol; every sharing attempt, connect wait and
session end with the full `SessionFailure` Debug plus the receiver/link notes (sharing.rs standing
loop); worker exits and supervisor restarts (launch completion, lifecycle.rs); setup-link attempts,
connections and disconnects (session_link.rs); handshake outcome (session_handshake.rs); wire end
with quinn's close reason and our own close (session.rs); the source controller's first failure
with mode and capture route (session_source.rs `fail`); each early end in the source runtime
(capture stopped, geometry changed, control revision or route change not applied within 120 ms);
receiver stops (session_receiver.rs `stop`); and every revocation trigger on both platforms
(network watch interface/address/route/Wi-Fi changes, power events, native capture failures).

Invariant: events only. Never key identities, typed text, or pointer positions; the module header
says so and every new log line must keep it. Read `session end` lines on the side that ENDED the
session: the other side only ever sees "the other computer ended it" (SessionIo's Drop closes with
the same reason on every exit).
