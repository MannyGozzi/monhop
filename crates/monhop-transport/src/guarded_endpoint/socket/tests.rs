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
use tokio::sync::Notify;

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

/// How a send toward one destination fails.
#[derive(Clone, Copy)]
enum SendFailure {
    Kind(io::ErrorKind),
    Os(i32),
}

impl SendFailure {
    fn error(self) -> io::Error {
        match self {
            Self::Kind(kind) => kind.into(),
            Self::Os(code) => io::Error::from_raw_os_error(code),
        }
    }
}

struct TestIo {
    local: SocketAddrV4,
    inbox: Arc<Mutex<Inbox>>,
    target: Arc<Mutex<Inbox>>,
    /// On a shared link, sends reach the inbox of the address they name, or no one.
    routes: Vec<(SocketAddrV4, Arc<Mutex<Inbox>>)>,
    receives: AtomicUsize,
    sends: AtomicUsize,
    blocked: AtomicBool,
    partial_send: AtomicBool,
    hard_send_error: AtomicBool,
    hard_write_error: AtomicBool,
    discard_sends: AtomicBool,
    /// Sends to these destinations fail this way, and are counted in `failed_sends`.
    send_failures: Mutex<Vec<(SocketAddrV4, SendFailure)>>,
    failed_sends: AtomicUsize,
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
            routes: Vec::new(),
            receives: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            partial_send: AtomicBool::new(false),
            hard_send_error: AtomicBool::new(false),
            hard_write_error: AtomicBool::new(false),
            discard_sends: AtomicBool::new(false),
            send_failures: Mutex::default(),
            failed_sends: AtomicUsize::new(0),
            receive_pending: Notify::new(),
            write_pending: Notify::new(),
            revoke_on_receive: Mutex::new(None),
            revoke_on_send: Mutex::new(None),
        }
    }
    fn enqueue(&self, arrival: Arrival) {
        self.enqueue_bytes(arrival, vec![42; arrival.length.min(64)]);
    }
    fn enqueue_bytes(&self, arrival: Arrival, bytes: Vec<u8>) {
        self.inbox
            .lock()
            .unwrap()
            .queue
            .push_back(Ok(Packet { arrival, bytes }));
    }
    fn fail_sends_to(&self, destination: SocketAddrV4, failure: SendFailure) {
        self.send_failures
            .lock()
            .unwrap()
            .push((destination, failure));
    }
    fn heal_sends(&self) {
        self.send_failures.lock().unwrap().clear();
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
        let failure = self
            .send_failures
            .lock()
            .unwrap()
            .iter()
            .find(|(destination, _)| *destination == peer)
            .map(|(_, failure)| *failure);
        if let Some(failure) = failure {
            self.failed_sends.fetch_add(1, Ordering::SeqCst);
            return Err(failure.error());
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
        let target = if self.routes.is_empty() {
            &self.target
        } else {
            match self.routes.iter().find(|(address, _)| *address == peer) {
                Some((_, inbox)) => inbox,
                None => return Ok(buffer.len()),
            }
        };
        let reader = {
            let mut inbox = target.lock().unwrap();
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

/// A socket over `io` pinned to `local` and `members` without a lock, every member reachable.
fn pinned_socket<I>(
    io: I,
    local: SocketAddrV4,
    members: &[SocketAddrV4],
    interface_index: u32,
) -> GuardedSocket<I> {
    GuardedSocket::pinned(
        io,
        local,
        members,
        interface_index,
        Reachability::new(members.len()),
        RevocationSignal::default(),
    )
}

fn fixture() -> Arc<GuardedSocket<TestIo>> {
    Arc::new(pinned_socket(
        TestIo::new(LOCAL, Arc::default(), Arc::default()),
        LOCAL,
        &[PEER],
        INDEX,
    ))
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
    let first = Arc::new(pinned_socket(
        TestIo::new(LOCAL, first_inbox.clone(), second_inbox.clone()),
        LOCAL,
        &[PEER],
        INDEX,
    ));
    let second = Arc::new(pinned_socket(
        TestIo::new(PEER, second_inbox, first_inbox),
        PEER,
        &[LOCAL],
        INDEX,
    ));
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
    let server = Arc::new(SecureQuicConfig::server(&identity_b, &paired(&identity_a)).unwrap());
    let result = super::super::register_incoming(&endpoint_b, &second.signal, incoming, server);
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

        let resumed_socket = Arc::new(pinned_socket(
            TestIo::new(
                PEER,
                paused_socket.io.inbox.clone(),
                paused_socket.io.target.clone(),
            ),
            PEER,
            &[LOCAL],
            INDEX,
        ));
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

use super::super::{
    EndpointHandle, GroupMember, GroupSelection, GuardedEndpoint, MemberHandshakeFailure,
    native::SharedCheck,
};
use crate::{
    crypto::CertificateFingerprint,
    policy::{InterfaceKind, InterfaceSnapshot, PeerRoute, RouteSnapshot},
};

const MEMBERS: [SocketAddrV4; 3] = [
    SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 11), 24800),
    SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 12), 24800),
    SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 13), 24800),
];

