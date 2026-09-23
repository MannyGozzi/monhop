//! A dropped input packet between two real quinn endpoints on a LAN-like path: MonHop's transport
//! must deliver an isolated input within a bound that quinn's default loss recovery cannot meet.

use std::{
    fmt,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use monhop_transport::crypto::{
    DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer,
};
use quinn::{Endpoint, TransportConfig};
use tokio::{
    sync::mpsc::{UnboundedSender, unbounded_channel},
    time::{sleep, timeout},
};

/// Loopback plus this each way gives the ~6 ms RTT MonHop sees between two machines on a LAN.
const ONE_WAY_DELAY: Duration = Duration::from_millis(3);
/// Larger than any ACK-only or probe packet, so the relay can pick the input packet unseen.
const MESSAGE_LEN: usize = 256;
const WARM_UP_ROUND_TRIPS: usize = 10;
/// Outlasts quinn's default 25 ms delayed ACK even on a 15.6 ms Windows timer tick.
const QUIESCE: Duration = Duration::from_millis(80);
const TRIALS: usize = 5;
/// MonHop's PTO carries the peer's 1 ms max ACK delay in place of 25 ms. Load slows both configs
/// alike, so only the gap between them is asserted, well under the 24 ms it structurally is.
const MIN_SAVING: Duration = Duration::from_millis(15);
const RELAY_POLL: Duration = Duration::from_millis(20);
const TEST_DEADLINE: Duration = Duration::from_secs(20);

#[tokio::test]
async fn a_dropped_input_packet_recovers_faster_with_monhop_transport() {
    measure_quinn_timers_not_the_os_tick();
    timeout(TEST_DEADLINE, async {
        let quinn_defaults = measure(Some(quinn_default_transport())).await;
        let monhop = measure(None).await;
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
    })
    .await
    .expect("loss recovery test exceeded its deadline");
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

/// Times each dropped input from write to read; `None` keeps MonHop's own transport settings.
async fn measure(transport: Option<Arc<TransportConfig>>) -> Recovery {
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

    let server = Endpoint::server(server_config, loopback()).expect("server endpoint");
    let relay = LossyRelay::start(server.local_addr().expect("server address"));
    let (arrivals, mut arrived) = unbounded_channel();
    let receiver = tokio::spawn(echo_then_time_inputs(server.clone(), arrivals));
    let mut client = Endpoint::client(loopback()).expect("client endpoint");
    client.set_default_client_config(client_config);
    let connection = client
        .connect(relay.address, LOCAL_TLS_SERVER_NAME)
        .expect("valid TLS name")
        .await
        .expect("handshake through the relay");
    let (mut send, mut recv) = connection.open_bi().await.expect("input stream");

    let mut delays = Vec::with_capacity(TRIALS);
    let mut echo = [0_u8; MESSAGE_LEN];
    for dropped in 1..=TRIALS as u64 {
        for _ in 0..WARM_UP_ROUND_TRIPS {
            send.write_all(&[0; MESSAGE_LEN])
                .await
                .expect("warm-up input");
            recv.read_exact(&mut echo).await.expect("warm-up echo");
        }
        sleep(QUIESCE).await;
        relay.loss.arm();
        let sent = Instant::now();
        send.write_all(&[1; MESSAGE_LEN]).await.expect("input");
        let received = arrived.recv().await.expect("input arrival");
        assert_eq!(relay.loss.dropped.load(Ordering::SeqCst), dropped);
        delays.push(received - sent);
    }

    let stats = connection.stats();
    let server_connection = receiver.await.expect("receiver task");
    connection.close(0_u32.into(), b"measured");
    server_connection.close(0_u32.into(), b"measured");
    client.close(0_u32.into(), b"measured");
    server.close(0_u32.into(), b"measured");
    Recovery {
        delays,
        smoothed_rtt: stats.path.rtt,
        lost_packets: stats.path.lost_packets,
        ack_frequency_frames: stats.frame_tx.ack_frequency,
    }
}

async fn echo_then_time_inputs(
    server: Endpoint,
    arrivals: UnboundedSender<Instant>,
) -> quinn::Connection {
    let connection = server
        .accept()
        .await
        .expect("incoming connection")
        .await
        .expect("server handshake");
    let (mut send, mut recv) = connection.accept_bi().await.expect("input stream");
    let mut message = [0_u8; MESSAGE_LEN];
    for _ in 0..TRIALS {
        for _ in 0..WARM_UP_ROUND_TRIPS {
            recv.read_exact(&mut message).await.expect("warm-up input");
            send.write_all(&message).await.expect("warm-up echo");
        }
        recv.read_exact(&mut message).await.expect("input");
        arrivals
            .send(Instant::now())
            .expect("measurement is waiting");
    }
    connection
}

/// Tokio's timers fire on the 15.6 ms Windows tick unless the process asks for 1 ms, as the app
/// does at startup; this test process has to ask for itself.
#[cfg(windows)]
fn measure_quinn_timers_not_the_os_tick() {
    assert_eq!(time_begin_period(1), 0, "1 ms timer resolution");
}

#[cfg(not(windows))]
fn measure_quinn_timers_not_the_os_tick() {}

// SAFETY: timeBeginPeriod takes one integer and only changes this process's timer resolution.
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    #[link_name = "timeBeginPeriod"]
    safe fn time_begin_period(period_millis: u32) -> u32;
}

/// Quinn's defaults for every knob loss recovery reads; MTU probing is off as in MonHop so the
/// relay only ever sees real traffic. MonHop's other settings do not touch this timeline.
fn quinn_default_transport() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.mtu_discovery_config(None);
    Arc::new(transport)
}

