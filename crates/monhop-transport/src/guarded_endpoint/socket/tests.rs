use super::*;
use crate::crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer};
use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Wake, Waker},
    time::Duration,
};
use tokio::sync::{Notify, watch};

const LOCAL: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 10), 24800);
const PEER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 12), 24800);
const INDEX: u32 = 7;
const DEADLINE: Duration = Duration::from_secs(1);

struct Packet {
    arrival: Arrival,
    bytes: Vec<u8>,
}
#[derive(Default)]
struct Inbox {
    queue: VecDeque<io::Result<Packet>>,
    reader: Option<Waker>,
    arrivals: usize,
}
#[derive(Default)]
struct CountWake(AtomicUsize);
impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct TestIo {
    local: SocketAddrV4,
    inbox: Arc<Mutex<Inbox>>,
    target: Arc<Mutex<Inbox>>,
    receives: AtomicUsize,
    sends: AtomicUsize,
    blocked: AtomicBool,
    partial_send: AtomicBool,
    hard_send_error: AtomicBool,
    hard_write_error: AtomicBool,
    discard_sends: AtomicBool,
    receive_pending: Notify,
    write_pending: Notify,
    revoke_on_receive: Mutex<Option<RevocationSignal>>,
    revoke_on_send: Mutex<Option<RevocationSignal>>,
}

impl TestIo {
    fn new(local: SocketAddrV4, inbox: Arc<Mutex<Inbox>>, target: Arc<Mutex<Inbox>>) -> Self {
        Self {
            local,
            inbox,
            target,
            receives: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            partial_send: AtomicBool::new(false),
            hard_send_error: AtomicBool::new(false),
            hard_write_error: AtomicBool::new(false),
            discard_sends: AtomicBool::new(false),
            receive_pending: Notify::new(),
            write_pending: Notify::new(),
            revoke_on_receive: Mutex::new(None),
            revoke_on_send: Mutex::new(None),
        }
    }
    fn enqueue(&self, arrival: Arrival) {
        self.inbox.lock().unwrap().queue.push_back(Ok(Packet {
            arrival,
            bytes: vec![42; arrival.length.min(64)],
        }));
    }
}
impl DatagramIo for TestIo {
    fn poll_receive(&self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<io::Result<Arrival>> {
        self.receives.fetch_add(1, Ordering::SeqCst);
        if let Some(signal) = self.revoke_on_receive.lock().unwrap().take() {
            signal.revoke();
        }
        let mut inbox = self.inbox.lock().unwrap();
        match inbox.queue.pop_front() {
            Some(Ok(packet)) => {
                let length = packet.bytes.len().min(buffer.len());
                buffer[..length].copy_from_slice(&packet.bytes[..length]);
                Poll::Ready(Ok(packet.arrival))
            }
            Some(Err(error)) => Poll::Ready(Err(error)),
            None => {
                inbox.reader = Some(cx.waker().clone());
                self.receive_pending.notify_one();
                Poll::Pending
            }
        }
    }
    fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if let Some(signal) = self.revoke_on_send.lock().unwrap().take() {
            signal.revoke();
        }
        if self.hard_send_error.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::NetworkDown.into());
        }
        if self.blocked.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if self.partial_send.load(Ordering::SeqCst) {
            return Ok(buffer.len() - 1);
        }
        if self.discard_sends.load(Ordering::SeqCst) {
            return Ok(buffer.len());
        }
        let reader = {
            let mut inbox = self.target.lock().unwrap();
            inbox.arrivals += 1;
            inbox.queue.push_back(Ok(Packet {
                bytes: buffer.to_vec(),
                arrival: Arrival {
                    length: buffer.len(),
                    source: self.local,
                    destination: *peer.ip(),
                    interface_index: INDEX,
                },
            }));
            inbox.reader.take()
        };
        if let Some(reader) = reader {
            reader.wake();
        }
        Ok(buffer.len())
    }
    fn writable(self: Arc<Self>) -> WritableFuture {
        Box::pin(async move {
            if self.hard_write_error.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::NetworkDown.into());
            }
            if self.blocked.load(Ordering::SeqCst) {
                self.write_pending.notify_one();
                pending().await
            } else {
                Ok(())
            }
        })
    }
    fn local_addr(&self) -> io::Result<SocketAddrV4> {
        Ok(self.local)
    }
}