/// The lock a bind on the memory link would hold: every member on-link with a direct route.
fn memory_lock(local: SocketAddrV4, members: &[SocketAddrV4]) -> NetworkLock {
    let selected = InterfaceSnapshot {
        stable_id: "memory-link".into(),
        name: "memory".into(),
        index: INDEX,
        address: *local.ip(),
        prefix_len: 24,
        kind: InterfaceKind::Ethernet,
        is_hardware: true,
        is_up: true,
        network_signature: vec![1; 32],
    };
    let route = RouteSnapshot {
        interface_index: INDEX,
        source: *local.ip(),
        next_hop: Ipv4Addr::UNSPECIFIED,
    };
    let routes: Vec<_> = members.iter().map(|member| (*member.ip(), route)).collect();
    NetworkLock::new_group(selected, &routes, false).unwrap()
}

/// One in-memory link shared by every node: a send reaches the inbox of the address it names, or
/// no one. Each socket is built as a bind builds it, admitting only its own members.
fn memory_network<const N: usize>(
    nodes: [(SocketAddrV4, &[SocketAddrV4]); N],
) -> [GuardedSocket<TestIo>; N] {
    let inboxes: Vec<(SocketAddrV4, Arc<Mutex<Inbox>>)> = nodes
        .iter()
        .map(|(address, _)| (*address, Arc::default()))
        .collect();
    nodes.map(|(local, members)| {
        let own = &inboxes
            .iter()
            .find(|(address, _)| *address == local)
            .unwrap()
            .1;
        let mut io = TestIo::new(local, own.clone(), Arc::default());
        io.routes = inboxes
            .iter()
            .filter(|(address, _)| *address != local)
            .cloned()
            .collect();
        GuardedSocket::new(
            io,
            &memory_lock(local, members),
            local,
            members,
            Reachability::new(members.len()),
            RevocationSignal::default(),
        )
        .unwrap()
    })
}

fn group(local: SocketAddrV4, members: &[(SocketAddrV4, &DeviceIdentity)]) -> GroupSelection {
    GroupSelection {
        stable_id: "memory-link".into(),
        interface_index: INDEX,
        local,
        members: members
            .iter()
            .map(|(address, identity)| GroupMember {
                address: *address,
                pin: paired(identity),
            })
            .collect(),
    }
}

fn guarded(
    socket: GuardedSocket<TestIo>,
    identity: &DeviceIdentity,
    members: &[(SocketAddrV4, &DeviceIdentity)],
) -> GuardedEndpoint {
    let local = socket.local;
    GuardedEndpoint::over_socket(socket, &group(local, members), identity).unwrap()
}

fn handshake_failure(error: &io::Error) -> &MemberHandshakeFailure {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<MemberHandshakeFailure>())
        .expect("a member's handshake failure")
}