type Delayed = (Instant, Vec<u8>);

/// Forwards client datagrams to the server and back, each held `ONE_WAY_DELAY` in order.
///
/// Plain threads keep that delay exact on every OS; tokio timers tick at 15.6 ms on Windows.
struct LossyRelay {
    address: SocketAddr,
    loss: Arc<Loss>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl LossyRelay {
    fn start(server: SocketAddr) -> Self {
        let front = relay_socket();
        let back = relay_socket();
        let address = front.local_addr().expect("relay address");
        let client = Arc::new(OnceLock::new());
        let server = Arc::new(OnceLock::from(server));
        let loss = Arc::new(Loss::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (to_server, toward_server) = channel();
        let (to_client, toward_client) = channel();
        let threads = vec![
            thread::spawn({
                let (front, client, loss, stop) =
                    (front.clone(), client.clone(), loss.clone(), stop.clone());
                move || receive(&front, &client, &loss, &stop, &to_server)
            }),
            thread::spawn({
                let (back, server) = (back.clone(), server.clone());
                move || deliver(&back, &server, toward_server)
            }),
            thread::spawn({
                let stop = stop.clone();
                move || receive(&back, &server, &Loss::default(), &stop, &to_client)
            }),
            thread::spawn(move || deliver(&front, &client, toward_client)),
        ];
        Self {
            address,
            loss,
            stop,
            threads,
        }
    }
}

impl Drop for LossyRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for relay_thread in self.threads.drain(..) {
            relay_thread.join().expect("relay thread");
        }
    }
}

#[derive(Default)]
struct Loss {
    armed: AtomicBool,
    dropped: AtomicU64,
}

impl Loss {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Only a 1-RTT datagram big enough to hold a whole input message is ever dropped.
    fn takes(&self, datagram: &[u8]) -> bool {
        let short_header = datagram.first().is_some_and(|byte| byte & 0x80 == 0);
        let taken = short_header
            && datagram.len() > MESSAGE_LEN
            && self.armed.swap(false, Ordering::SeqCst);
        if taken {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
        taken
    }
}

fn relay_socket() -> Arc<UdpSocket> {
    let socket = UdpSocket::bind(loopback()).expect("relay socket");
    socket
        .set_read_timeout(Some(RELAY_POLL))
        .expect("relay read timeout");
    Arc::new(socket)
}

fn receive(
    socket: &UdpSocket,
    sender: &OnceLock<SocketAddr>,
    loss: &Loss,
    stop: &AtomicBool,
    queue: &Sender<Delayed>,
) {
    let mut buffer = vec![0_u8; usize::from(u16::MAX)];
    while !stop.load(Ordering::SeqCst) {
        // Timeouts let `stop` end the thread; Windows also surfaces ICMP unreachables here.
        let Ok((length, source)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let _ = sender.set(source);
        let datagram = &buffer[..length];
        if !loss.takes(datagram)
            && queue
                .send((Instant::now() + ONE_WAY_DELAY, datagram.to_vec()))
                .is_err()
        {
            return;
        }
    }
}

fn deliver(socket: &UdpSocket, destination: &OnceLock<SocketAddr>, queue: Receiver<Delayed>) {
    for (due, datagram) in queue {
        thread::sleep(due.saturating_duration_since(Instant::now()));
        if let Some(&address) = destination.get() {
            let _ = socket.send_to(&datagram, address);
        }
    }
}

fn paired(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("complete out-of-band fingerprint matches peer DER")
}

fn loopback() -> SocketAddr {
    "127.0.0.1:0"
        .parse()
        .expect("explicit IPv4 loopback address")
}
