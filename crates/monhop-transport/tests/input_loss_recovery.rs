//! A dropped input packet on a simulated, fully deterministic network: MonHop's transport must
//! recover markedly sooner than quinn's default loss recovery.

use std::{
    collections::VecDeque,
    fmt,
    net::{Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use monhop_transport::crypto::{
    DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer,
};
use quinn_proto::{
    ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig,
    Event, ReadError, ServerConfig, StreamId, TransportConfig,
};

/// Each way, for the ~6 ms RTT MonHop sees between two machines on a LAN.
const ONE_WAY_DELAY: Duration = Duration::from_millis(3);
/// Larger than any ACK-only or probe packet, so the network can pick the input packet unseen.
const MESSAGE_LEN: usize = 256;
const WARM_UP_ROUND_TRIPS: usize = 10;
/// Outlasts quinn's default 25 ms delayed ACK.
const QUIESCE: Duration = Duration::from_millis(80);
const TRIALS: usize = 5;
/// MonHop's PTO carries the peer's 1 ms max ACK delay in place of 25 ms: the virtual clock measures
/// exactly 24 ms (17 against 41), and the margin keeps an rttvar difference from deciding it.
const MIN_SAVING: Duration = Duration::from_millis(20);
/// Fails a harness bug loudly instead of hanging the test.
const MAX_STEPS: usize = 100_000;
const MAX_SIMULATED_TIME: Duration = Duration::from_secs(5);

const CLIENT_RNG_SEED: [u8; 32] = [0x11; 32];
const SERVER_RNG_SEED: [u8; 32] = [0x22; 32];

#[test]
fn a_dropped_input_packet_recovers_faster_with_monhop_transport() {
    let quinn_defaults = measure(Some(quinn_default_transport()));
    let monhop = measure(None);
    println!("isolated input whose first packet is dropped, {ONE_WAY_DELAY:?} each way:");
    println!("  quinn defaults: {quinn_defaults}");
    println!("  monhop:         {monhop}");

    assert_eq!(quinn_defaults.ack_frequency_frames, 0);
    assert!(monhop.ack_frequency_frames >= 1, "monhop: {monhop}");
    for recovery in [&quinn_defaults, &monhop] {
        assert!(recovery.lost_packets >= TRIALS as u64, "{recovery}");
    }
    assert!(
        monhop.median() + MIN_SAVING <= quinn_defaults.median(),
        "monhop saves less than {MIN_SAVING:?}: {monhop} against {quinn_defaults}"
    );
}

struct Recovery {
    delays: Vec<Duration>,
    smoothed_rtt: Duration,
    lost_packets: u64,
    ack_frequency_frames: u64,
}

impl Recovery {
    fn median(&self) -> Duration {
        let mut sorted = self.delays.clone();
        sorted.sort();
        sorted[sorted.len() / 2]
    }
}

impl fmt::Display for Recovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "median {:.1?}, trials {:.1?}, srtt {:.1?}, lost packets {}",
            self.median(),
            self.delays,
            self.smoothed_rtt,
            self.lost_packets
        )
    }
}

/// Times each dropped input from write to read on a deterministic virtual clock; `None` keeps
/// MonHop's own transport settings.
fn measure(transport: Option<Arc<TransportConfig>>) -> Recovery {
    let server_identity = DeviceIdentity::generate().expect("server identity");
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let mut server_config = SecureQuicConfig::server(&server_identity, &paired(&client_identity))
        .expect("server config");
    let mut client_config = SecureQuicConfig::client(&client_identity, &paired(&server_identity))
        .expect("client config");
    if let Some(transport) = transport {
        server_config.transport_config(transport.clone());
        client_config.transport_config(transport);
    }

    let mut harness = Harness::connect(server_config, client_config);
    let stream = harness.open_bi_stream();

    let mut delays = Vec::with_capacity(TRIALS);
    for dropped in 1..=TRIALS as u64 {
        for _ in 0..WARM_UP_ROUND_TRIPS {
            harness.round_trip(stream);
        }
        harness.quiesce(QUIESCE);
        harness.loss.arm();
        let sent = harness.time;
        write_all(harness.client_conn(), stream, &[1; MESSAGE_LEN]);
        let mut received_len = 0;
        harness.run_until(
            |harness| {
                received_len +=
                    read_available(harness.server_conn(), stream, MESSAGE_LEN - received_len);
                received_len == MESSAGE_LEN
            },
            "input delivery",
        );
        assert_eq!(
            harness.loss.dropped, dropped,
            "exactly one datagram dropped per trial"
        );
        delays.push(harness.time - sent);
    }

    let stats = harness.client_conn().stats();
    Recovery {
        delays,
        smoothed_rtt: stats.path.rtt,
        lost_packets: stats.path.lost_packets,
        ack_frequency_frames: stats.frame_tx.ack_frequency,
    }
}

