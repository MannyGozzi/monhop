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
    /// Every send attempt, whatever came of it: when, where to, and what.
    attempts: Mutex<Vec<(Instant, SocketAddrV4, Vec<u8>)>>,
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
            attempts: Mutex::default(),
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
    /// When each send to `destination` was attempted, and what it carried.
    fn attempts_to(&self, destination: SocketAddrV4) -> Vec<(Instant, Vec<u8>)> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, to, _)| *to == destination)
            .map(|(at, _, bytes)| (*at, bytes.clone()))
            .collect()
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
        self.attempts
            .lock()
            .unwrap()
            .push((Instant::now(), peer, buffer.to_vec()));
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
fn an_oversized_datagram_from_anyone_is_dropped_without_revoking() {
    let socket = fixture();
    for _ in 0..3 {
        socket
            .io
            .inbox
            .lock()
            .unwrap()
            .queue
            .push_back(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                OversizedDatagram,
            )));
    }
    socket.io.enqueue(arrival());
    let (result, meta) = receive(&socket, &mut Context::from_waker(Waker::noop()));
    assert!(matches!(result, Poll::Ready(Ok(1))));
    assert_eq!(meta.addr, PEER.into());
    assert!(!socket.signal.is_revoked());
    assert_eq!(socket.oversized.load(Ordering::Relaxed), 3);
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
    native::{Cadence, SharedCheck},
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

/// The selected adapter on the memory link, a /24.
fn memory_interface(local: SocketAddrV4) -> InterfaceSnapshot {
    InterfaceSnapshot {
        stable_id: "memory-link".into(),
        name: "memory".into(),
        index: INDEX,
        address: *local.ip(),
        prefix_len: 24,
        kind: InterfaceKind::Ethernet,
        is_hardware: true,
        is_up: true,
        network_signature: vec![1; 32],
    }
}

/// The lock a bind on the memory link would hold: every member on-link with a direct route.
fn memory_lock(local: SocketAddrV4, members: &[SocketAddrV4]) -> NetworkLock {
    let route = RouteSnapshot {
        interface_index: INDEX,
        source: *local.ip(),
        next_hop: Ipv4Addr::UNSPECIFIED,
    };
    let routes: Vec<_> = members.iter().map(|member| (*member.ip(), route)).collect();
    NetworkLock::new_group(memory_interface(local), &routes, false).unwrap()
}

/// One in-memory link shared by every node: a send reaches the inbox of the address it names, or
/// no one.
fn memory_link<const N: usize>(addresses: [SocketAddrV4; N]) -> [TestIo; N] {
    let inboxes: Vec<(SocketAddrV4, Arc<Mutex<Inbox>>)> = addresses
        .iter()
        .map(|address| (*address, Arc::default()))
        .collect();
    addresses.map(|local| {
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
        io
    })
}