#[test]
fn a_member_set_admits_exactly_its_members_on_the_pinned_interface() {
    let lock = memory_lock(LOCAL, &MEMBERS);
    let io = || TestIo::new(LOCAL, Arc::default(), Arc::default());
    let signal = RevocationSignal::default;
    let stranger = SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 14), 24800);
    let mut reordered = MEMBERS;
    reordered.swap(0, 2);
    let mut portless = MEMBERS;
    portless[1] = SocketAddrV4::new(*MEMBERS[1].ip(), 0);
    let mut substituted = MEMBERS;
    substituted[2] = stranger;
    for members in [
        &MEMBERS[..2],
        &[MEMBERS[0], MEMBERS[1], MEMBERS[2], stranger][..],
        &[MEMBERS[0], MEMBERS[0], MEMBERS[2]][..],
        &reordered[..],
        &portless[..],
        &substituted[..],
        &[][..],
    ] {
        let reach = Reachability::new(members.len());
        assert_eq!(
            GuardedSocket::new(io(), &lock, LOCAL, members, reach, signal())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    let reach = || Reachability::new(MEMBERS.len());
    let wrong_local = SocketAddrV4::new(*MEMBERS[0].ip(), 24800);
    assert!(GuardedSocket::new(io(), &lock, wrong_local, &MEMBERS, reach(), signal()).is_err());
    let mut revoked = lock.clone();
    let _ = revoked.revalidate::<PeerRoute>(None, &[]);
    assert_eq!(
        GuardedSocket::new(io(), &revoked, LOCAL, &MEMBERS, reach(), signal())
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionAborted
    );
    assert_eq!(
        GuardedSocket::new(io(), &lock, LOCAL, &MEMBERS, Reachability::new(2), signal())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );

    let socket = GuardedSocket::new(io(), &lock, LOCAL, &MEMBERS, reach(), signal()).unwrap();
    let member = |source| Arrival {
        source,
        ..arrival()
    };
    let refused = [
        member(SocketAddrV4::new(*MEMBERS[1].ip(), 24801)),
        member(stranger),
        member(LOCAL),
        Arrival {
            destination: *MEMBERS[1].ip(),
            ..member(MEMBERS[0])
        },
        Arrival {
            interface_index: INDEX + 1,
            ..member(MEMBERS[2])
        },
        Arrival {
            length: 0,
            ..member(MEMBERS[1])
        },
    ];
    for packet in refused {
        socket.io.enqueue(packet);
    }
    for source in MEMBERS {
        socket.io.enqueue(member(source));
    }
    let mut cx = Context::from_waker(Waker::noop());
    for source in MEMBERS {
        let (result, meta) = receive(&socket, &mut cx);
        assert!(matches!(result, Poll::Ready(Ok(1))));
        assert_eq!(meta.addr, source.into());
        assert_eq!(meta.dst_ip, Some((*LOCAL.ip()).into()));
    }
    assert_eq!(
        socket.io.receives.load(Ordering::SeqCst),
        refused.len() + MEMBERS.len()
    );
    assert!(receive(&socket, &mut cx).0.is_pending());
    assert!(!socket.signal.is_revoked());
}

#[test]
fn sends_reach_only_members_and_anything_else_revokes() {
    let [hub, first, second, third] = memory_network([
        (LOCAL, &MEMBERS[..]),
        (MEMBERS[0], &[LOCAL][..]),
        (MEMBERS[1], &[LOCAL][..]),
        (MEMBERS[2], &[LOCAL][..]),
    ]);
    for (index, member) in MEMBERS.into_iter().enumerate() {
        for _ in 0..=index {
            hub.try_send(&Transmit {
                destination: member.into(),
                ..transmit()
            })
            .unwrap();
        }
    }
    let arrivals = [&first, &second, &third].map(|node| node.io.inbox.lock().unwrap().arrivals);
    assert_eq!(arrivals, [1, 2, 3]);
    assert_eq!(hub.io.sends.load(Ordering::SeqCst), 6);
    assert!(!hub.signal.is_revoked());

    let lock = memory_lock(LOCAL, &MEMBERS);
    let fresh = || {
        GuardedSocket::new(
            TestIo::new(LOCAL, Arc::default(), Arc::default()),
            &lock,
            LOCAL,
            &MEMBERS,
            Reachability::new(MEMBERS.len()),
            RevocationSignal::default(),
        )
        .unwrap()
    };
    let bad = [
        "192.168.50.14:24800",
        "192.168.50.12:24801",
        "192.168.50.10:24800",
        "[::1]:24800",
        "[::ffff:192.168.50.12]:24800",
    ]
    .map(|destination| Transmit {
        destination: destination.parse().unwrap(),
        ..transmit()
    });
    let wrong_source = Transmit {
        destination: MEMBERS[0].into(),
        src_ip: Some("192.168.50.99".parse().unwrap()),
        ..transmit()
    };
    for tx in bad.iter().chain([&wrong_source]) {
        let socket = fresh();
        assert_eq!(
            socket.try_send(tx).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(socket.signal.is_revoked());
        assert_eq!(
            socket
                .try_send(&Transmit {
                    destination: MEMBERS[0].into(),
                    ..transmit()
                })
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn the_default_server_config_refuses_every_client() {
    let (server_socket, client_socket) = memory_sockets();
    let server_id = DeviceIdentity::generate().unwrap();
    let client_id = DeviceIdentity::generate().unwrap();
    let server = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(SecureQuicConfig::server_refusing_all().unwrap()),
        server_socket.clone(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let client = endpoint(client_socket.clone(), &client_id, &server_id);

    let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async {
                client
                    .connect(LOCAL.into(), LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
            },
            async {
                // With no certificate to present, the default config fails on the first flight.
                match server.accept().await.unwrap().accept() {
                    Ok(connecting) => connecting.await.map(drop),
                    Err(refused) => Err(refused),
                }
            }
        )
    })
    .await
    .expect("the refusal is prompt");
    assert!(dialed.is_err());
    assert!(accepted.is_err());

    // The same two identities connect once the member's pinned configuration is chosen.
    let pinned = Arc::new(SecureQuicConfig::server(&server_id, &paired(&client_id)).unwrap());
    let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async {
                client
                    .connect(LOCAL.into(), LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
                    .unwrap()
            },
            async {
                server
                    .accept()
                    .await
                    .unwrap()
                    .accept_with(pinned)
                    .unwrap()
                    .await
                    .unwrap()
            }
        )
    })
    .await
    .expect("the pinned handshake finishes");
    delivers(&dialed, &accepted).await;
    server_socket.signal.revoke();
    client_socket.signal.revoke();
}

#[tokio::test]
async fn three_members_share_one_endpoint_each_bound_to_its_own_pin() {
    let [hub_socket, first_socket, second_socket, third_socket] = memory_network([
        (LOCAL, &MEMBERS[..]),
        (MEMBERS[0], &[LOCAL][..]),
        (MEMBERS[1], &[LOCAL][..]),
        (MEMBERS[2], &[LOCAL][..]),
    ]);
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 3].map(|()| DeviceIdentity::generate().unwrap());
    let prints: [CertificateFingerprint; 3] = [0, 1, 2].map(|index| ids[index].fingerprint());
    let hub = guarded(
        hub_socket,
        &hub_id,
        &[
            (MEMBERS[0], &ids[0]),
            (MEMBERS[1], &ids[1]),
            (MEMBERS[2], &ids[2]),
        ],
    );
    let first = guarded(first_socket, &ids[0], &[(LOCAL, &hub_id)]);
    let second = guarded(second_socket, &ids[1], &[(LOCAL, &hub_id)]);
    let third = guarded(third_socket, &ids[2], &[(LOCAL, &hub_id)]);
    assert_eq!(
        hub.connect().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        hub.accept().await.unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );

    let (from_first, from_third, accepted, (to_second, at_second)) =
        tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async { first.connect().unwrap().await.unwrap() },
                async { third.connect().unwrap().await.unwrap() },
                async {
                    let one = hub.accept_any().await.unwrap();
                    let other = hub.accept_any().await.unwrap();
                    [one, other]
                },
                async {
                    tokio::join!(
                        async { hub.connect_member(prints[1]).unwrap().await.unwrap() },
                        async { second.accept().await.unwrap() }
                    )
                }
            )
        })
        .await
        .expect("every member connected within a second");
    hub.confirm_member(prints[1], &to_second).unwrap();
    assert_eq!(to_second.remote_address(), MEMBERS[1].into());

    let accepted_from = |print: CertificateFingerprint| {
        accepted
            .iter()
            .find(|(member, _)| *member == print)
            .map(|(_, connection)| connection)
            .expect("each dialing member was accepted as itself")
    };
    let at_hub_first = accepted_from(prints[0]);
    let at_hub_third = accepted_from(prints[2]);
    assert_eq!(at_hub_first.remote_address(), MEMBERS[0].into());
    assert_eq!(at_hub_third.remote_address(), MEMBERS[2].into());
    for (ours, theirs) in [
        (at_hub_first, &from_first),
        (at_hub_third, &from_third),
        (&to_second, &at_second),
    ] {
        delivers(ours, theirs).await;
        delivers(theirs, ours).await;
    }

    // A connection names exactly one member; confirming it as another closes it.
    assert_eq!(
        hub.confirm_member(prints[0], &to_second)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    tokio::time::timeout(DEADLINE, at_second.closed())
        .await
        .expect("the misbound connection closed");
    delivers(at_hub_first, &from_first).await;
    assert!(
        hub.confirm_member(hub_id.fingerprint(), at_hub_third)
            .is_err()
    );
    for endpoint in [&hub, &first, &second, &third] {
        endpoint.revoke();
    }
}