fn paired(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("complete out-of-band fingerprint matches peer DER")
}

/// Quinn's defaults for every knob loss recovery reads; MTU probing is off as in MonHop.
fn quinn_default_transport() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.mtu_discovery_config(None);
    Arc::new(transport)
}

fn write_all(connection: &mut Connection, stream: StreamId, mut data: &[u8]) {
    while !data.is_empty() {
        let written = connection
            .send_stream(stream)
            .write(data)
            .expect("stream write");
        data = &data[written..];
    }
}

/// Reads up to `max` bytes currently available; returns the number actually read.
fn read_available(connection: &mut Connection, stream: StreamId, max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let mut recv = connection.recv_stream(stream);
    let mut chunks = recv.read(true).expect("recv stream readable");
    let mut read = 0;
    while read < max {
        match chunks.next(max - read) {
            Ok(Some(chunk)) => read += chunk.bytes.len(),
            Ok(None) | Err(ReadError::Blocked) => break,
            Err(error) => panic!("stream read error: {error:?}"),
        }
    }
    let _ = chunks.finalize();
    read
}

/// Drives two `quinn_proto` endpoints sans-IO over a simulated network: every datagram is
/// delayed by exactly `ONE_WAY_DELAY`, and the virtual clock only advances to the next event.
struct Harness {
    time: Instant,
    epoch: Instant,
    client: Node,
    server: Node,
    wire: Wire,
    loss: Loss,
}

impl Harness {
    fn connect(server_config: ServerConfig, client_config: ClientConfig) -> Self {
        let now = Instant::now();
        let mut server_endpoint_config = EndpointConfig::default();
        server_endpoint_config.rng_seed(Some(SERVER_RNG_SEED));
        let mut client_endpoint_config = EndpointConfig::default();
        client_endpoint_config.rng_seed(Some(CLIENT_RNG_SEED));

        let server_endpoint = Endpoint::new(
            Arc::new(server_endpoint_config),
            Some(Arc::new(server_config)),
            true,
            None,
        );
        let mut client_endpoint = Endpoint::new(Arc::new(client_endpoint_config), None, true, None);
        let (client_ch, client_conn) = client_endpoint
            .connect(now, client_config, server_addr(), LOCAL_TLS_SERVER_NAME)
            .expect("valid TLS name");

        let mut harness = Self {
            time: now,
            epoch: now,
            client: Node {
                endpoint: client_endpoint,
                remote: server_addr(),
                connection: Some((client_ch, client_conn)),
                timeout: None,
            },
            server: Node {
                endpoint: server_endpoint,
                remote: client_addr(),
                connection: None,
                timeout: None,
            },
            wire: Wire::default(),
            loss: Loss::default(),
        };

        let mut client_connected = false;
        let mut server_connected = false;
        harness.run_until(
            |harness| {
                if let Some((_, conn)) = &mut harness.client.connection {
                    while let Some(event) = conn.poll() {
                        client_connected |= matches!(event, Event::Connected);
                    }
                }
                if let Some((_, conn)) = &mut harness.server.connection {
                    while let Some(event) = conn.poll() {
                        server_connected |= matches!(event, Event::Connected);
                    }
                }
                client_connected && server_connected
            },
            "handshake",
        );
        harness
    }

