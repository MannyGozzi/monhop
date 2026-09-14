# Session threads run at elevated priority; never widen PEER_LIVENESS for load

Every session drop on 2026-09-13 lined up with a build on one of the two computers: the Mac's
receiver ended sessions with `DestinationActor(Receiver, 21)` = `ReceiverFailure::Health(
DeadlineExpired)` and a reply age of 500-506 ms while cargo clippy/test ran on the Mac; Windows
ended them with `PeerHealth(DeadlineExpired)` during its release build. Both sides log
"peer close reason before ours: None" and the source's line lands ~0.3 s before the
destination's: one event, the starved side blamed the peer.

Fix (122c765): `monhop_transport::session_threads::mark_time_sensitive()` (macOS
`pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)`, Windows
`SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`) runs at the start of the sharing worker thread
(tokio current-thread runtime: QUIC driver, heartbeats), the destination actor thread and the
native capture threads. The 500 ms deadline and the 120 ms suppression lease stay as they are; a
reappearing DeadlineExpired under load means a thread is missing the call, not a deadline to widen.