fn fixture() -> Arc<GuardedSocket<TestIo>> {
    Arc::new(GuardedSocket {
        io: Arc::new(TestIo::new(LOCAL, Arc::default(), Arc::default())),
        local: LOCAL,
        peer: PEER,
        interface_index: INDEX,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    })
}
fn arrival() -> Arrival {
    Arrival {
        length: 4,
        source: PEER,
        destination: *LOCAL.ip(),
        interface_index: INDEX,
    }
}
fn receive<I: DatagramIo>(
    socket: &GuardedSocket<I>,
    cx: &mut Context<'_>,
) -> (Poll<io::Result<usize>>, RecvMeta) {
    let mut bytes = [0; 128];
    let mut meta = [RecvMeta::default()];
    let result = socket.poll_recv(cx, &mut [IoSliceMut::new(&mut bytes)], &mut meta);
    (result, meta[0])
}
fn transmit() -> Transmit<'static> {
    Transmit {
        destination: PEER.into(),
        ecn: Some(quinn::udp::EcnCodepoint::Ect0),
        contents: b"fixed",
        segment_size: None,
        src_ip: Some((*LOCAL.ip()).into()),
    }
}
fn kind(result: Poll<io::Result<usize>>) -> io::ErrorKind {
    match result {
        Poll::Ready(Err(error)) => error.kind(),
        _ => panic!("expected ready error"),
    }
}

#[test]
fn only_exact_peer_destination_and_arrival_interface_reach_quinn() {
    let socket = fixture();
    let bad = [
        Arrival {
            source: SocketAddrV4::new(*PEER.ip(), 24801),
            ..arrival()
        },
        Arrival {
            source: "192.168.50.13:24800".parse().unwrap(),
            ..arrival()
        },
        Arrival {
            destination: "192.168.50.11".parse().unwrap(),
            ..arrival()
        },
        Arrival {
            interface_index: INDEX + 1,
            ..arrival()
        },
        Arrival {
            length: 0,
            ..arrival()
        },
    ];
    for packet in bad {
        socket.io.enqueue(packet);
    }
    socket.io.enqueue(arrival());
    let mut cx = Context::from_waker(Waker::noop());
    let (result, meta) = receive(&socket, &mut cx);
    assert!(matches!(result, Poll::Ready(Ok(1))));
    assert_eq!(meta.addr, PEER.into());
    assert_eq!(meta.dst_ip, Some((*LOCAL.ip()).into()));
    assert_eq!((meta.len, meta.stride, meta.ecn), (4, 4, None));
    assert_eq!(socket.io.receives.load(Ordering::SeqCst), 6);
    assert!(!socket.signal.is_revoked());
}

#[test]
fn unwanted_traffic_has_a_bounded_poll_budget_and_reschedules() {
    let socket = fixture();
    for _ in 0..RECEIVE_BUDGET {
        socket.io.enqueue(Arrival {
            source: LOCAL,
            ..arrival()
        });
    }
    socket.io.enqueue(arrival());
    let wake = Arc::new(CountWake::default());
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(receive(&socket, &mut cx).0.is_pending());
    assert_eq!(socket.io.receives.load(Ordering::SeqCst), RECEIVE_BUDGET);
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert!(matches!(receive(&socket, &mut cx).0, Poll::Ready(Ok(1))));
    assert!(!socket.signal.is_revoked());
}

#[test]
fn revocation_before_and_during_io_blocks_delivery_and_future_io() {
    let mut cx = Context::from_waker(Waker::noop());
    let socket = fixture();
    socket.signal.revoke();
    assert_eq!(
        kind(receive(&socket, &mut cx).0),
        io::ErrorKind::ConnectionAborted
    );
    assert!(socket.try_send(&transmit()).is_err());
    assert_eq!(socket.io.receives.load(Ordering::SeqCst), 0);
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);

    let socket = fixture();
    socket.io.enqueue(arrival());
    *socket.io.revoke_on_receive.lock().unwrap() = Some(socket.signal.clone());
    assert_eq!(
        kind(receive(&socket, &mut cx).0),
        io::ErrorKind::ConnectionAborted
    );
    assert!(socket.try_send(&transmit()).is_err());
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);

    let socket = fixture();
    *socket.io.revoke_on_send.lock().unwrap() = Some(socket.signal.clone());
    assert_eq!(
        socket.try_send(&transmit()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(socket.try_send(&transmit()).is_err());
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 1);
}