#[tokio::test]
async fn a_member_presenting_another_members_certificate_is_refused() {
    let [hub_socket, impostor_socket, second_socket] = memory_network([
        (LOCAL, &MEMBERS[..2]),
        (MEMBERS[0], &[LOCAL][..]),
        (MEMBERS[1], &[LOCAL][..]),
    ]);
    let hub_id = DeviceIdentity::generate().unwrap();
    let first_id = DeviceIdentity::generate().unwrap();
    let second_id = DeviceIdentity::generate().unwrap();
    let hub = guarded(
        hub_socket,
        &hub_id,
        &[(MEMBERS[0], &first_id), (MEMBERS[1], &second_id)],
    );
    // The second member's key, used from the first member's recorded address.
    let impostor = guarded(impostor_socket, &second_id, &[(LOCAL, &hub_id)]);
    let second = guarded(second_socket, &second_id, &[(LOCAL, &hub_id)]);

    let (dialed, refused) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async { impostor.connect().unwrap().await },
            hub.accept_any()
        )
    })
    .await
    .expect("the refusal is prompt");
    let Err(refused) = refused else {
        panic!("the impostor was accepted");
    };
    let failure = handshake_failure(&refused);
    assert!(failure.member == first_id.fingerprint());
    assert!(failure.cause.is_some());
    if let Ok(dialed) = dialed {
        tokio::time::timeout(DEADLINE, dialed.closed())
            .await
            .expect("the impostor's connection closed");
    }

    let (dialed, answered) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async { hub.connect_member(first_id.fingerprint()).unwrap().await },
            impostor.accept()
        )
    })
    .await
    .expect("the refusal is prompt");
    assert!(dialed.is_err());
    assert!(answered.is_err());

    let (dialed, (member, accepted)) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(async { second.connect().unwrap().await.unwrap() }, async {
            hub.accept_any().await.unwrap()
        })
    })
    .await
    .expect("the real member still connects");
    assert!(member == second_id.fingerprint());
    assert_eq!(accepted.remote_address(), MEMBERS[1].into());
    delivers(&dialed, &accepted).await;
    assert_eq!(
        hub.confirm_member(first_id.fingerprint(), &accepted)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    for endpoint in [&hub, &impostor, &second] {
        endpoint.revoke();
    }
}

