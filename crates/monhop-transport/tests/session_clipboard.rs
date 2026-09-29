//! The clipboard link on negotiated loopback Share connections: the real link on both ends, or
//! the link against a raw quinn peer that writes whatever bytes a case needs. The links run on
//! their own runtime thread, as the app's clipboard IO thread does, apart from the runtime that
//! drives the endpoints and the control stream.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use monhop_core::{DisplayId, Platform, Point};
use monhop_protocol::{
    Capabilities, ControlPermissions, DisplayDescription, DisplayTopology, Frame, Message,
    SessionPurpose,
    clipboard::{
        CLIPBOARD_HEADER_LEN, ClipboardHeader, ClipboardHeaderError, ClipboardKind,
        ClipboardPngError, MAX_CLIPBOARD_TEXT, decode_header, encode_header,
    },
};
use monhop_transport::{
    crypto::{
        CertificateFingerprint, DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig,
        VerifiedPeer,
    },
    session_clipboard::{
        ClipboardLink, ClipboardLinkConfig, ClipboardNote, ClipboardSink, InboundClipboard,
        NotSent, OutboundClipboard, Refusal, Violation, attach,
    },
    session_handshake::{HandshakeConfig, NegotiatedSession, negotiate},
    session_wire::{WRITE_DEADLINE, write_frame},
};
use tokio::{
    sync::{Notify, watch},
    time::{Instant, timeout},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Far longer than a loopback round trip: whatever has not happened by then is not happening.
const QUIET: Duration = Duration::from_millis(200);
/// The concurrent unidirectional streams `attach` grants.
const GRANTED_STREAMS: usize = 2;
/// The receiver admits 16 streams per this window.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// More than the 16 KiB stream window, so writing it proves the link admitted the header.
const PAST_THE_WINDOW: usize = 20 * 1024;
const MIB: usize = 1024 * 1024;

#[derive(Debug)]
enum Event {
    Received(InboundClipboard),
    PeerState([u8; 32], bool),
    Note([u8; 32], ClipboardNote),
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Event>>,
    changed: Notify,
}

impl ClipboardSink for Recorder {
    fn received(&self, item: InboundClipboard) {
        self.push(Event::Received(item));
    }

    fn peer_state(&self, peer: CertificateFingerprint, enabled: bool) {
        self.push(Event::PeerState(*peer.as_bytes(), enabled));
    }

    fn note(&self, peer: CertificateFingerprint, note: ClipboardNote) {
        self.push(Event::Note(*peer.as_bytes(), note));
    }
}

impl Recorder {
    fn push(&self, event: Event) {
        self.events.lock().expect("recorder lock").push(event);
        self.changed.notify_waiters();
    }

    fn mark(&self) -> usize {
        self.events.lock().expect("recorder lock").len()
    }

    fn count(&self, mut matches: impl FnMut(&Event) -> bool) -> usize {
        let events = self.events.lock().expect("recorder lock");
        events.iter().filter(|event| matches(event)).count()
    }

    /// Waits for the first event at or after `mark` that `select` picks.
    async fn wait_after<T>(&self, mark: usize, mut select: impl FnMut(&Event) -> Option<T>) -> T {
        timeout(EVENT_TIMEOUT, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let found = {
                    let events = self.events.lock().expect("recorder lock");
                    events.iter().skip(mark).find_map(&mut select)
                };
                if let Some(found) = found {
                    return found;
                }
                changed.await;
            }
        })
        .await
        .expect("the expected clipboard event arrived")
    }

    /// Returns the peer the note was reported for.
    async fn wait_note(&self, mark: usize, expected: ClipboardNote) -> [u8; 32] {
        self.wait_after(mark, |event| match event {
            Event::Note(peer, note) if *note == expected => Some(*peer),
            _ => None,
        })
        .await
    }

    async fn wait_peer_state(&self, mark: usize, enabled: bool) -> [u8; 32] {
        self.wait_after(mark, |event| match event {
            Event::PeerState(peer, state) if *state == enabled => Some(*peer),
            _ => None,
        })
        .await
    }

    async fn wait_received(&self, mark: usize) -> (ClipboardKind, Vec<u8>, u64, [u8; 32]) {
        self.wait_after(mark, |event| match event {
            Event::Received(item) => Some((
                item.kind,
                item.bytes.clone(),
                item.sequence,
                *item.peer.as_bytes(),
            )),
            _ => None,
        })
        .await
    }

    fn received(&self) -> usize {
        self.count(|event| matches!(event, Event::Received(_)))
    }

    fn violations(&self) -> usize {
        self.count(|event| {
            matches!(
                event,
                Event::Note(
                    _,
                    ClipboardNote::Refused {
                        reason: Refusal::Violation(_),
                        ..
                    }
                )
            )
        })
    }

    fn disabled(&self) -> usize {
        self.count(|event| matches!(event, Event::Note(_, ClipboardNote::Disabled)))
    }

    fn sent(&self) -> usize {
        self.count(|event| matches!(event, Event::Note(_, ClipboardNote::Sent { .. })))
    }
}