/// A memory link whose sockets are each built as a bind builds it, admitting only its members.
fn memory_network<const N: usize>(
    nodes: [(SocketAddrV4, &[SocketAddrV4]); N],
) -> [GuardedSocket<TestIo>; N] {
    let mut ios = memory_link(nodes.map(|(local, _)| local)).into_iter();
    nodes.map(|(local, members)| {
        GuardedSocket::new(
            ios.next().unwrap(),
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
    let hub_io = hub_socket.io.clone();
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
    let cadence = Cadence {
        recheck: INTERVAL,
        probe: INTERVAL * 2,
    };
    let hub = GuardedEndpoint::over_socket_rechecking(
        hub_socket,
        &group(LOCAL, &[(MEMBERS[0], &ids[0]), (MEMBERS[1], &ids[1])]),
        &hub_id,
        Some((check, cadence)),
    )
    .unwrap();
    let signal = hub.revocation_signal();
    let closed = hub.socket_closed();
    hub.set_reachable(&[false, true]);
    tokio::time::timeout(DEADLINE, async {
        while checks.load(Ordering::SeqCst) < 2 || hub_io.attempts_to(MEMBERS[0]).is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the timer rechecks and probes while a member is set aside");

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
    let probed = hub_io.attempts_to(MEMBERS[0]).len();
    tokio::time::sleep(INTERVAL * 5).await;
    assert_eq!(checks.load(Ordering::SeqCst), stopped);
    assert_eq!(hub_io.attempts_to(MEMBERS[0]).len(), probed);
    tokio::time::timeout(DEADLINE, closed)
        .await
        .expect("the timer kept no part of the endpoint open");
}

/// Stands in for the production cadence, so probing runs in real time.
const PROBING: Cadence = Cadence {
    recheck: Duration::from_millis(20),
    probe: Duration::from_millis(150),
};

/// A hub admitting two members on the memory link, its timer probing on `PROBING`, with each
/// side's io. The timer's check finds every route once `routes_back` is set and changes nothing
/// before.
struct Probing {
    hub: GuardedEndpoint,
    hub_io: Arc<TestIo>,
    reachability: Arc<Reachability>,
    routes_back: Arc<AtomicBool>,
    members: [(GuardedEndpoint, Arc<TestIo>); 2],
    ids: [DeviceIdentity; 2],
}

fn probing() -> Probing {
    let hub_id = DeviceIdentity::generate().unwrap();
    let ids = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
    let [hub_socket, first_socket, second_socket] = two_member_network();
    let hub_io = hub_socket.io.clone();
    let reachability = hub_socket.reachability();
    let routes_back = Arc::new(AtomicBool::new(false));
    let check: SharedCheck = {
        let (reachability, routes_back) = (reachability.clone(), routes_back.clone());
        Arc::new(move || {
            if routes_back.load(Ordering::SeqCst) {
                reachability.set(&[true, true]);
            }
            true
        })
    };
    let hub = GuardedEndpoint::over_socket_rechecking(
        hub_socket,
        &group(LOCAL, &[(MEMBERS[0], &ids[0]), (MEMBERS[1], &ids[1])]),
        &hub_id,
        Some((check, PROBING)),
    )
    .unwrap();
    let members = [(first_socket, &ids[0]), (second_socket, &ids[1])].map(|(socket, id)| {
        let io = socket.io.clone();
        (guarded(socket, id, &[(LOCAL, &hub_id)]), io)
    });
    Probing {
        hub,
        hub_io,
        reachability,
        routes_back,
        members,
        ids,
    }
}

/// When each send to `member` went out, once there are at least `count`, all of them probes.
async fn probes(io: &TestIo, member: SocketAddrV4, count: usize) -> Vec<Instant> {
    let sent = tokio::time::timeout(DEADLINE * 5, async {
        loop {
            let sent = io.attempts_to(member);
            if sent.len() >= count {
                break sent;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the timer probed the member");
    assert!(sent.iter().all(|(_, bytes)| bytes[..] == PROBE));
    sent.into_iter().map(|(at, _)| at).collect()
}

#[tokio::test]
async fn a_set_aside_member_gets_a_probe_after_the_interval_and_none_before() {
    let Probing {
        hub,
        hub_io,
        members: [(first, first_io), (second, _)],
        ids,
        ..
    } = probing();
    let marked = Instant::now();
    hub.set_reachable(&[false, true]);
    tokio::time::sleep(PROBING.probe * 2 / 3).await;
    assert!(hub_io.attempts_to(MEMBERS[0]).is_empty(), "probed early");
    let probed = probes(&hub_io, MEMBERS[0], 2).await;
    assert!(probed[0] >= marked + PROBING.probe);
    assert!(probed[1] >= probed[0] + PROBING.probe);
    assert!(hub_io.attempts_to(MEMBERS[1]).is_empty());
    assert!(!hub.is_revoked());

    // The member's Quinn drops each probe without a reply, and its endpoint carries on.
    tokio::time::timeout(DEADLINE, async {
        loop {
            let read = {
                let inbox = first_io.inbox.lock().unwrap();
                inbox.arrivals >= 2 && inbox.queue.is_empty()
            };
            if read {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the member read both probes");
    tokio::time::sleep(PROBING.recheck).await;
    assert_eq!(first_io.sends.load(Ordering::SeqCst), 0);
    assert!(!first.is_revoked());

    hub.set_reachable(&[true, true]);
    let print = ids[0].fingerprint();
    let (dialed, answered) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            async { hub.connect_member(print).unwrap().await.unwrap() },
            async { first.accept().await.unwrap() }
        )
    })
    .await
    .expect("the readmitted member answers");
    hub.confirm_member(print, &dialed).unwrap();
    delivers(&dialed, &answered).await;
    for endpoint in [&hub, &first, &second] {
        endpoint.revoke();
    }
}

#[test]
fn the_probe_passes_every_policy_check_and_is_one_byte() {
    let [hub, first, second] = two_member_network();
    // A reachable member needs no probe.
    hub.probe(MEMBERS[0]);
    assert_eq!(hub.io.sends.load(Ordering::SeqCst), 0);

    hub.reachability.set(&[false, true]);
    hub.probe(MEMBERS[0]);
    hub.probe(MEMBERS[1]);
    let sent = hub.io.attempts_to(MEMBERS[0]);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, [0]);
    assert!(hub.io.attempts_to(MEMBERS[1]).is_empty());
    assert_eq!(second.io.inbox.lock().unwrap().arrivals, 0);
    {
        let inbox = first.io.inbox.lock().unwrap();
        let Some(Ok(Packet { arrival, bytes })) = inbox.queue.front() else {
            panic!("the member set aside got no probe");
        };
        assert_eq!(bytes[..], PROBE);
        assert_eq!(
            (arrival.source, arrival.destination, arrival.interface_index),
            (LOCAL, *MEMBERS[0].ip(), INDEX)
        );
    }
    assert!(!hub.signal.is_revoked());

    // Each reaches the member's Quinn without spending its first-packet budget.
    for _ in 1..2 * FIRST_PACKET_BURST {
        hub.probe(MEMBERS[0]);
    }
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..2 * FIRST_PACKET_BURST {
        let (result, meta) = receive(&first, &mut cx);
        assert!(matches!(result, Poll::Ready(Ok(1))));
        assert_eq!((meta.addr, meta.len), (LOCAL.into(), PROBE.len()));
    }
    assert!(receive(&first, &mut cx).0.is_pending());
    assert_eq!(first.dropped[0].first_packets.load(Ordering::SeqCst), 0);

    // Revoked, the socket probes no one.
    hub.signal.revoke();
    hub.probe(MEMBERS[0]);
    assert_eq!(
        hub.io.sends.load(Ordering::SeqCst),
        2 * FIRST_PACKET_BURST as usize
    );
}

#[test]
fn a_probe_send_error_never_revokes() {
    #[cfg(windows)]
    let access_denied = 10_013; // WSAEACCES
    #[cfg(not(windows))]
    let access_denied = 13; // EACCES
    let failures = [
        io::ErrorKind::HostUnreachable,
        io::ErrorKind::NetworkUnreachable,
        io::ErrorKind::NetworkDown,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::ConnectionRefused,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::Other,
    ]
    .map(SendFailure::Kind)
    .into_iter()
    .chain(
        unreachable_codes()
            .into_iter()
            .chain([access_denied])
            .map(SendFailure::Os),
    );
    for failure in failures {
        let [hub, first, second] = two_member_network();
        hub.reachability.set(&[false, true]);
        hub.io.fail_sends_to(MEMBERS[0], failure);
        hub.probe(MEMBERS[0]);
        hub.probe(MEMBERS[0]);
        assert!(!hub.signal.is_revoked());
        assert_eq!(hub.io.failed_sends.load(Ordering::SeqCst), 2);
        assert_eq!(hub.dropped[0].probes.load(Ordering::SeqCst), 2);
        // The other member's session carries on, and a probe once sends heal gets through.
        hub.try_send(&to(MEMBERS[1])).unwrap();
        assert_eq!(second.io.inbox.lock().unwrap().arrivals, 1);
        hub.io.heal_sends();
        hub.probe(MEMBERS[0]);
        assert_eq!(first.io.inbox.lock().unwrap().arrivals, 1);
        assert!(!hub.signal.is_revoked());
    }

    let stuck: [fn(&TestIo) -> &AtomicBool; 2] = [|io| &io.hard_send_error, |io| &io.blocked];
    for stuck in stuck {
        let [hub, _, _] = two_member_network();
        hub.reachability.set(&[false, true]);
        stuck(&hub.io).store(true, Ordering::SeqCst);
        hub.probe(MEMBERS[0]);
        assert!(!hub.signal.is_revoked());
        assert_eq!(hub.dropped[0].probes.load(Ordering::SeqCst), 1);
    }

    // A short write is no send error: the socket misreported a datagram, and that still revokes.
    let [hub, _, _] = two_member_network();
    hub.reachability.set(&[false, true]);
    hub.io.partial_send.store(true, Ordering::SeqCst);
    hub.probe(MEMBERS[0]);
    assert!(hub.signal.is_revoked());
}

#[tokio::test]
async fn probing_stops_once_the_member_is_reachable() {
    let Probing {
        hub,
        hub_io,
        reachability,
        routes_back,
        members: [(first, _), (second, _)],
        ..
    } = probing();
    hub.set_reachable(&[false, true]);
    probes(&hub_io, MEMBERS[0], 1).await;

    // The timer's own check finds its route again.
    routes_back.store(true, Ordering::SeqCst);
    tokio::time::timeout(DEADLINE, async {
        while !reachability.reaches(0) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the timer readmitted the member");
    let probed = hub_io.attempts_to(MEMBERS[0]).len();
    tokio::time::sleep(PROBING.probe * 3).await;
    assert_eq!(hub_io.attempts_to(MEMBERS[0]).len(), probed);

    // Set aside again, it waits a whole interval for its first probe once more.
    routes_back.store(false, Ordering::SeqCst);
    let marked = Instant::now();
    hub.set_reachable(&[false, true]);
    let again = probes(&hub_io, MEMBERS[0], probed + 1).await;
    assert!(again[probed] >= marked + PROBING.probe);
    assert!(!hub.is_revoked());
    for endpoint in [&hub, &first, &second] {
        endpoint.revoke();
    }
}

#[tokio::test]
async fn no_probe_is_sent_to_a_non_member() {
    let lock = memory_lock(LOCAL, &MEMBERS);
    for stranger in [
        "192.168.50.14:24800",
        "192.168.50.12:24801",
        "192.168.50.12:0",
        "192.168.50.10:24800",
    ] {
        let socket = GuardedSocket::new(
            TestIo::new(LOCAL, Arc::default(), Arc::default()),
            &lock,
            LOCAL,
            &MEMBERS,
            Reachability::new(MEMBERS.len()),
            RevocationSignal::default(),
        )
        .unwrap();
        // Even with every member set aside, a probe must name one of them exactly.
        socket.reachability.set(&[false; 3]);
        socket.probe(stranger.parse().unwrap());
        assert!(socket.signal.is_revoked());
        socket.probe(MEMBERS[0]);
        assert_eq!(socket.io.sends.load(Ordering::SeqCst), 0);
    }

    // A computer forgotten on the endpoint is no longer paired there, and is never probed.
    let Probing {
        hub,
        hub_io,
        members: [(first, _), (second, _)],
        ids,
        ..
    } = probing();
    hub.forget_member(ids[0].fingerprint());
    hub.set_reachable(&[false, false]);
    probes(&hub_io, MEMBERS[1], 2).await;
    assert!(hub_io.attempts_to(MEMBERS[0]).is_empty());
    assert!(!hub.is_revoked());
    for endpoint in [&hub, &first, &second] {
        endpoint.revoke();
    }
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

/// Runs on this Mac's physical network: `MONHOP_TEST_SOURCE` is its IPv4 address there and
/// `MONHOP_TEST_SILENT_PEER` an unused address on the same subnet, which nothing answers. XNU
/// rejects the silent member once resolution gives up and ignores route lookups until a send after
/// its hold-down; the production timer's probe must be that send.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "set MONHOP_TEST_SOURCE and MONHOP_TEST_SILENT_PEER to an unused address on its subnet"]
async fn native_probe_lifts_the_rejection_of_a_member_that_stopped_answering() {
    use super::super::native::{CADENCE, Probe, Readmission};
    use monhop_platform_macos::{network, udp_receive};

    /// XNU's default net.link.ether.inet.host_down_time is 20 s; this allows for a longer one.
    const HOLD_DOWN: Duration = Duration::from_secs(30);
    let address = |variable: &str| -> Ipv4Addr {
        std::env::var(variable)
            .unwrap_or_else(|_| panic!("{variable} must be set for this explicit native test"))
            .parse()
            .unwrap_or_else(|_| panic!("{variable} must contain an IPv4 address"))
    };
    let source = address("MONHOP_TEST_SOURCE");
    let silent = address("MONHOP_TEST_SILENT_PEER");
    let index = network::enumerate_adapters()
        .unwrap()
        .into_iter()
        .find(|adapter| adapter.address == source)
        .expect("the source's interface")
        .index;
    let socket = std::net::UdpSocket::bind((source, 0)).unwrap();
    network::restrict_udp_interface(&socket, index).unwrap();
    let receiver: NativeSocket = udp_receive::UdpReceiver::configure(socket)
        .unwrap()
        .into_async()
        .unwrap()
        .into();
    let local = receiver.local_addr().unwrap();
    let member = SocketAddrV4::new(silent, 9);
    // Only this private fixture skips the lock; its member is an on-link private address all the same.
    let hub = Arc::new(pinned_socket(receiver, local, &[member], index));

    // Each session send is one resolution attempt; XNU rejects the entry once they run out.
    NativeSocket::writable(&hub.io).await.unwrap();
    for _ in 0..8 {
        hub.try_send(&Transmit {
            destination: member.into(),
            ecn: None,
            contents: b"resolve",
            segment_size: None,
            src_ip: None,
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
    }
    let error = network::best_route(source, silent).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::HostUnreachable, "{error}");
    let marked = Instant::now();
    hub.reachability.set(&[false]);

    // The production cadence and a check that reads only this member's route.
    let check: SharedCheck = {
        let reachability = hub.reachability();
        Arc::new(move || match network::best_route(source, silent) {
            Ok(route) if route.interface_index == index && route.next_hop.is_unspecified() => {
                reachability.set(&[true]);
                true
            }
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::HostUnreachable => {
                reachability.set(&[false]);
                true
            }
            Err(_) => false,
        })
    };
    let probed = Arc::new(Mutex::new(Vec::new()));
    let probe: Probe = {
        let (prober, probed) = (hub.prober(), probed.clone());
        Box::new(move |_| {
            probed.lock().unwrap().push(Instant::now());
            prober(member);
        })
    };
    let _timer = Readmission::start(
        &tokio::runtime::Handle::current(),
        check,
        probe,
        hub.revocation(),
        hub.reachability(),
        CADENCE,
    );
    let since_mark = |at: &Instant| *at - marked;
    let readmitted = tokio::time::timeout(HOLD_DOWN + CADENCE.probe + CADENCE.recheck, async {
        while !hub.reachability.reaches(0) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Instant::now()
    })
    .await
    .unwrap_or_else(|_| {
        let probed: Vec<_> = probed.lock().unwrap().iter().map(since_mark).collect();
        let revoked = hub.signal.is_revoked();
        panic!("still set aside (revoked: {revoked}); probed {probed:?} after the mark")
    });
    assert!(!hub.signal.is_revoked());

    // Readmitted by the check just after a probe, never by the passing of time alone.
    let probed = probed.lock().unwrap().clone();
    let last = *probed.last().expect("the member was probed");
    let report = format!(
        "readmitted {:?} and probed {:?} after the mark, {} probes failing in the hold-down",
        since_mark(&readmitted),
        probed.iter().map(since_mark).collect::<Vec<_>>(),
        hub.dropped[0].probes.load(Ordering::SeqCst),
    );
    assert!(probed[0] >= marked + CADENCE.probe, "{report}");
    assert!(readmitted - last <= CADENCE.recheck, "{report}");
    hub.signal.revoke();
}

mod pairing {
    use std::sync::OnceLock;

    use monhop_core::Platform;

    use super::*;
    use crate::{
        guarded_endpoint::PairingEndpoint,
        pairing_code::PairingCode,
        pairing_exchange::{ConfirmedPeer, PairingFailure, PairingRole, confirm_pairing},
    };

    /// Longer than a handshake on the memory link, far shorter than the idle timeout.
    const UNANSWERED: Duration = Duration::from_millis(1500);
    const SHOWING: SocketAddrV4 = LOCAL;
    const ENTERING: SocketAddrV4 = MEMBERS[0];
    const OTHER: SocketAddrV4 = MEMBERS[1];

    fn listening_socket(io: TestIo, chosen: &Arc<OnceLock<SocketAddrV4>>) -> GuardedSocket<TestIo> {
        GuardedSocket::listening(
            io,
            &memory_interface(SHOWING),
            SHOWING,
            chosen.clone(),
            RevocationSignal::default(),
        )
        .unwrap()
    }

    fn listener(io: TestIo, identity: &DeviceIdentity) -> PairingEndpoint {
        let chosen = Arc::new(OnceLock::new());
        PairingEndpoint::listening_over(listening_socket(io, &chosen), chosen, identity).unwrap()
    }

    fn dialing_socket(io: TestIo, local: SocketAddrV4) -> GuardedSocket<TestIo> {
        GuardedSocket::new(
            io,
            &memory_lock(local, &[SHOWING]),
            local,
            &[SHOWING],
            Reachability::new(1),
            RevocationSignal::default(),
        )
        .unwrap()
    }

    fn dialer(io: TestIo, local: SocketAddrV4, identity: &DeviceIdentity) -> PairingEndpoint {
        PairingEndpoint::dialing_over(dialing_socket(io, local), SHOWING, identity).unwrap()
    }

    async fn showing_side(
        endpoint: &PairingEndpoint,
        code: &PairingCode,
        identity: &DeviceIdentity,
    ) -> Result<ConfirmedPeer, PairingFailure> {
        let connection = endpoint.accept().await.unwrap();
        let confirmed = confirm_pairing(
            &connection,
            PairingRole::Showing,
            code,
            identity,
            SHOWING,
            Platform::MacOs,
        )
        .await?;
        let peer = confirmed.peer().clone();
        confirmed.finish_saved().await?;
        Ok(peer)
    }

    async fn entering_side(
        endpoint: &PairingEndpoint,
        typed: &str,
        identity: &DeviceIdentity,
    ) -> Result<ConfirmedPeer, PairingFailure> {
        let code = PairingCode::parse(typed).unwrap();
        assert_eq!(
            code.showing_address(*ENTERING.ip(), 24).unwrap(),
            *SHOWING.ip()
        );
        let connection = endpoint.dial().await.unwrap();
        let confirmed = confirm_pairing(
            &connection,
            PairingRole::Entering,
            &code,
            identity,
            ENTERING,
            Platform::Windows,
        )
        .await?;
        let peer = confirmed.peer().clone();
        confirmed.finish_saved().await?;
        Ok(peer)
    }

    /// Whether `endpoint` fails to connect, or gets no answer at all.
    async fn unanswered(endpoint: &PairingEndpoint) -> bool {
        tokio::time::timeout(UNANSWERED, endpoint.dial())
            .await
            .map_or(true, |dialed| dialed.is_err())
    }

    #[test]
    fn a_pairing_listener_hears_other_subnet_hosts_until_it_chooses_one() {
        let chosen = Arc::new(OnceLock::new());
        let socket = listening_socket(
            TestIo::new(SHOWING, Arc::default(), Arc::default()),
            &chosen,
        );
        let waker = Waker::from(Arc::new(CountWake::default()));
        let mut cx = Context::from_waker(&waker);
        let mut heard = |arrival: Arrival| {
            socket.io.enqueue(arrival);
            matches!(receive(&socket, &mut cx).0, Poll::Ready(Ok(1)))
        };
        let from = |source: SocketAddrV4| Arrival {
            source,
            ..arrival()
        };
        assert!(heard(from(ENTERING)));
        assert!(heard(from(OTHER)));
        for stranger in [
            SocketAddrV4::new(*ENTERING.ip(), 24801),
            "192.168.51.11:24800".parse().unwrap(),
            "8.8.8.8:24800".parse().unwrap(),
            "192.168.50.255:24800".parse().unwrap(),
            "192.168.50.0:24800".parse().unwrap(),
            SHOWING,
        ] {
            assert!(!heard(from(stranger)), "{stranger}");
        }
        assert!(!heard(Arrival {
            interface_index: INDEX + 1,
            ..from(ENTERING)
        }));
        assert!(!heard(Arrival {
            destination: *OTHER.ip(),
            ..from(ENTERING)
        }));

        chosen.set(ENTERING).unwrap();
        assert!(heard(from(ENTERING)));
        assert!(!heard(from(OTHER)), "only the chosen host is heard");

        // Replies Quinn queued for another host of the subnet may leave; nothing else may.
        let to = |destination: SocketAddrV4| Transmit {
            destination: destination.into(),
            src_ip: None,
            ..transmit()
        };
        assert!(socket.try_send(&to(OTHER)).is_ok());
        assert!(!socket.signal.is_revoked());
        let _ = socket.try_send(&to("192.168.51.11:24800".parse().unwrap()));
        assert!(socket.signal.is_revoked(), "a send off the subnet revokes");
    }

    #[test]
    fn a_pairing_listener_socket_must_match_its_adapter() {
        let io = || TestIo::new(SHOWING, Arc::default(), Arc::default());
        let chosen = Arc::new(OnceLock::new());
        let mut wrong_prefix = memory_interface(SHOWING);
        wrong_prefix.prefix_len = 32;
        for (interface, local) in [
            (wrong_prefix, SHOWING),
            (memory_interface(OTHER), SHOWING),
            (
                memory_interface(SHOWING),
                SocketAddrV4::new(*SHOWING.ip(), 0),
            ),
            (memory_interface(OTHER), OTHER),
        ] {
            let signal = RevocationSignal::default();
            assert!(
                GuardedSocket::listening(io(), &interface, local, chosen.clone(), signal).is_err()
            );
        }
        let revoked = RevocationSignal::default();
        revoked.revoke();
        let interface = memory_interface(SHOWING);
        assert!(GuardedSocket::listening(io(), &interface, SHOWING, chosen, revoked).is_err());
    }

    #[tokio::test]
    async fn the_right_code_pairs_and_each_side_learns_the_other_exactly() {
        let [at_showing, at_entering] = memory_link([SHOWING, ENTERING]);
        let showing_id = DeviceIdentity::generate().unwrap();
        let entering_id = DeviceIdentity::generate().unwrap();
        let showing = listener(at_showing, &showing_id);
        let entering = dialer(at_entering, ENTERING, &entering_id);
        let code = PairingCode::generate(*SHOWING.ip()).unwrap();
        let typed = code.display().to_ascii_lowercase().replace('-', " ");
        let (at_showing, at_entering) = tokio::time::timeout(DEADLINE * 5, async {
            tokio::join!(
                showing_side(&showing, &code, &showing_id),
                entering_side(&entering, &typed, &entering_id)
            )
        })
        .await
        .expect("pairing finished");
        let at_showing = at_showing.unwrap();
        let at_entering = at_entering.unwrap();
        assert_eq!(at_showing.certificate, entering_id.certificate_der());
        assert_eq!(at_showing.endpoint, ENTERING);
        assert_eq!(at_showing.platform, Platform::Windows);
        assert_eq!(at_entering.certificate, showing_id.certificate_der());
        assert_eq!(at_entering.endpoint, SHOWING);
        assert_eq!(at_entering.platform, Platform::MacOs);
        assert!(
            showing.accept().await.is_err(),
            "one connection per listener"
        );
        showing.revoke();
        entering.revoke();
    }

    #[tokio::test]
    async fn a_wrong_code_fails_both_sides_and_burns_the_shown_code() {
        let [at_showing, at_entering, at_other] = memory_link([SHOWING, ENTERING, OTHER]);
        let showing_id = DeviceIdentity::generate().unwrap();
        let entering_id = DeviceIdentity::generate().unwrap();
        let showing = listener(at_showing, &showing_id);
        let entering = dialer(at_entering, ENTERING, &entering_id);
        let other = dialer(at_other, OTHER, &DeviceIdentity::generate().unwrap());
        let code = PairingCode::generate(*SHOWING.ip()).unwrap();
        let wrong = PairingCode::generate(*SHOWING.ip()).unwrap().display();
        let (at_showing, at_entering) = tokio::time::timeout(DEADLINE * 5, async {
            tokio::join!(
                showing_side(&showing, &code, &showing_id),
                entering_side(&entering, &wrong, &entering_id)
            )
        })
        .await
        .expect("both sides gave up promptly");
        assert_eq!(at_showing.unwrap_err(), PairingFailure::NotConfirmed);
        assert_eq!(at_entering.unwrap_err(), PairingFailure::NotConfirmed);

        // The right code no longer gets anywhere, from the same host or another.
        assert!(unanswered(&entering).await);
        assert!(unanswered(&other).await);
        assert!(showing.accept().await.is_err());
        for endpoint in [&showing, &entering, &other] {
            endpoint.revoke();
        }
    }

    #[tokio::test]
    async fn a_failed_handshake_from_another_computer_locks_no_one_out() {
        let [at_showing, at_entering, at_other] = memory_link([SHOWING, ENTERING, OTHER]);
        let showing_id = DeviceIdentity::generate().unwrap();
        let entering_id = DeviceIdentity::generate().unwrap();
        let showing = listener(at_showing, &showing_id);
        let entering = dialer(at_entering, ENTERING, &entering_id);
        // A third paired computer still dialing this one for sharing.
        let sharing = guarded(
            dialing_socket(at_other, OTHER),
            &DeviceIdentity::generate().unwrap(),
            &[(SHOWING, &showing_id)],
        );
        let code = PairingCode::generate(*SHOWING.ip()).unwrap();
        let typed = code.display();
        let (at_showing, at_entering) = tokio::time::timeout(DEADLINE * 5, async {
            tokio::join!(showing_side(&showing, &code, &showing_id), async {
                let dialed =
                    tokio::time::timeout(UNANSWERED, async { sharing.connect().unwrap().await })
                        .await;
                assert!(dialed.map_or(true, |dialed| dialed.is_err()));
                entering_side(&entering, &typed, &entering_id).await
            })
        })
        .await
        .expect("pairing finished");
        assert_eq!(at_showing.unwrap().endpoint, ENTERING);
        assert_eq!(
            at_entering.unwrap().certificate,
            showing_id.certificate_der()
        );
        for endpoint in [&showing, &entering] {
            endpoint.revoke();
        }
        sharing.revoke();
    }

    #[tokio::test]
    async fn a_revoked_listener_answers_no_one() {
        let [at_showing, at_entering] = memory_link([SHOWING, ENTERING]);
        let showing = listener(at_showing, &DeviceIdentity::generate().unwrap());
        let entering = dialer(at_entering, ENTERING, &DeviceIdentity::generate().unwrap());
        showing.revoker().revoke();
        assert!(showing.is_revoked());
        assert!(showing.accept().await.is_err());
        assert!(unanswered(&entering).await);
        entering.revoke();
    }

    #[tokio::test]
    async fn a_sharing_dialer_never_reaches_a_pairing_listener() {
        let [at_showing, at_entering] = memory_link([SHOWING, ENTERING]);
        let showing_id = DeviceIdentity::generate().unwrap();
        let entering_id = DeviceIdentity::generate().unwrap();
        let showing = listener(at_showing, &showing_id);
        let sharing = guarded(
            dialing_socket(at_entering, ENTERING),
            &entering_id,
            &[(SHOWING, &showing_id)],
        );
        let (dialed, accepted) = tokio::join!(
            tokio::time::timeout(UNANSWERED, async { sharing.connect().unwrap().await }),
            tokio::time::timeout(UNANSWERED, showing.accept())
        );
        assert!(dialed.map_or(true, |dialed| dialed.is_err()));
        assert!(accepted.map_or(true, |accepted| accepted.is_err()));
        showing.revoke();
        sharing.revoke();
    }
}
