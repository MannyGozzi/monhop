# Late heartbeat and motion datagrams are dropped, never fatal

Heartbeats (Ping/Pong) and absolute motion travel as QUIC datagrams and can arrive out of
order. Since 42eb0c6 (2026-09-12) every gate drops a late one instead of ending the session:
`session_startup.rs` `receive` (stale heartbeat → `Ok(None)`), `session_source.rs`
`on_remote_frame` (stale heartbeat ignored), `session_receiver.rs` `receive_active` (stale
heartbeat, stale motion datagram, or a datagram from the epoch just left → `Ok(None)`), and
`session_health.rs` `receive_pong` (a token already superseded earns no liveness credit).
A dropped datagram is never answered, never applied, and never refreshes liveness.

Still fatal on purpose: any ordered-stream sequence or epoch violation, a Pong token that was
never sent (`UnexpectedResponse`), and 500 ms without a fresh reply (`PEER_LIVENESS`).
Before this, one reordered packet produced `[Startup: Sequence]` / `InvalidRemoteSequence` /
`InvalidSequence` and the standing reconnect started over (Mac Home: "Sharing dropped.
Reconnecting. Attempt N", close: the other computer ended it, Link 0.0 s).