/// One end's link with the switch and outbound slot the hub would own.
struct Side {
    recorder: Arc<Recorder>,
    switch: watch::Sender<bool>,
    outbound: watch::Sender<Option<Arc<OutboundClipboard>>>,
    _link: ClipboardLink,
}

impl Side {
    fn attach(
        session: &NegotiatedSession,
        peer: CertificateFingerprint,
        enabled: bool,
        runtime: &ClipboardRuntime,
    ) -> Self {
        let recorder = Arc::new(Recorder::default());
        let (switch, local_enabled) = watch::channel(enabled);
        let (outbound, outbound_watch) = watch::channel(None);
        let link = attach(
            session.connection.clone(),
            ClipboardLinkConfig {
                peer,
                epoch: session.initial_epoch,
                local_enabled,
                outbound: outbound_watch,
                sink: recorder.clone(),
            },
            &runtime.handle,
        );
        Self {
            recorder,
            switch,
            outbound,
            _link: link,
        }
    }

    fn publish(&self, kind: ClipboardKind, bytes: Vec<u8>) {
        let item = OutboundClipboard::new(kind, bytes).expect("a valid outbound item");
        self.outbound.send_replace(Some(Arc::new(item)));
    }
}

/// A current-thread runtime on its own thread, like the app's `monhop-clipboard-io`.
struct ClipboardRuntime {
    handle: tokio::runtime::Handle,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardRuntime {
    fn start() -> Self {
        let (handle_sender, handle_receiver) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("clipboard runtime");
            handle_sender
                .send(runtime.handle().clone())
                .expect("hand out the clipboard runtime");
            runtime.block_on(async {
                let _ = stopped.await;
            });
        });
        Self {
            handle: handle_receiver.recv().expect("clipboard runtime handle"),
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for ClipboardRuntime {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn header(kind: ClipboardKind, epoch: u64, sequence: u64, payload_len: u32) -> Vec<u8> {
    encode_header(&ClipboardHeader {
        kind,
        epoch,
        sequence,
        payload_len,
    })
    .to_vec()
}

/// A PNG signature and IHDR (RGBA, 8 bits) padded to `len`; only the IHDR is ever pre-checked.
fn png(width: u32, height: u32, len: usize) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    let prefix = bytes.len();
    bytes.extend((prefix..len).map(|index| (index % 251) as u8));
    bytes
}

fn state_stream(epoch: u64, sequence: u64, enabled: bool) -> Vec<u8> {
    [
        header(ClipboardKind::State, epoch, sequence, 1),
        vec![u8::from(enabled)],
    ]
    .concat()
}

/// Opens a raw stream to the link and writes `bytes`, leaving it open.
async fn raw_stream(connection: &quinn::Connection, bytes: &[u8]) -> quinn::SendStream {
    let mut stream = timeout(EVENT_TIMEOUT, connection.open_uni())
        .await
        .expect("the link grants stream credit")
        .expect("open a stream");
    stream.write_all(bytes).await.expect("write the stream");
    stream
}

/// A whole transfer; the link may stop it early, so write errors are part of the case.
async fn raw_transfer(connection: &quinn::Connection, bytes: &[u8]) -> quinn::SendStream {
    let mut stream = timeout(EVENT_TIMEOUT, connection.open_uni())
        .await
        .expect("the link grants stream credit")
        .expect("open a stream");
    if stream.write_all(bytes).await.is_ok() {
        let _ = stream.finish();
    }
    stream
}

async fn assert_stopped(stream: &quinn::SendStream) {
    let stopped = timeout(EVENT_TIMEOUT, stream.stopped())
        .await
        .expect("the link answers the stream");
    assert!(
        matches!(stopped, Ok(Some(_))),
        "the link stopped the stream: {stopped:?}"
    );
}

fn refused(kind: Option<ClipboardKind>, reason: Refusal) -> ClipboardNote {
    ClipboardNote::Refused { kind, reason }
}

fn violation(kind: Option<ClipboardKind>, violation: Violation) -> ClipboardNote {
    refused(kind, Refusal::Violation(violation))
}

#[tokio::test]
async fn without_attach_no_clipboard_stream_is_accepted() {
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        assert!(
            timeout(QUIET, pair.client.connection.open_uni())
                .await
                .is_err(),
            "a Share connection grants no unidirectional stream until attach"
        );
        assert!(
            timeout(QUIET, pair.server.connection.accept_uni())
                .await
                .is_err()
        );
        assert!(pair.client.connection.close_reason().is_none());
        assert!(pair.server.connection.close_reason().is_none());
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn dropping_the_link_withdraws_its_stream_credit() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let mut pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();
        let mut state = raw_stream(&client, &state_stream(epoch, 1, true)).await;
        state.finish().expect("finish the State stream");
        server.recorder.wait_peer_state(0, true).await;

        drop(server);
        let mut held = Vec::new();
        while let Ok(opened) = timeout(QUIET, client.open_uni()).await {
            let mut stream = opened.expect("only credit granted before the drop");
            stream
                .write_all(&[0])
                .await
                .expect("make the stream visible");
            held.push(stream);
            assert!(
                held.len() <= GRANTED_STREAMS,
                "credit is never renewed after the link drops"
            );
        }
        assert!(client.close_reason().is_none());
        assert!(pair.server.connection.close_reason().is_none());
        control_round_trip(&mut pair.client, &mut pair.server).await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn switch_states_are_exchanged_both_ways() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = Side::attach(&pair.client, pair.server_id, true, &runtime);
        assert_eq!(
            server.recorder.wait_peer_state(0, true).await,
            *pair.client_id.as_bytes()
        );
        assert_eq!(
            client.recorder.wait_peer_state(0, true).await,
            *pair.server_id.as_bytes()
        );

        let mark = client.recorder.mark();
        server.switch.send_replace(false);
        client.recorder.wait_peer_state(mark, false).await;
        let mark = client.recorder.mark();
        server.switch.send_replace(true);
        client.recorder.wait_peer_state(mark, true).await;
        let mark = server.recorder.mark();
        client.switch.send_replace(false);
        server.recorder.wait_peer_state(mark, false).await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn content_moves_only_while_both_switches_are_on() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = Side::attach(&pair.client, pair.server_id, false, &runtime);
        server.recorder.wait_peer_state(0, false).await;
        client.recorder.wait_peer_state(0, true).await;

        server.publish(ClipboardKind::Text, b"peer is off".to_vec());
        client.publish(ClipboardKind::Text, b"sender is off".to_vec());
        tokio::time::sleep(QUIET).await;
        assert_eq!(
            (server.recorder.received(), client.recorder.received()),
            (0, 0)
        );
        assert_eq!((server.recorder.sent(), client.recorder.sent()), (0, 0));

        let mark = server.recorder.mark();
        client.switch.send_replace(true);
        server.recorder.wait_peer_state(mark, true).await;
        server.publish(ClipboardKind::Text, b"both on".to_vec());
        let (kind, bytes, sequence, peer) = client.recorder.wait_received(0).await;
        assert_eq!(
            (kind, bytes.as_slice()),
            (ClipboardKind::Text, &b"both on"[..])
        );
        assert_eq!(sequence, 2, "the attach State took sequence 1");
        assert_eq!(peer, *pair.server_id.as_bytes());
        let sent_to = server
            .recorder
            .wait_note(
                0,
                ClipboardNote::Sent {
                    kind: ClipboardKind::Text,
                    bytes: 7,
                },
            )
            .await;
        assert_eq!(sent_to, *pair.client_id.as_bytes());

        let mark = client.recorder.mark();
        server.switch.send_replace(false);
        client.recorder.wait_peer_state(mark, false).await;
        server.publish(ClipboardKind::Text, b"sender is off again".to_vec());
        client.publish(ClipboardKind::Text, b"peer is off again".to_vec());
        tokio::time::sleep(QUIET).await;
        assert_eq!(
            (server.recorder.received(), client.recorder.received()),
            (0, 1)
        );
        assert_eq!((server.recorder.sent(), client.recorder.sent()), (1, 0));
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn a_receiver_that_is_off_stops_content_right_after_the_header() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, false, &runtime);
        for sequence in 1..=5 {
            let mark = server.recorder.mark();
            let stream = raw_stream(
                &pair.client.connection,
                &header(ClipboardKind::Text, epoch, sequence, 100),
            )
            .await;
            assert_stopped(&stream).await;
            server
                .recorder
                .wait_note(
                    mark,
                    refused(Some(ClipboardKind::Text), Refusal::SwitchedOff),
                )
                .await;
        }
        assert_eq!(server.recorder.received(), 0);
        assert_eq!(
            (server.recorder.violations(), server.recorder.disabled()),
            (0, 0),
            "refusing while off is not the peer's fault"
        );
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn malformed_headers_are_refused_and_the_connection_stays_usable() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let mut pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        let oversize = header(ClipboardKind::Text, epoch, 1, MAX_CLIPBOARD_TEXT + 1);
        let mut bad_magic = header(ClipboardKind::Text, epoch, 2, 5);
        bad_magic[0] ^= 0xFF;
        let mut reserved = header(ClipboardKind::Text, epoch, 3, 5);
        reserved[CLIPBOARD_HEADER_LEN - 1] = 1;
        for (bytes, error) in [
            (oversize, ClipboardHeaderError::InvalidPayloadLength),
            (bad_magic, ClipboardHeaderError::BadMagic),
            (reserved, ClipboardHeaderError::NonZeroReservedField),
        ] {
            let mark = server.recorder.mark();
            let stream = raw_stream(&client, &bytes).await;
            assert_stopped(&stream).await;
            server
                .recorder
                .wait_note(mark, violation(None, Violation::Header(error)))
                .await;
        }

        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 4, 5), b"after".to_vec()].concat(),
        )
        .await;
        let (_, bytes, sequence, _) = server.recorder.wait_received(0).await;
        assert_eq!((bytes.as_slice(), sequence), (&b"after"[..], 4));
        assert_eq!(server.recorder.disabled(), 0);
        assert!(client.close_reason().is_none());
        assert!(pair.server.connection.close_reason().is_none());
        control_round_trip(&mut pair.client, &mut pair.server).await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn epoch_mismatch_and_bad_lengths_are_refused_and_the_connection_stays_usable() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let mut pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        let mark = server.recorder.mark();
        let stream = raw_stream(&client, &header(ClipboardKind::Text, epoch ^ 1, 1, 5)).await;
        assert_stopped(&stream).await;
        server
            .recorder
            .wait_note(
                mark,
                violation(None, Violation::Header(ClipboardHeaderError::EpochMismatch)),
            )
            .await;

        let mark = server.recorder.mark();
        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 2, 5), b"hello!".to_vec()].concat(),
        )
        .await;
        server
            .recorder
            .wait_note(
                mark,
                violation(Some(ClipboardKind::Text), Violation::TrailingBytes),
            )
            .await;

        let mark = server.recorder.mark();
        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 3, 10), b"short".to_vec()].concat(),
        )
        .await;
        server
            .recorder
            .wait_note(
                mark,
                violation(Some(ClipboardKind::Text), Violation::Truncated),
            )
            .await;

        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 4, 5), b"after".to_vec()].concat(),
        )
        .await;
        let (_, bytes, sequence, _) = server.recorder.wait_received(0).await;
        assert_eq!((bytes.as_slice(), sequence), (&b"after"[..], 4));
        assert_eq!(
            server.recorder.received(),
            1,
            "no refused stream was delivered"
        );
        assert!(client.close_reason().is_none());
        control_round_trip(&mut pair.client, &mut pair.server).await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn four_violations_disable_the_link_once() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let mut pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();
        let mut bad_magic = header(ClipboardKind::Text, epoch, 1, 5);
        bad_magic[0] ^= 0xFF;

        for _ in 0..4 {
            let mark = server.recorder.mark();
            let stream = raw_stream(&client, &bad_magic).await;
            assert_stopped(&stream).await;
            server
                .recorder
                .wait_note(
                    mark,
                    violation(None, Violation::Header(ClipboardHeaderError::BadMagic)),
                )
                .await;
        }
        assert_eq!(
            server.recorder.wait_note(0, ClipboardNote::Disabled).await,
            *pair.client_id.as_bytes()
        );

        // Credit already granted can still open streams; each is stopped unread and uncounted.
        let mut leftover = 0;
        while let Ok(opened) = timeout(QUIET, client.open_uni()).await {
            let mut stream = opened.expect("only credit granted before the link disabled");
            stream
                .write_all(&state_stream(epoch, 10, true)[..1])
                .await
                .expect("make the stream visible");
            assert_stopped(&stream).await;
            leftover += 1;
            assert!(leftover <= GRANTED_STREAMS, "credit is never renewed");
        }
        tokio::time::sleep(QUIET).await;
        assert_eq!(
            (server.recorder.violations(), server.recorder.disabled()),
            (4, 1)
        );
        assert_eq!(
            server
                .recorder
                .count(|event| matches!(event, Event::PeerState(..))),
            0,
            "nothing from a disabled peer is read"
        );
        assert!(client.close_reason().is_none());
        assert!(pair.server.connection.close_reason().is_none());
        control_round_trip(&mut pair.client, &mut pair.server).await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn a_reset_mid_body_delivers_nothing() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        let mut stream = raw_stream(
            &client,
            &[
                header(ClipboardKind::Text, epoch, 1, 64 * 1024),
                vec![b'x'; PAST_THE_WINDOW],
            ]
            .concat(),
        )
        .await;
        stream.reset(7_u32.into()).expect("reset the stream");
        server
            .recorder
            .wait_note(0, refused(Some(ClipboardKind::Text), Refusal::PeerReset))
            .await;

        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 2, 4), b"next".to_vec()].concat(),
        )
        .await;
        let (_, bytes, sequence, _) = server.recorder.wait_received(0).await;
        assert_eq!((bytes.as_slice(), sequence), (&b"next"[..], 2));
        assert_eq!(server.recorder.received(), 1);
        assert_eq!(
            server.recorder.violations(),
            0,
            "a reset is not a violation"
        );
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn a_newer_outbound_item_resets_the_transfer_in_flight() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();
        client.set_max_concurrent_uni_streams(2_u32.into());

        let mut attach_state = client.accept_uni().await.expect("the attach State");
        assert_eq!(
            attach_state.read_to_end(64).await.expect("read the State"),
            state_stream(epoch, 1, true)
        );
        let mut state = raw_stream(&client, &state_stream(epoch, 1, true)).await;
        state.finish().expect("finish the State stream");
        server.recorder.wait_peer_state(0, true).await;

        let first_len = 4 * MIB;
        server.publish(ClipboardKind::Png, png(512, 512, first_len));
        let mut first = client.accept_uni().await.expect("the first transfer");
        let mut first_header = [0; CLIPBOARD_HEADER_LEN];
        first
            .read_exact(&mut first_header)
            .await
            .expect("read its header");
        let decoded = decode_header(&first_header, epoch).expect("a valid header");
        assert_eq!(
            (decoded.kind, decoded.sequence, decoded.payload_len as usize),
            (ClipboardKind::Png, 2, first_len)
        );
        let mut started = vec![0; 4096];
        first
            .read_exact(&mut started)
            .await
            .expect("read the start of its body");

        server.publish(ClipboardKind::Text, b"second wins".to_vec());
        server
            .recorder
            .wait_note(
                0,
                ClipboardNote::NotSent {
                    kind: ClipboardKind::Png,
                    bytes: first_len as u32,
                    reason: NotSent::Superseded,
                },
            )
            .await;
        loop {
            match first.read_chunk(usize::MAX, true).await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("a superseded transfer must not finish"),
                Err(quinn::ReadError::Reset(_)) => break,
                Err(error) => panic!("unexpected read failure: {error:?}"),
            }
        }

        let mut second = client.accept_uni().await.expect("the second transfer");
        assert_eq!(
            second.read_to_end(1024).await.expect("read it whole"),
            [
                header(ClipboardKind::Text, epoch, 3, 11),
                b"second wins".to_vec()
            ]
            .concat()
        );
        server
            .recorder
            .wait_note(
                0,
                ClipboardNote::Sent {
                    kind: ClipboardKind::Text,
                    bytes: 11,
                },
            )
            .await;
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn a_newer_inbound_transfer_aborts_the_older_read() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        let image = png(64, 64, MIB);
        let first = raw_stream(
            &client,
            &[
                header(ClipboardKind::Png, epoch, 1, MIB as u32),
                image[..PAST_THE_WINDOW].to_vec(),
            ]
            .concat(),
        )
        .await;
        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 2, 5), b"newer".to_vec()].concat(),
        )
        .await;
        assert_stopped(&first).await;
        server
            .recorder
            .wait_note(0, refused(Some(ClipboardKind::Png), Refusal::Superseded))
            .await;
        let (kind, bytes, sequence, _) = server.recorder.wait_received(0).await;
        assert_eq!(
            (kind, bytes.as_slice(), sequence),
            (ClipboardKind::Text, &b"newer"[..], 2)
        );
        tokio::time::sleep(QUIET).await;
        assert_eq!(server.recorder.received(), 1);
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn a_stale_sequence_is_dropped_without_counting_as_a_violation() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 5, 6), b"newest".to_vec()].concat(),
        )
        .await;
        server.recorder.wait_received(0).await;
        for stale in [4, 5, 1, 3, 2] {
            let mark = server.recorder.mark();
            raw_transfer(
                &client,
                &[
                    header(ClipboardKind::Text, epoch, stale, 5),
                    b"stale".to_vec(),
                ]
                .concat(),
            )
            .await;
            server
                .recorder
                .wait_note(mark, refused(Some(ClipboardKind::Text), Refusal::Stale))
                .await;
        }
        tokio::time::sleep(QUIET).await;
        assert_eq!(server.recorder.received(), 1);
        assert_eq!(
            (server.recorder.violations(), server.recorder.disabled()),
            (0, 0)
        );
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn an_oversize_png_is_stopped_at_its_ihdr() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);

        // Only the header and the 33-byte IHDR prefix of a declared 20 MiB image are ever sent.
        let stream = raw_stream(
            &pair.client.connection,
            &[
                header(ClipboardKind::Png, epoch, 1, (20 * MIB) as u32),
                png(40_000, 40_000, 33),
            ]
            .concat(),
        )
        .await;
        assert_stopped(&stream).await;
        server
            .recorder
            .wait_note(
                0,
                violation(
                    Some(ClipboardKind::Png),
                    Violation::InvalidPng(ClipboardPngError::InvalidDimensions),
                ),
            )
            .await;
        assert_eq!(server.recorder.received(), 0);
    })
    .await
    .expect("test exceeded its deadline");
}