    /// Reserves a bidirectional stream id; nothing is sent until the first write, so the peer
    /// learns of it (and can be told to `accept` it) only once that data arrives.
    fn open_bi_stream(&mut self) -> StreamId {
        self.client_conn()
            .streams()
            .open(Dir::Bi)
            .expect("open stream")
    }

    /// One 256-byte message written by the client, read and echoed by the server, then read back.
    fn round_trip(&mut self, stream: StreamId) {
        write_all(self.client_conn(), stream, &[0; MESSAGE_LEN]);
        let mut server_read = 0;
        let mut client_read = 0;
        let mut echoed = false;
        self.run_until(
            |harness| {
                if !echoed {
                    server_read +=
                        read_available(harness.server_conn(), stream, MESSAGE_LEN - server_read);
                    if server_read == MESSAGE_LEN {
                        write_all(harness.server_conn(), stream, &[0; MESSAGE_LEN]);
                        echoed = true;
                    }
                }
                if echoed {
                    client_read +=
                        read_available(harness.client_conn(), stream, MESSAGE_LEN - client_read);
                }
                client_read == MESSAGE_LEN
            },
            "warm-up round trip",
        );
    }

    /// Advances virtual time by `duration`, settling anything scheduled before the deadline (such
    /// as a delayed ACK) and jumping straight past any dead time after it.
    fn quiesce(&mut self, duration: Duration) {
        let deadline = self.time + duration;
        let mut reached = false;
        for _ in 0..MAX_STEPS {
            self.settle();
            match self.next_wakeup() {
                Some(next) if next <= deadline => self.time = next,
                _ => {
                    reached = true;
                    break;
                }
            }
        }
        assert!(
            reached,
            "quiesce: exceeded {MAX_STEPS} simulated steps before {duration:?} elapsed"
        );
        self.time = deadline;
    }

    /// Steps the simulated network until `done` reports true, or fails loudly instead of hanging.
    fn run_until(&mut self, mut done: impl FnMut(&mut Self) -> bool, guard: &str) {
        for _ in 0..MAX_STEPS {
            if done(self) {
                return;
            }
            assert!(
                self.time - self.epoch < MAX_SIMULATED_TIME,
                "{guard}: exceeded {MAX_SIMULATED_TIME:?} of simulated time"
            );
            assert!(
                self.step(),
                "{guard}: network went idle before the condition was met"
            );
        }
        panic!("{guard}: exceeded {MAX_STEPS} simulated steps");
    }

    /// Advances to the next scheduled event and settles it. Returns false once idle.
    fn step(&mut self) -> bool {
        self.settle();
        let Some(next) = self.next_wakeup() else {
            return false;
        };
        self.time = next;
        self.settle();
        true
    }

    /// Delivers due datagrams and drives both connections at the current time, to a fixed point.
    fn settle(&mut self) {
        loop {
            let mut progressed = false;
            let mut client_out = Vec::new();
            let mut server_out = Vec::new();

            while self
                .wire
                .server_to_client
                .front()
                .is_some_and(|packet| packet.due <= self.time)
            {
                let packet = self
                    .wire
                    .server_to_client
                    .pop_front()
                    .expect("checked above");
                self.client
                    .handle_datagram(self.time, packet.bytes, &mut client_out);
                progressed = true;
            }
            while self
                .wire
                .client_to_server
                .front()
                .is_some_and(|packet| packet.due <= self.time)
            {
                let packet = self
                    .wire
                    .client_to_server
                    .pop_front()
                    .expect("checked above");
                self.server
                    .handle_datagram(self.time, packet.bytes, &mut server_out);
                progressed = true;
            }

            while self.client.drive(self.time, &mut client_out) {
                progressed = true;
            }
            while self.server.drive(self.time, &mut server_out) {
                progressed = true;
            }

            for bytes in client_out.drain(..) {
                if self.loss.takes(&bytes) {
                    continue;
                }
                self.wire.client_to_server.push_back(Packet {
                    due: self.time + ONE_WAY_DELAY,
                    bytes,
                });
            }
            for bytes in server_out.drain(..) {
                self.wire.server_to_client.push_back(Packet {
                    due: self.time + ONE_WAY_DELAY,
                    bytes,
                });
            }

            if !progressed {
                break;
            }
        }
    }