fn handles_cross_threads<T: Clone + Send + Sync + 'static>() {}

#[test]
fn connection_drivers_run_on_the_network_runtime_when_another_thread_dials() {
    handles_cross_threads::<EndpointHandle>();
    let (handle_sender, handle_receiver) = std::sync::mpsc::channel();
    let (dialed_sender, dialed_receiver) = tokio::sync::oneshot::channel::<quinn::Connection>();
    let network = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let [dialer_socket, listener_socket] =
                memory_network([(LOCAL, &[PEER][..]), (PEER, &[LOCAL][..])]);
            let dialer_id = DeviceIdentity::generate().unwrap();
            let listener_id = DeviceIdentity::generate().unwrap();
            let dialer = guarded(dialer_socket, &dialer_id, &[(PEER, &listener_id)]);
            let listener = guarded(listener_socket, &listener_id, &[(LOCAL, &dialer_id)]);
            handle_sender
                .send((dialer.handle(), listener_id.fingerprint()))
                .unwrap();
            let (accepted, dialed) = tokio::time::timeout(DEADLINE, async {
                tokio::join!(listener.accept(), dialed_receiver)
            })
            .await
            .expect("the dial from another thread finished");
            let (accepted, dialed) = (accepted.unwrap(), dialed.unwrap());
            // The dialing thread's runtime is gone; only the network runtime drives this now.
            delivers(&dialed, &accepted).await;
            delivers(&accepted, &dialed).await;
            dialer.revoke();
            listener.revoke();
        });
    });

    let (handle, listener) = handle_receiver.recv_timeout(DEADLINE).unwrap();
    assert!(tokio::runtime::Handle::try_current().is_err());
    let connecting = handle.connect_member(listener).unwrap();
    let dialing = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let connection = dialing
        .block_on(async { tokio::time::timeout(DEADLINE, connecting).await })
        .expect("the dial finished within a second")
        .unwrap();
    handle.confirm_member(listener, &connection).unwrap();
    drop(dialing);
    dialed_sender.send(connection).unwrap();
    network.join().unwrap();
    assert!(handle.is_revoked());
}

/// Each platform's host-down, host-unreachable, no-buffer and network-unreachable send codes.
fn unreachable_codes() -> Vec<i32> {
    if cfg!(windows) {
        vec![10_064, 10_065, 10_055, 10_051]
    } else if cfg!(target_os = "macos") {
        vec![64, 65, 55, 51]
    } else {
        Vec::new()
    }
}

/// How a send to a host that stopped answering fails.
#[cfg(windows)]
const HOST_DOWN: SendFailure = SendFailure::Os(10_064);
#[cfg(target_os = "macos")]
const HOST_DOWN: SendFailure = SendFailure::Os(64);
#[cfg(not(any(windows, target_os = "macos")))]
const HOST_DOWN: SendFailure = SendFailure::Kind(io::ErrorKind::HostUnreachable);

fn to(member: SocketAddrV4) -> Transmit<'static> {
    Transmit {
        destination: member.into(),
        ..transmit()
    }
}

/// A hub and two members on one memory link.
fn two_member_network() -> [GuardedSocket<TestIo>; 3] {
    memory_network([
        (LOCAL, &MEMBERS[..2]),
        (MEMBERS[0], &[LOCAL][..]),
        (MEMBERS[1], &[LOCAL][..]),
    ])
}

/// A hub endpoint admitting two members, and each member's endpoint admitting the hub.
fn two_member_endpoints(
    hub_id: &DeviceIdentity,
    ids: &[DeviceIdentity; 2],
) -> (GuardedEndpoint, Arc<TestIo>, [GuardedEndpoint; 2]) {
    let [hub_socket, first_socket, second_socket] = two_member_network();
    let hub_io = hub_socket.io.clone();
    let hub = guarded(
        hub_socket,
        hub_id,
        &[(MEMBERS[0], &ids[0]), (MEMBERS[1], &ids[1])],
    );
    let first = guarded(first_socket, &ids[0], &[(LOCAL, hub_id)]);
    let second = guarded(second_socket, &ids[1], &[(LOCAL, hub_id)]);
    (hub, hub_io, [first, second])
}