#[tokio::test]
async fn more_than_sixteen_streams_in_ten_seconds_are_refused() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();

        for sequence in 1..=16 {
            let mark = server.recorder.mark();
            raw_transfer(&client, &state_stream(epoch, sequence, sequence % 2 == 1)).await;
            server
                .recorder
                .wait_peer_state(mark, sequence % 2 == 1)
                .await;
        }
        raw_transfer(&client, &state_stream(epoch, 17, true)).await;
        server
            .recorder
            .wait_note(0, violation(None, Violation::RateLimited))
            .await;
        assert_eq!(server.recorder.violations(), 1);
    })
    .await
    .expect("test exceeded its deadline");
}

/// A burst the path bunched up past the rate counts as one violation however many streams it
/// holds, and content flows again once the window has passed.
#[tokio::test]
async fn jitter_beyond_the_rate_window_does_not_disable_the_link() {
    let runtime = ClipboardRuntime::start();
    timeout(TEST_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let epoch = pair.server.initial_epoch.get();
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = pair.client.connection.clone();
        let started = Instant::now();

        for sequence in 1..=16 {
            let mark = server.recorder.mark();
            raw_transfer(&client, &state_stream(epoch, sequence, sequence % 2 == 1)).await;
            server
                .recorder
                .wait_peer_state(mark, sequence % 2 == 1)
                .await;
        }
        // Twice as many streams over the rate as it takes violations to disable the link.
        for sequence in 17..=24 {
            let mark = server.recorder.mark();
            let stream = raw_stream(&client, &state_stream(epoch, sequence, true)).await;
            assert_stopped(&stream).await;
            let expected = if sequence == 17 {
                violation(None, Violation::RateLimited)
            } else {
                refused(None, Refusal::RateLimited)
            };
            server.recorder.wait_note(mark, expected).await;
        }
        assert_eq!(
            (server.recorder.violations(), server.recorder.disabled()),
            (1, 0)
        );

        tokio::time::sleep_until(started + RATE_WINDOW + Duration::from_secs(1)).await;
        raw_transfer(
            &client,
            &[header(ClipboardKind::Text, epoch, 25, 5), b"later".to_vec()].concat(),
        )
        .await;
        let (kind, bytes, sequence, _) = server.recorder.wait_received(0).await;
        assert_eq!(
            (kind, bytes.as_slice(), sequence),
            (ClipboardKind::Text, &b"later"[..], 25)
        );
        assert_eq!(
            (server.recorder.violations(), server.recorder.disabled()),
            (1, 0)
        );
    })
    .await
    .expect("test exceeded its deadline");
}