    fn next_wakeup(&self) -> Option<Instant> {
        [
            self.client.timeout,
            self.server.timeout,
            self.wire.client_to_server.front().map(|packet| packet.due),
            self.wire.server_to_client.front().map(|packet| packet.due),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn client_conn(&mut self) -> &mut Connection {
        &mut self.client.connection.as_mut().expect("client connected").1
    }

    fn server_conn(&mut self) -> &mut Connection {
        &mut self.server.connection.as_mut().expect("server connected").1
    }
}

/// One `quinn_proto` endpoint plus the single connection it ever holds in this harness.
struct Node {
    endpoint: Endpoint,
    remote: SocketAddr,
    connection: Option<(ConnectionHandle, Connection)>,
    timeout: Option<Instant>,
}

impl Node {
    fn handle_datagram(&mut self, now: Instant, data: Vec<u8>, out: &mut Vec<Vec<u8>>) {
        let mut buf = Vec::new();
        let Some(event) = self.endpoint.handle(
            now,
            self.remote,
            None,
            None,
            BytesMut::from(data.as_slice()),
            &mut buf,
        ) else {
            return;
        };
        match event {
            DatagramEvent::NewConnection(incoming) => {
                if incoming.remote_address_validated() {
                    match self.endpoint.accept(incoming, now, &mut buf, None) {
                        Ok((ch, conn)) => {
                            assert!(
                                self.connection.is_none(),
                                "one connection per node in this harness"
                            );
                            self.connection = Some((ch, conn));
                        }
                        Err(error) => panic!("server refused connection: {:?}", error.cause),
                    }
                } else {
                    let transmit = self
                        .endpoint
                        .retry(incoming, &mut buf)
                        .expect("retry token");
                    out.push(buf[..transmit.size].to_vec());
                }
            }
            DatagramEvent::ConnectionEvent(ch, event) => {
                let (existing, conn) = self
                    .connection
                    .as_mut()
                    .expect("connection event before the connection was accepted");
                debug_assert_eq!(ch, *existing);
                conn.handle_event(event);
            }
            DatagramEvent::Response(transmit) => {
                out.push(buf[..transmit.size].to_vec());
            }
        }
    }

    /// One pass of timers, endpoint events and outgoing transmits. Returns whether it did anything.
    fn drive(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) -> bool {
        let mut progressed = false;
        if self.timeout.is_some_and(|timeout| timeout <= now) {
            self.timeout = None;
            if let Some((_, conn)) = &mut self.connection {
                conn.handle_timeout(now);
            }
            progressed = true;
        }
        if let Some((ch, conn)) = &mut self.connection {
            while let Some(event) = conn.poll_endpoint_events() {
                progressed = true;
                if let Some(connection_event) = self.endpoint.handle_event(*ch, event) {
                    conn.handle_event(connection_event);
                }
            }
            let mut buf = Vec::new();
            while let Some(transmit) = conn.poll_transmit(now, 1, &mut buf) {
                progressed = true;
                out.push(buf[..transmit.size].to_vec());
                buf.clear();
            }
            self.timeout = conn.poll_timeout();
        }
        progressed
    }
}

#[derive(Default)]
struct Wire {
    client_to_server: VecDeque<Packet>,
    server_to_client: VecDeque<Packet>,
}

struct Packet {
    due: Instant,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Loss {
    armed: bool,
    dropped: u64,
}

impl Loss {
    fn arm(&mut self) {
        self.armed = true;
    }

    /// Only a 1-RTT datagram big enough to hold a whole input message is ever dropped.
    fn takes(&mut self, datagram: &[u8]) -> bool {
        let short_header = datagram.first().is_some_and(|byte| byte & 0x80 == 0);
        let taken = short_header && datagram.len() > MESSAGE_LEN && self.armed;
        if taken {
            self.armed = false;
            self.dropped += 1;
        }
        taken
    }
}

fn server_addr() -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 4433)
}

fn client_addr() -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 44433)
}