#[test]
fn native_errors_and_impossible_lengths_revoke_without_retry() {
    for error_kind in [
        io::ErrorKind::InvalidData,
        io::ErrorKind::Interrupted,
        io::ErrorKind::ConnectionReset,
    ] {
        let socket = fixture();
        socket
            .io
            .inbox
            .lock()
            .unwrap()
            .queue
            .push_back(Err(error_kind.into()));
        let mut cx = Context::from_waker(Waker::noop());
        let result = kind(receive(&socket, &mut cx).0);
        assert_eq!(
            result,
            if error_kind == io::ErrorKind::ConnectionReset {
                io::ErrorKind::ConnectionAborted
            } else {
                error_kind
            }
        );
        assert!(socket.signal.is_revoked());
        assert_eq!(socket.io.receives.load(Ordering::SeqCst), 1);
    }
    let socket = fixture();
    socket.io.enqueue(Arrival {
        length: MAX_DATAGRAM_BYTES + 1,
        ..arrival()
    });
    assert_eq!(
        kind(receive(&socket, &mut Context::from_waker(Waker::noop())).0),
        io::ErrorKind::InvalidData
    );
    assert!(socket.signal.is_revoked());
}

#[test]
fn sends_cannot_change_peer_source_segmentation_or_bounds() {
    let bad = [
        Transmit {
            destination: LOCAL.into(),
            ..transmit()
        },
        Transmit {
            destination: "[::1]:24800".parse().unwrap(),
            ..transmit()
        },
        Transmit {
            src_ip: Some("192.168.50.99".parse().unwrap()),
            ..transmit()
        },
        Transmit {
            segment_size: Some(4),
            ..transmit()
        },
        Transmit {
            contents: &[],
            ..transmit()
        },
    ];
    for tx in bad {
        let socket = fixture();
        assert_eq!(
            socket.try_send(&tx).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
        assert!(socket.signal.is_revoked());
    }
    let socket = fixture();
    let large = vec![0; MAX_DATAGRAM_BYTES + 1];
    assert!(
        socket
            .try_send(&Transmit {
                contents: &large,
                ..transmit()
            })
            .is_err()
    );
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);

    let socket = fixture();
    socket.try_send(&transmit()).unwrap();
    socket
        .try_send(&Transmit {
            src_ip: None,
            ecn: None,
            ..transmit()
        })
        .unwrap();
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 2);
    socket.io.partial_send.store(true, Ordering::SeqCst);
    assert_eq!(
        socket.try_send(&transmit()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(socket.signal.is_revoked());
}

#[test]
fn writer_pollers_do_not_replace_revocation_observer_and_can_be_reused() {
    let socket = fixture();
    let receive_wake = Arc::new(CountWake::default());
    let receive_waker = Waker::from(receive_wake.clone());
    assert!(
        receive(&socket, &mut Context::from_waker(&receive_waker))
            .0
            .is_pending()
    );
    let write_wake = Arc::new(CountWake::default());
    let write_waker = Waker::from(write_wake.clone());
    let mut cx = Context::from_waker(&write_waker);
    let mut poller = socket.clone().create_io_poller();
    for _ in 0..3 {
        assert!(matches!(
            poller.as_mut().poll_writable(&mut cx),
            Poll::Ready(Ok(()))
        ));
    }
    socket.io.blocked.store(true, Ordering::SeqCst);
    assert_eq!(
        socket.try_send(&transmit()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!socket.signal.is_revoked());
    assert!(poller.as_mut().poll_writable(&mut cx).is_pending());
    socket.signal.revoke();
    assert_eq!(receive_wake.0.load(Ordering::SeqCst), 1);
    assert_eq!(write_wake.0.load(Ordering::SeqCst), 0);
    assert!(poller.as_mut().poll_writable(&mut cx).is_pending());
}

fn paired(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .unwrap()
}
fn endpoint<I: DatagramIo>(
    socket: Arc<GuardedSocket<I>>,
    identity: &DeviceIdentity,
    peer: &DeviceIdentity,
) -> quinn::Endpoint {
    let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(SecureQuicConfig::server(identity, &paired(peer)).unwrap()),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    endpoint.set_default_client_config(SecureQuicConfig::client(identity, &paired(peer)).unwrap());
    endpoint
}
async fn connected_pair(
    client: &quinn::Endpoint,
    server: &quinn::Endpoint,
    address: SocketAddrV4,
) -> (quinn::Connection, quinn::Connection) {
    tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async {
                client
                    .connect(address.into(), LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
                    .unwrap()
            },
            async { server.accept().await.unwrap().await.unwrap() }
        )
    })
    .await
    .expect("mutually pinned handshake exceeded one second")
}