/// Input must never wait on a clipboard transfer: with an 8 MiB image in flight, 500 control
/// frames (enough to refill the control stream's flow-control window several times) each finish
/// inside the session write deadline, and all of them before the image does.
#[tokio::test]
async fn control_frames_meet_the_write_deadline_during_an_8_mib_transfer() {
    let runtime = ClipboardRuntime::start();
    timeout(Duration::from_secs(60), async {
        let mut pair = negotiated_pair().await;
        let server = Side::attach(&pair.server, pair.client_id, true, &runtime);
        let client = Side::attach(&pair.client, pair.server_id, true, &runtime);
        server.recorder.wait_peer_state(0, true).await;
        client.recorder.wait_peer_state(0, true).await;

        let image = png(2048, 1024, 8 * MIB);
        let receiving = pair.client.connection.clone();
        let before = receiving.stats().udp_rx.bytes;
        server.publish(ClipboardKind::Png, image.clone());
        while receiving.stats().udp_rx.bytes - before < MIB as u64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(client.recorder.received(), 0, "the transfer is in flight");

        let frame = Frame::new(
            pair.server.initial_epoch,
            0,
            Message::DisplayTopology(wide_topology()),
        )
        .with_scope(pair.server.outbound_scope());
        let (writer, reader) = (&mut pair.server, &mut pair.client);
        let writes = async {
            let mut slowest = Duration::ZERO;
            for _ in 0..500 {
                let started = Instant::now();
                write_frame(&writer.connection, &mut writer.control_streams.send, &frame)
                    .await
                    .expect("a control write meets its deadline");
                slowest = slowest.max(started.elapsed());
            }
            slowest
        };
        let reads = async {
            let streams = &mut reader.control_streams;
            for _ in 0..500 {
                let read = streams
                    .reader
                    .read_frame(&mut streams.recv)
                    .await
                    .expect("read a control frame");
                assert_eq!(read, frame);
            }
        };
        let (slowest, ()) = tokio::join!(writes, reads);
        let in_flight = client.recorder.received() == 0;

        let (kind, bytes, _, _) = client.recorder.wait_received(0).await;
        assert!(
            kind == ClipboardKind::Png && bytes == image,
            "the image arrives intact"
        );
        assert!(
            slowest < WRITE_DEADLINE,
            "slowest control write took {slowest:?}"
        );
        assert!(
            in_flight,
            "every control write finished while the image was in flight"
        );
        server
            .recorder
            .wait_note(
                0,
                ClipboardNote::Sent {
                    kind: ClipboardKind::Png,
                    bytes: (8 * MIB) as u32,
                },
            )
            .await;
    })
    .await
    .expect("test exceeded its deadline");
}