#[tokio::test]
async fn an_unreachable_member_drops_datagrams_without_revoking_the_endpoint() {
    let codes = unreachable_codes();
    if let [_, host_unreachable, _, network_unreachable] = codes[..] {
        assert_eq!(
            io::Error::from_raw_os_error(host_unreachable).kind(),
            io::ErrorKind::HostUnreachable
        );
        assert_eq!(
            io::Error::from_raw_os_error(network_unreachable).kind(),
            io::ErrorKind::NetworkUnreachable
        );
    }
    let failures = [
        SendFailure::Kind(io::ErrorKind::HostUnreachable),
        SendFailure::Kind(io::ErrorKind::NetworkUnreachable),
    ]
    .into_iter()
    .chain(codes.into_iter().map(SendFailure::Os));
    for failure in failures {
        let [hub, down, up] = two_member_network();
        hub.io.fail_sends_to(MEMBERS[0], failure);
        for _ in 0..3 {
            hub.try_send(&to(MEMBERS[0])).unwrap();
            hub.try_send(&to(MEMBERS[1])).unwrap();
        }
        assert!(!hub.signal.is_revoked());
        assert_eq!(hub.dropped[0].sends.load(Ordering::SeqCst), 3);
        assert_eq!(hub.dropped[1].sends.load(Ordering::SeqCst), 0);
        assert_eq!(down.io.inbox.lock().unwrap().arrivals, 0);
        assert_eq!(up.io.inbox.lock().unwrap().arrivals, 3);
        hub.io.heal_sends();
        hub.try_send(&to(MEMBERS[0])).unwrap();
        assert_eq!(down.io.inbox.lock().unwrap().arrivals, 1);
    }

    // With live sessions, the member that went down costs the other member nothing.
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
    let (hub, hub_io, [down, up]) = two_member_endpoints(&hub_id, &ids);
    let (from_down, from_up, accepted) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async { down.connect().unwrap().await.unwrap() },
            async { up.connect().unwrap().await.unwrap() },
            async {
                [
                    hub.accept_any().await.unwrap(),
                    hub.accept_any().await.unwrap(),
                ]
            }
        )
    })
    .await
    .expect("both members connected");
    let at_hub = |print: CertificateFingerprint| {
        accepted
            .iter()
            .find(|(member, _)| *member == print)
            .map(|(_, connection)| connection)
            .expect("each member was accepted as itself")
    };
    let (at_down, at_up) = (at_hub(ids[0].fingerprint()), at_hub(ids[1].fingerprint()));

    hub_io.fail_sends_to(MEMBERS[0], HOST_DOWN);
    at_down
        .send_datagram(b"lost while the member is down".to_vec().into())
        .unwrap();
    tokio::time::timeout(DEADLINE, async {
        while hub_io.failed_sends.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the hub sent toward the member that is down");
    delivers(at_up, &from_up).await;
    delivers(&from_up, at_up).await;
    assert!(!hub.is_revoked());
    assert!(at_up.close_reason().is_none());

    // Back before its idle timeout, that member's own session carries on.
    hub_io.heal_sends();
    delivers(at_down, &from_down).await;
    delivers(&from_down, at_down).await;
    assert!(!hub.is_revoked());
    for endpoint in [&hub, &down, &up] {
        endpoint.revoke();
    }
}

#[test]
fn a_policy_violation_still_revokes() {
    let lock = memory_lock(LOCAL, &MEMBERS);
    let fresh = || {
        GuardedSocket::new(
            TestIo::new(LOCAL, Arc::default(), Arc::default()),
            &lock,
            LOCAL,
            &MEMBERS,
            Reachability::new(MEMBERS.len()),
            RevocationSignal::default(),
        )
        .unwrap()
    };
    // Neither a member that stopped answering nor one set aside by a route check excuses a
    // transmit that leaves the pinned members, source address, or size bounds.
    let excusing = || {
        let socket = fresh();
        for member in MEMBERS {
            socket.io.fail_sends_to(member, HOST_DOWN);
        }
        socket.reachability.set(&[false, true, true]);
        socket
    };
    let wrong_source = Some("192.168.50.99".parse().unwrap());
    let large = vec![0; MAX_DATAGRAM_BYTES + 1];
    let violations = [
        to("192.168.50.14:24800".parse().unwrap()),
        to(SocketAddrV4::new(*MEMBERS[0].ip(), 24801)),
        to(LOCAL),
        Transmit {
            destination: "[::ffff:192.168.50.11]:24800".parse().unwrap(),
            ..transmit()
        },
        Transmit {
            src_ip: wrong_source,
            ..to(MEMBERS[0])
        },
        Transmit {
            src_ip: wrong_source,
            ..to(MEMBERS[1])
        },
        Transmit {
            segment_size: Some(4),
            ..to(MEMBERS[0])
        },
        Transmit {
            contents: &[],
            ..to(MEMBERS[1])
        },
        Transmit {
            contents: &large,
            ..to(MEMBERS[2])
        },
    ];
    for tx in &violations {
        let socket = excusing();
        assert_eq!(
            socket.try_send(tx).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(socket.signal.is_revoked());
        assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
    }

    // Any other send failure toward a member still revokes, and so does a short write.
    #[cfg(windows)]
    let access_denied = 10_013; // WSAEACCES
    #[cfg(not(windows))]
    let access_denied = 13; // EACCES
    for failure in [
        SendFailure::Kind(io::ErrorKind::NetworkDown),
        SendFailure::Kind(io::ErrorKind::PermissionDenied),
        SendFailure::Kind(io::ErrorKind::ConnectionRefused),
        SendFailure::Kind(io::ErrorKind::Other),
        SendFailure::Os(access_denied),
    ] {
        let socket = fresh();
        socket.io.fail_sends_to(MEMBERS[1], failure);
        assert_eq!(
            socket.try_send(&to(MEMBERS[1])).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(socket.signal.is_revoked());
    }
    let socket = fresh();
    socket.io.partial_send.store(true, Ordering::SeqCst);
    assert_eq!(
        socket.try_send(&to(MEMBERS[2])).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(socket.signal.is_revoked());
}

#[tokio::test]
async fn a_member_set_aside_by_a_route_check_is_absent_until_one_finds_it() {
    let [hub, first, second] = two_member_network();
    hub.reachability.set(&[false, true]);
    hub.try_send(&to(MEMBERS[0])).unwrap();
    hub.try_send(&to(MEMBERS[1])).unwrap();
    assert_eq!(hub.io.sends.load(Ordering::SeqCst), 1);
    assert_eq!(hub.dropped[0].sends.load(Ordering::SeqCst), 1);
    assert_eq!(first.io.inbox.lock().unwrap().arrivals, 0);
    assert_eq!(second.io.inbox.lock().unwrap().arrivals, 1);
    let from = |source| Arrival {
        source,
        ..arrival()
    };
    hub.io.enqueue(from(MEMBERS[0]));
    hub.io.enqueue(from(MEMBERS[1]));
    let mut cx = Context::from_waker(Waker::noop());
    let (result, meta) = receive(&hub, &mut cx);
    assert!(matches!(result, Poll::Ready(Ok(1))));
    assert_eq!(meta.addr, MEMBERS[1].into());
    assert!(receive(&hub, &mut cx).0.is_pending());
    assert!(!hub.signal.is_revoked());

    // The next check that finds its route admits it again.
    hub.reachability.set(&[true, true]);
    hub.try_send(&to(MEMBERS[0])).unwrap();
    assert_eq!(first.io.inbox.lock().unwrap().arrivals, 1);
    hub.io.enqueue(from(MEMBERS[0]));
    let (result, meta) = receive(&hub, &mut cx);
    assert!(matches!(result, Poll::Ready(Ok(1))));
    assert_eq!(meta.addr, MEMBERS[0].into());

    // A dial to it fails at once with its own error, and only for that member.
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
    let (hub, _, [first, second]) = two_member_endpoints(&hub_id, &ids);
    hub.set_reachable(&[false, true]);
    assert_eq!(
        hub.connect_member(ids[0].fingerprint()).unwrap_err().kind(),
        io::ErrorKind::HostUnreachable
    );
    assert!(hub.handle().admits(ids[0].fingerprint()));
    for (index, member) in [(1, &second), (0, &first)] {
        if index == 0 {
            hub.set_reachable(&[true, true]);
        }
        let print = ids[index].fingerprint();
        let (dialed, answered) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async { hub.connect_member(print).unwrap().await.unwrap() },
                async { member.accept().await.unwrap() }
            )
        })
        .await
        .expect("a reachable member answers");
        hub.confirm_member(print, &dialed).unwrap();
        delivers(&dialed, &answered).await;
    }
    assert!(!hub.is_revoked());
    for endpoint in [&hub, &first, &second] {
        endpoint.revoke();
    }
}

#[tokio::test]
async fn dropping_the_endpoint_stops_the_timer() {
    const INTERVAL: Duration = Duration::from_millis(20);
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
    let [hub_socket, _, _] = two_member_network();
    let checks = Arc::new(AtomicUsize::new(0));
    // Finds the member still set aside on every run, so only a stop ends the timer.
    let check: SharedCheck = {
        let checks = checks.clone();
        Arc::new(move || {
            checks.fetch_add(1, Ordering::SeqCst);
            true
        })
    };
    let held = Arc::downgrade(&check);
    let hub = GuardedEndpoint::over_socket_rechecking(
        hub_socket,
        &group(LOCAL, &[(MEMBERS[0], &ids[0]), (MEMBERS[1], &ids[1])]),
        &hub_id,
        Some((check, INTERVAL)),
    )
    .unwrap();
    let signal = hub.revocation_signal();
    let closed = hub.socket_closed();
    hub.set_reachable(&[false, true]);
    tokio::time::timeout(DEADLINE, async {
        while checks.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the timer rechecks while a member is set aside");

    drop(hub);
    assert!(signal.is_revoked());
    tokio::time::timeout(DEADLINE, async {
        while held.strong_count() > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the timer let go of its check");
    let stopped = checks.load(Ordering::SeqCst);
    tokio::time::sleep(INTERVAL * 5).await;
    assert_eq!(checks.load(Ordering::SeqCst), stopped);
    tokio::time::timeout(DEADLINE, closed)
        .await
        .expect("the timer kept no part of the endpoint open");
}

#[tokio::test]
async fn first_packet_flood_from_one_member_does_not_block_another_members_handshake() {
    // Long-header Initial, Handshake and a short-header packet, by their first byte.
    const INITIAL: u8 = 0xC3;
    const HANDSHAKE: u8 = 0xE3;
    const SHORT: u8 = 0x43;
    let [hub, _, _] = two_member_network();
    let send = |source, first| {
        hub.io.enqueue_bytes(
            Arrival {
                source,
                length: 64,
                ..arrival()
            },
            vec![first; 64],
        );
    };
    let drain = || {
        let mut delivered = Vec::new();
        loop {
            let mut bytes = [0; 128];
            let mut meta = [RecvMeta::default()];
            match hub.poll_recv(
                &mut Context::from_waker(Waker::noop()),
                &mut [IoSliceMut::new(&mut bytes)],
                &mut meta,
            ) {
                Poll::Ready(Ok(1)) => delivered.push((meta[0].addr, bytes[0])),
                Poll::Pending => return delivered,
                other => panic!("unexpected receive: {other:?}"),
            }
        }
    };
    for _ in 0..3 * FIRST_PACKET_BURST {
        send(MEMBERS[0], INITIAL);
    }
    send(MEMBERS[1], INITIAL);
    send(MEMBERS[0], HANDSHAKE);
    send(MEMBERS[0], SHORT);
    let delivered = drain();
    let flooded = SocketAddr::from(MEMBERS[0]);
    assert_eq!(
        delivered
            .iter()
            .filter(|packet| **packet == (flooded, INITIAL))
            .count(),
        FIRST_PACKET_BURST as usize
    );
    for packet in [
        (MEMBERS[1].into(), INITIAL),
        (flooded, HANDSHAKE),
        (flooded, SHORT),
    ] {
        assert!(delivered.contains(&packet), "{packet:?} was held back");
    }
    assert_eq!(
        hub.dropped[0].first_packets.load(Ordering::SeqCst),
        u64::from(2 * FIRST_PACKET_BURST)
    );
    assert_eq!(hub.dropped[1].first_packets.load(Ordering::SeqCst), 0);
    assert!(!hub.signal.is_revoked());

    // The budget earns one first packet back per interval.
    hub.first_packets.lock().unwrap()[0].refilled = Instant::now() - FIRST_PACKET_REFILL;
    send(MEMBERS[0], INITIAL);
    send(MEMBERS[0], INITIAL);
    assert_eq!(drain(), [(flooded, INITIAL)]);

    // A member dialing far more handshakes than Quinn queues never holds up another's.
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
    let (hub, _, [flooding, other]) = two_member_endpoints(&hub_id, &ids);
    let flood: Vec<_> = (0..2 * crate::crypto::MAX_PENDING_INCOMING)
        .map(|_| flooding.connect().unwrap())
        .collect();
    let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(async { other.connect().unwrap().await }, async {
            loop {
                match hub.accept_any().await {
                    Ok((member, connection)) if member == ids[1].fingerprint() => {
                        break connection;
                    }
                    Ok(_) => {}
                    Err(_) => assert!(!hub.is_revoked(), "the flood revoked the endpoint"),
                }
            }
        })
    })
    .await
    .expect("the other member connected during the flood");
    delivers(&dialed.unwrap(), &accepted).await;
    drop(flood);
    for endpoint in [&hub, &flooding, &other] {
        endpoint.revoke();
    }
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
    let make_io = || -> NativeSocket {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        platform::network::restrict_udp_interface(&socket, index).unwrap();
        platform::udp_receive::UdpReceiver::configure(socket)
            .unwrap()
            .into_async()
            .unwrap()
            .into()
    };
    let io_a = make_io();
    let io_b = make_io();
    let local_a = io_a.local_addr().unwrap();
    let local_b = io_b.local_addr().unwrap();
    // Only this private test fixture admits loopback. Production construction requires NetworkLock.
    let first = Arc::new(pinned_socket(io_a, local_a, &[local_b], index));
    let second = Arc::new(pinned_socket(io_b, local_b, &[local_a], index));
    let identity_a = DeviceIdentity::generate().unwrap();
    let identity_b = DeviceIdentity::generate().unwrap();
    let endpoint_a = endpoint(first.clone(), &identity_a, &identity_b);
    let endpoint_b = endpoint(second.clone(), &identity_b, &identity_a);
    let idle_io = make_io();
    let idle_address = idle_io.local_addr().unwrap();
    let idle = Arc::new(pinned_socket(
        ObservedIo {
            inner: Arc::new(idle_io),
            pending: Notify::new(),
        },
        idle_address,
        &[local_b],
        index,
    ));
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