fn memory_sockets() -> (Arc<GuardedSocket<TestIo>>, Arc<GuardedSocket<TestIo>>) {
    let first_inbox: Arc<Mutex<Inbox>> = Arc::default();
    let second_inbox: Arc<Mutex<Inbox>> = Arc::default();
    let first = Arc::new(GuardedSocket {
        io: Arc::new(TestIo::new(
            LOCAL,
            first_inbox.clone(),
            second_inbox.clone(),
        )),
        local: LOCAL,
        peer: PEER,
        interface_index: INDEX,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    });
    let second = Arc::new(GuardedSocket {
        io: Arc::new(TestIo::new(PEER, second_inbox, first_inbox)),
        local: PEER,
        peer: LOCAL,
        interface_index: INDEX,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    });
    (first, second)
}

#[tokio::test]
async fn late_incoming_registration_after_driver_loss_is_rejected_without_waiting() {
    let (first, second) = memory_sockets();
    let identity_a = DeviceIdentity::generate().unwrap();
    let identity_b = DeviceIdentity::generate().unwrap();
    let endpoint_a = endpoint(first.clone(), &identity_a, &identity_b);
    let endpoint_b = endpoint(second.clone(), &identity_b, &identity_a);
    let outgoing = endpoint_a
        .connect(PEER.into(), LOCAL_TLS_SERVER_NAME)
        .unwrap();
    let incoming = tokio::time::timeout(DEADLINE, endpoint_b.accept())
        .await
        .unwrap()
        .unwrap();
    second.signal.revoke();
    assert!(
        tokio::time::timeout(DEADLINE, endpoint_b.accept())
            .await
            .unwrap()
            .is_none()
    );
    let result = super::super::register_incoming(&endpoint_b, &second.signal, incoming);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    first.signal.revoke();
    assert!(
        tokio::time::timeout(DEADLINE, outgoing)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn send_failures_close_connections_through_the_endpoint_driver() {
    for failure in 0..4 {
        let (first, second) = memory_sockets();
        let identity_a = DeviceIdentity::generate().unwrap();
        let identity_b = DeviceIdentity::generate().unwrap();
        let endpoint_a = endpoint(first.clone(), &identity_a, &identity_b);
        let endpoint_b = endpoint(second.clone(), &identity_b, &identity_a);
        let (connection_a, _connection_b) = connected_pair(&endpoint_a, &endpoint_b, PEER).await;
        let retained_connection = connection_a.clone();
        second.io.discard_sends.store(true, Ordering::SeqCst);
        match failure {
            0 => first.io.hard_send_error.store(true, Ordering::SeqCst),
            1 => first.io.partial_send.store(true, Ordering::SeqCst),
            2 => *first.io.revoke_on_send.lock().unwrap() = Some(first.signal.clone()),
            3 => {
                first.io.blocked.store(true, Ordering::SeqCst);
                first.io.hard_write_error.store(true, Ordering::SeqCst);
            }
            _ => unreachable!(),
        }
        connection_a
            .send_datagram(b"fixed send failure probe".to_vec().into())
            .unwrap();
        tokio::time::timeout(DEADLINE, connection_a.closed())
            .await
            .expect("send failure stranded a live connection driver");
        assert!(first.signal.is_revoked());
        assert!(retained_connection.close_reason().is_some());
        assert!(
            connection_a
                .send_datagram(b"fixed".to_vec().into())
                .is_err()
        );
        assert!(
            tokio::time::timeout(DEADLINE, endpoint_a.accept())
                .await
                .unwrap()
                .is_none()
        );
        second.signal.revoke();
    }
}

#[tokio::test]
async fn idle_revocation_fails_connections_accepts_and_pending_writers_without_peer_ack() {
    let (first, second) = memory_sockets();
    let identity_a = DeviceIdentity::generate().unwrap();
    let identity_b = DeviceIdentity::generate().unwrap();
    let endpoint_a = endpoint(first.clone(), &identity_a, &identity_b);
    let endpoint_b = endpoint(second.clone(), &identity_b, &identity_a);
    let (connection_a, _connection_b) = connected_pair(&endpoint_a, &endpoint_b, PEER).await;

    second.io.discard_sends.store(true, Ordering::SeqCst);
    first.io.blocked.store(true, Ordering::SeqCst);
    connection_a
        .send_datagram(b"fixed bounded probe".to_vec().into())
        .unwrap();
    tokio::time::timeout(DEADLINE, first.io.write_pending.notified())
        .await
        .unwrap();
    {
        let mut old_notice = Box::pin(first.io.receive_pending.notified());
        let _ = old_notice
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
    }
    first
        .io
        .inbox
        .lock()
        .unwrap()
        .reader
        .clone()
        .unwrap()
        .wake();
    tokio::time::timeout(DEADLINE, first.io.receive_pending.notified())
        .await
        .unwrap();
    assert!(first.io.inbox.lock().unwrap().queue.is_empty());
    let arrivals_before = first.io.inbox.lock().unwrap().arrivals;
    let before_sends = first.io.sends.load(Ordering::SeqCst);
    first.signal.revoke();
    tokio::time::timeout(DEADLINE, connection_a.closed())
        .await
        .expect("idle connection did not close");
    assert!(
        tokio::time::timeout(DEADLINE, endpoint_a.accept())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        endpoint_a.connect(PEER.into(), LOCAL_TLS_SERVER_NAME),
        Err(quinn::ConnectError::EndpointStopping)
    ));
    assert_eq!(first.io.sends.load(Ordering::SeqCst), before_sends);
    assert_eq!(first.io.inbox.lock().unwrap().arrivals, arrivals_before);
    second.signal.revoke();
}

#[tokio::test]
async fn idle_listener_revocation_wakes_without_traffic_or_protocol_timers() {
    let socket = fixture();
    let identity = DeviceIdentity::generate().unwrap();
    let peer = DeviceIdentity::generate().unwrap();
    let endpoint = endpoint(socket.clone(), &identity, &peer);
    tokio::time::timeout(DEADLINE, socket.io.receive_pending.notified())
        .await
        .unwrap();
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
    socket.signal.revoke();
    assert!(
        tokio::time::timeout(DEADLINE, endpoint.accept())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
}

struct ObservedIo<I> {
    inner: Arc<I>,
    pending: Notify,
}
impl<I: DatagramIo> DatagramIo for ObservedIo<I> {
    fn poll_receive(&self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<io::Result<Arrival>> {
        let result = self.inner.poll_receive(cx, buffer);
        if result.is_pending() {
            self.pending.notify_one();
        }
        result
    }
    fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
        self.inner.try_send_to(buffer, peer)
    }
    fn writable(self: Arc<Self>) -> WritableFuture {
        self.inner.clone().writable()
    }
    fn local_addr(&self) -> io::Result<SocketAddrV4> {
        self.inner.local_addr()
    }
}

#[tokio::test]
async fn revocation_wakes_a_pending_handshake_without_waiting_for_its_timeout() {
    let socket = fixture();
    let local_identity = DeviceIdentity::generate().unwrap();
    let peer_identity = DeviceIdentity::generate().unwrap();
    let endpoint = endpoint(socket.clone(), &local_identity, &peer_identity);
    let connecting = endpoint
        .connect(PEER.into(), LOCAL_TLS_SERVER_NAME)
        .unwrap();
    tokio::time::timeout(DEADLINE, socket.io.receive_pending.notified())
        .await
        .unwrap();
    socket.signal.revoke();
    assert!(
        tokio::time::timeout(DEADLINE, connecting)
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        tokio::time::timeout(DEADLINE, endpoint.accept())
            .await
            .unwrap()
            .is_none()
    );
}

/// The standing side's connection first, whichever side dials.
async fn session(
    standing: &quinn::Endpoint,
    peer: &quinn::Endpoint,
    standing_dials: bool,
) -> (quinn::Connection, quinn::Connection) {
    if standing_dials {
        connected_pair(standing, peer, PEER).await
    } else {
        let (theirs, ours) = connected_pair(peer, standing, LOCAL).await;
        (ours, theirs)
    }
}

/// Crosses what a session uses: the handshake's bidirectional stream, then an input datagram.
async fn delivers(from: &quinn::Connection, to: &quinn::Connection) {
    tokio::time::timeout(DEADLINE, async {
        let (mut sent, _) = from.open_bi().await.unwrap();
        sent.write_all(b"hello").await.unwrap();
        sent.finish().unwrap();
        let (_, mut received) = to.accept_bi().await.unwrap();
        assert_eq!(received.read_to_end(64).await.unwrap(), b"hello");
        from.send_datagram(b"input".to_vec().into()).unwrap();
        assert_eq!(to.read_datagram().await.unwrap().as_ref(), b"input");
    })
    .await
    .expect("a stream and a datagram crossed within a second");
}

/// The pausing side closes, revokes its socket and comes back on a fresh one at the same address;
/// the standing endpoint serves that next session while the ended one's close still drains.
#[tokio::test]
async fn a_standing_endpoint_serves_the_next_session_while_the_paused_close_drains() {
    use crate::session::SESSION_ENDED_REASON;
    for standing_dials in [true, false] {
        let (standing_socket, paused_socket) = memory_sockets();
        let ours_id = DeviceIdentity::generate().unwrap();
        let theirs_id = DeviceIdentity::generate().unwrap();
        let standing = endpoint(standing_socket.clone(), &ours_id, &theirs_id);
        let paused = endpoint(paused_socket.clone(), &theirs_id, &ours_id);
        let (ours, theirs) = session(&standing, &paused, standing_dials).await;
        delivers(&theirs, &ours).await;

        theirs.close(0_u32.into(), SESSION_ENDED_REASON);
        let ended = tokio::time::timeout(DEADLINE, ours.closed()).await.unwrap();
        assert!(matches!(
            ended,
            quinn::ConnectionError::ApplicationClosed(close)
                if close.reason.as_ref() == SESSION_ENDED_REASON
        ));
        drop(ours);
        paused_socket.signal.revoke();
        drop((theirs, paused));
        // One poll before this task yields: the drain timer cannot have run in between.
        let mut idle = std::pin::pin!(standing.wait_idle());
        assert!(
            idle.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the next attempt starts while the ended session's close still drains"
        );

        let resumed_socket = Arc::new(GuardedSocket {
            io: Arc::new(TestIo::new(
                PEER,
                paused_socket.io.inbox.clone(),
                paused_socket.io.target.clone(),
            )),
            local: PEER,
            peer: LOCAL,
            interface_index: INDEX,
            lifetime: watch::Sender::new(()),
            signal: RevocationSignal::default(),
        });
        let resumed = endpoint(resumed_socket.clone(), &theirs_id, &ours_id);
        let (ours, theirs) = session(&standing, &resumed, standing_dials).await;
        delivers(&theirs, &ours).await;
        delivers(&ours, &theirs).await;
        assert!(!standing_socket.signal.is_stopping());
        standing_socket.signal.revoke();
        resumed_socket.signal.revoke();
    }
}

/// Quinn keeps the socket in its driver tasks past the last handle, so a rebind of the pinned port
/// waits for the socket itself, not for that drop.
#[tokio::test]
async fn the_socket_outlives_its_last_handle_until_the_drivers_run() {
    let (ours_socket, theirs_socket) = memory_sockets();
    let mut lifetime = ours_socket.lifetime();
    let ours_id = DeviceIdentity::generate().unwrap();
    let theirs_id = DeviceIdentity::generate().unwrap();
    let ours = endpoint(ours_socket, &ours_id, &theirs_id);
    let theirs = endpoint(theirs_socket, &theirs_id, &ours_id);
    let (dialed, accepted) = connected_pair(&ours, &theirs, PEER).await;
    delivers(&dialed, &accepted).await;
    ours.close(0_u32.into(), b"exchange complete");
    tokio::time::timeout(DEADLINE, ours.wait_idle())
        .await
        .expect("the closed connection drained");

    drop((dialed, ours));
    assert!(
        lifetime.has_changed().is_ok(),
        "the endpoint driver still holds the socket when the last handle drops"
    );
    tokio::time::timeout(DEADLINE, async {
        while lifetime.changed().await.is_ok() {}
    })
    .await
    .expect("the socket closed once the drivers ran");
    drop((accepted, theirs));
}

#[tokio::test]
#[ignore = "explicit localhost-only guarded QUIC probe; does not authorize a physical network"]
async fn native_loopback_guarded_quic_delivers_and_revokes() {
    #[cfg(target_os = "macos")]
    use monhop_platform_macos as platform;
    #[cfg(windows)]
    use monhop_platform_windows as platform;
    let index = platform::network::enumerate_adapters()
        .unwrap()
        .into_iter()
        .find(|adapter| adapter.address == Ipv4Addr::LOCALHOST)
        .expect("literal localhost interface")
        .index;
    let make_io = || {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        platform::network::restrict_udp_interface(&socket, index).unwrap();
        platform::udp_receive::UdpReceiver::configure(socket)
            .unwrap()
            .into_async()
            .unwrap()
    };
    let io_a = make_io();
    let io_b = make_io();
    let local_a = io_a.local_addr().unwrap();
    let local_b = io_b.local_addr().unwrap();
    // Only this private test fixture admits loopback. Production construction requires NetworkLock.
    let first = Arc::new(GuardedSocket {
        io: Arc::new(io_a),
        local: local_a,
        peer: local_b,
        interface_index: index,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    });
    let second = Arc::new(GuardedSocket {
        io: Arc::new(io_b),
        local: local_b,
        peer: local_a,
        interface_index: index,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    });
    let identity_a = DeviceIdentity::generate().unwrap();
    let identity_b = DeviceIdentity::generate().unwrap();
    let endpoint_a = endpoint(first.clone(), &identity_a, &identity_b);
    let endpoint_b = endpoint(second.clone(), &identity_b, &identity_a);
    let idle_io = make_io();
    let idle_address = idle_io.local_addr().unwrap();
    let idle = Arc::new(GuardedSocket {
        io: Arc::new(ObservedIo {
            inner: Arc::new(idle_io),
            pending: Notify::new(),
        }),
        local: idle_address,
        peer: local_b,
        interface_index: index,
        lifetime: watch::Sender::new(()),
        signal: RevocationSignal::default(),
    });
    let idle_endpoint = endpoint(idle.clone(), &identity_a, &identity_b);
    tokio::time::timeout(DEADLINE, idle.io.pending.notified())
        .await
        .unwrap();
    idle.signal.revoke();
    assert!(
        tokio::time::timeout(DEADLINE, idle_endpoint.accept())
            .await
            .unwrap()
            .is_none()
    );

    let (connection_a, connection_b) = connected_pair(&endpoint_a, &endpoint_b, local_b).await;
    connection_a
        .send_datagram(b"MonHop guarded native probe".to_vec().into())
        .unwrap();
    let received = tokio::time::timeout(DEADLINE, connection_b.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received[..], b"MonHop guarded native probe");
    first.signal.revoke();
    tokio::time::timeout(DEADLINE, connection_a.closed())
        .await
        .expect("native guarded endpoint did not revoke");
    assert!(
        tokio::time::timeout(DEADLINE, endpoint_a.accept())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        endpoint_a.connect(local_b.into(), LOCAL_TLS_SERVER_NAME),
        Err(quinn::ConnectError::EndpointStopping)
    ));
    second.signal.revoke();
}