/// Two displays with the longest names: a few hundred bytes per control frame.
fn wide_topology() -> DisplayTopology {
    let display = |id: u64, x: f64| DisplayDescription {
        id: DisplayId(id),
        name: "n".repeat(monhop_protocol::MAX_DISPLAY_NAME_BYTES),
        native_width: 1920,
        native_height: 1080,
        logical_origin: Point::new(x, 0.0),
        logical_size: Point::new(1920.0, 1080.0),
        scale_factor: 1.0,
        is_primary: id == 1,
        monitor: None,
    };
    DisplayTopology::new(vec![display(1, 0.0), display(2, 1920.0)])
        .expect("fixture topology is valid")
}

async fn control_round_trip(from: &mut NegotiatedSession, to: &mut NegotiatedSession) {
    let frame =
        Frame::new(from.initial_epoch, 0, Message::SessionReady).with_scope(from.outbound_scope());
    write_frame(&from.connection, &mut from.control_streams.send, &frame)
        .await
        .expect("write a control frame");
    let streams = &mut to.control_streams;
    assert_eq!(
        streams
            .reader
            .read_frame(&mut streams.recv)
            .await
            .expect("read a control frame"),
        frame
    );
}

struct Pair {
    client: NegotiatedSession,
    server: NegotiatedSession,
    client_id: CertificateFingerprint,
    server_id: CertificateFingerprint,
    _endpoints: [quinn::Endpoint; 2],
}

fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("generated identity has a full matching pin")
}

fn displays(id: u64) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(id),
        name: "fixture".into(),
        native_width: 100,
        native_height: 100,
        logical_origin: Point::default(),
        logical_size: Point::new(100.0, 100.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("fixture topology is valid")
}

fn share_config<'a>(
    identity: &'a DeviceIdentity,
    peer: &'a VerifiedPeer,
    (local, remote): (Platform, Platform),
    topology: &'a DisplayTopology,
) -> HandshakeConfig<'a> {
    let features =
        Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
            .expect("fixture capabilities are known");
    HandshakeConfig::new(
        identity,
        peer,
        local,
        remote,
        features,
        features,
        topology,
        ControlPermissions::BOTH,
        SessionPurpose::Share,
    )
    .expect("share handshake configuration")
}

async fn negotiated_pair() -> Pair {
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let server_identity = DeviceIdentity::generate().expect("server identity");
    let (client_pin, server_pin) = (pin(&client_identity), pin(&server_identity));
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = quinn::Endpoint::server(
        SecureQuicConfig::server(&server_identity, &client_pin).expect("server TLS configuration"),
        loopback,
    )
    .expect("loopback server endpoint");
    let mut dialer = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
    dialer.set_default_client_config(
        SecureQuicConfig::client(&client_identity, &server_pin).expect("client TLS configuration"),
    );
    let connecting = dialer
        .connect(
            listener.local_addr().expect("server address"),
            LOCAL_TLS_SERVER_NAME,
        )
        .expect("loopback connection");
    let (client_connection, server_connection) = tokio::join!(
        async { connecting.await.expect("client TLS connection") },
        async {
            listener
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("server TLS connection")
        },
    );
    let (client_displays, server_displays) = (displays(1), displays(2));
    let (client, server) = tokio::join!(
        negotiate(
            client_connection,
            share_config(
                &client_identity,
                &server_pin,
                (Platform::Windows, Platform::MacOs),
                &client_displays,
            ),
        ),
        negotiate(
            server_connection,
            share_config(
                &server_identity,
                &client_pin,
                (Platform::MacOs, Platform::Windows),
                &server_displays,
            ),
        ),
    );
    Pair {
        client: client.expect("client share session"),
        server: server.expect("server share session"),
        client_id: client_identity.fingerprint(),
        server_id: server_identity.fingerprint(),
        _endpoints: [dialer, listener],
    }
}
