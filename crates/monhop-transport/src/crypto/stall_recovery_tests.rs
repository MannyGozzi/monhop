//! A multi-second outage on a simulated, fully deterministic network: once it ends, heartbeats
//! must resume at once instead of after quinn's backed-off probe.

use std::{
    collections::VecDeque,
    net::{Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use monhop_core::Point;
use monhop_protocol::{Frame, Message, Motion, SessionEpoch};
use quinn_proto::{
    ConnectionHandle, ConnectionStats, DatagramEvent, Dir, Endpoint, EndpointConfig, ReadError,
    StreamId, WriteError,
    congestion::{ControllerFactory, CubicConfig},
};

use super::{
    CONGESTION_WINDOW_FLOOR, DeviceIdentity, FlooredCubic, LOCAL_TLS_SERVER_NAME, SecureQuicConfig,
    VerifiedPeer, transport_config_with,
};
use crate::session_health::{HEARTBEAT_INTERVAL, HOLD_LIMIT, PEER_LIVENESS, RETREAT_AFTER};

/// Each way, for the ~6 ms RTT MonHop sees between two machines on a LAN.
const ONE_WAY_DELAY: Duration = Duration::from_millis(3);
/// Heartbeats flowing before and between the drops.
const STEADY: Duration = Duration::from_millis(500);
/// A Wi-Fi stall long enough for persistent congestion to collapse Cubic's window.
const STALL: Duration = Duration::from_millis(500);
/// Far past the point a collapsed window fills, inside the session's hold.
const OUTAGE: Duration = Duration::from_secs(3);
/// Past the input written as the outage began: from here on a sender sends heartbeats or probes.
const LATE_IN_OUTAGE: Duration = Duration::from_secs(1);
/// A 1000 Hz mouse moving as the outage begins, until the source holds.
const MOTION_INTERVAL: Duration = Duration::from_millis(1);
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(1);
/// Fails a harness bug loudly instead of hanging the test.
const MAX_STEPS: usize = 1_000_000;

const DIALER: usize = 0;
const LISTENER: usize = 1;
const RNG_SEEDS: [[u8; 32]; 2] = [[0x11; 32], [0x22; 32]];
const PORTS: [u16; 2] = [44_433, 4_433];

#[test]
fn heartbeats_resume_at_once_after_an_outage_only_with_the_floored_window() {
    let floored = outage(Arc::new(FlooredCubic::default()), false);
    let plain = outage(Arc::new(CubicConfig::default()), false);
    let input = outage(Arc::new(FlooredCubic::default()), true);
    let plain_input = outage(Arc::new(CubicConfig::default()), true);
    for (name, run) in [
        ("floored", &floored),
        ("plain cubic", &plain),
        ("floored, input as it began", &input),
        ("plain cubic, input as it began", &plain_input),
    ] {
        println!("{OUTAGE:?} outage, {name}:");
        for (side, direction) in ["dialer", "listener"].iter().zip(run) {
            println!("  {side} sending: {direction:?}");
        }
    }

    let resumed_within = HEARTBEAT_INTERVAL * 2;
    let heartbeats_due = (OUTAGE - LATE_IN_OUTAGE).as_millis() / HEARTBEAT_INTERVAL.as_millis();
    let hold = PEER_LIVENESS + HOLD_LIMIT;
    for direction in &floored {
        assert!(direction.window >= CONGESTION_WINDOW_FLOOR, "{direction:?}");
        assert!(
            u128::from(direction.late_datagrams) >= heartbeats_due,
            "{direction:?}"
        );
        assert!(
            direction
                .resumed
                .is_some_and(|after| after <= resumed_within),
            "{direction:?}"
        );
        // A whole hold of heartbeats, plus the one-packet margin quinn keeps, fits the floor.
        let per_hold = u128::from(direction.outage_bytes) * hold.as_millis() / OUTAGE.as_millis();
        assert!(
            per_hold + u128::from(direction.mtu) < u128::from(CONGESTION_WINDOW_FLOOR),
            "{direction:?}"
        );
    }
    for direction in &plain {
        // The stall left Cubic at its minimum, which the outage fills within moments.
        assert_eq!(
            direction.window,
            2 * u64::from(direction.mtu),
            "{direction:?}"
        );
        assert!(
            u128::from(direction.late_datagrams) * 4 < heartbeats_due,
            "{direction:?}"
        );
        assert!(
            direction
                .resumed
                .is_none_or(|after| after > resumed_within * 5),
            "{direction:?}"
        );
    }
    // Input written as the outage began used up its stream's credit before the source would hold,
    // yet the peer's heartbeats still bring both directions back at once.
    let source = &input[DIALER];
    assert!(source.backlog > 0, "{source:?}");
    assert!(
        source.credit_ran_out.is_some_and(|at| at < RETREAT_AFTER),
        "{source:?}"
    );
    for direction in &input {
        assert!(
            direction
                .resumed
                .is_some_and(|after| after <= resumed_within),
            "{direction:?}"
        );
    }
}

/// What one direction showed: the sender's window once the stall was over, the datagrams it sent
/// late in the outage, its bytes over the whole outage, the frames still waiting for stream credit
/// when the outage ended and how long into the outage a write first waited, and how long after
/// the outage its peer first read one of its frames.
#[derive(Debug)]
struct Direction {
    window: u64,
    late_datagrams: u64,
    outage_bytes: u64,
    mtu: u16,
    backlog: usize,
    credit_ran_out: Option<Duration>,
    resumed: Option<Duration>,
}

/// Steady heartbeats both ways, a stall, steady again, then an `OUTAGE`: everything either side
/// sends is dropped. With `input`, the dialer also writes motion until it would hold.
fn outage(congestion: Arc<dyn ControllerFactory + Send + Sync>, input: bool) -> [Direction; 2] {
    let mut harness = Harness::connect(congestion);
    harness.run_for(STEADY);
    harness.gated = true;
    harness.run_for(STALL);
    harness.gated = false;
    harness.run_for(STEADY);
    let windows = harness.stats().map(|stats| stats.path.cwnd);

    harness.gated = true;
    let began = harness.time;
    for side in &mut harness.sides {
        side.outbox.first_blocked = None;
    }
    if input {
        harness.input = Some((began, began + RETREAT_AFTER));
    }
    let before = harness.stats();
    harness.run_for(LATE_IN_OUTAGE);
    let late = harness.stats();
    harness.run_for(OUTAGE - LATE_IN_OUTAGE);
    let after = harness.stats();
    let backlogs = harness
        .sides
        .each_ref()
        .map(|side| side.outbox.frames.len());
    let blocked = harness
        .sides
        .each_ref()
        .map(|side| side.outbox.first_blocked.map(|at| at - began));

    harness.gated = false;
    let opened = harness.time;
    let heard_before = harness.sides.each_ref().map(|side| side.heard.len());
    harness.run_until(opened + HOLD_LIMIT, |harness| {
        harness
            .sides
            .iter()
            .zip(heard_before)
            .all(|(side, before)| side.heard.len() > before)
    });
    [DIALER, LISTENER].map(|sender| {
        let reader = 1 - sender;
        Direction {
            window: windows[sender],
            late_datagrams: after[sender].udp_tx.datagrams - late[sender].udp_tx.datagrams,
            outage_bytes: after[sender].udp_tx.bytes - before[sender].udp_tx.bytes,
            mtu: after[sender].path.current_mtu,
            backlog: backlogs[sender],
            credit_ran_out: blocked[sender],
            resumed: harness.sides[reader]
                .heard
                .get(heard_before[reader])
                .map(|at| *at - opened),
        }
    })
}

fn paired(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .unwrap()
}

fn address(side: usize) -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), PORTS[side])
}

fn encoded(sequence: u64, message: Message) -> Vec<u8> {
    let mut bytes = Vec::new();
    Frame::new(SessionEpoch::new(1).unwrap(), sequence, message)
        .encode_into(&mut bytes)
        .unwrap();
    bytes
}

/// One frame waiting for stream credit.
struct Queued {
    heartbeat: bool,
    bytes: Vec<u8>,
}

/// Frames in send order, kept as the session's outbox keeps them: a newer heartbeat replaces an
/// unsent one, and nothing else is ever dropped or moved ahead.
#[derive(Default)]
struct Outbox {
    frames: VecDeque<Queued>,
    /// Bytes of the front frame already written.
    written: usize,
    /// Where each whole frame written so far ends in the stream, in order.
    ends: Vec<u64>,
    first_blocked: Option<Instant>,
}

impl Outbox {
    fn queue_heartbeat(&mut self, bytes: Vec<u8>) {
        let unsent = usize::from(self.written > 0);
        if let Some(stale) = self
            .frames
            .iter()
            .skip(unsent)
            .position(|queued| queued.heartbeat)
        {
            self.frames.remove(stale + unsent);
        }
        self.frames.push_back(Queued {
            heartbeat: true,
            bytes,
        });
    }

    fn queue_input(&mut self, bytes: Vec<u8>) {
        self.frames.push_back(Queued {
            heartbeat: false,
            bytes,
        });
    }
}

/// One endpoint, the single connection it holds, and the application on top of it.
struct Side {
    endpoint: Endpoint,
    remote: SocketAddr,
    connection: Option<(ConnectionHandle, quinn_proto::Connection)>,
    timeout: Option<Instant>,
    outbox: Outbox,
    next_beat: Option<Instant>,
    sequence: u64,
    read: u64,
    /// When each of the peer's frames was read whole, in order.
    heard: Vec<Instant>,
}

impl Side {
    fn connection(&mut self) -> &mut quinn_proto::Connection {
        &mut self.connection.as_mut().unwrap().1
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

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
                    let accepted = self.endpoint.accept(incoming, now, &mut buf, None);
                    let accepted = accepted.unwrap_or_else(|error| panic!("{:?}", error.cause));
                    assert!(self.connection.replace(accepted).is_none());
                } else {
                    let transmit = self.endpoint.retry(incoming, &mut buf).unwrap();
                    out.push(buf[..transmit.size].to_vec());
                }
            }
            DatagramEvent::ConnectionEvent(_, event) => self.connection().handle_event(event),
            DatagramEvent::Response(transmit) => out.push(buf[..transmit.size].to_vec()),
        }
    }

    /// One pass of timers, endpoint events and transmits; whether it did anything.
    fn drive(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) -> bool {
        let Some((handle, connection)) = &mut self.connection else {
            return false;
        };
        let mut progressed = false;
        if self.timeout.is_some_and(|timeout| timeout <= now) {
            self.timeout = None;
            connection.handle_timeout(now);
            progressed = true;
        }
        while let Some(event) = connection.poll_endpoint_events() {
            progressed = true;
            if let Some(event) = self.endpoint.handle_event(*handle, event) {
                connection.handle_event(event);
            }
        }
        while connection.poll().is_some() {}
        let mut buf = Vec::new();
        while let Some(transmit) = connection.poll_transmit(now, 1, &mut buf) {
            progressed = true;
            out.push(buf[..transmit.size].to_vec());
            buf.clear();
        }
        self.timeout = connection.poll_timeout();
        progressed
    }

    /// Writes queued frames until the stream refuses; whether it wrote anything.
    fn flush(&mut self, now: Instant, stream: StreamId) -> bool {
        let mut progressed = false;
        while let Some(front) = self.outbox.frames.front() {
            let (len, written) = (front.bytes.len(), self.outbox.written);
            let Some((_, connection)) = &mut self.connection else {
                break;
            };
            match connection
                .send_stream(stream)
                .write(&front.bytes[written..])
            {
                Ok(0) => break,
                Ok(more) => {
                    progressed = true;
                    self.outbox.written += more;
                    if self.outbox.written < len {
                        continue;
                    }
                    let end = self.outbox.ends.last().copied().unwrap_or(0) + len as u64;
                    self.outbox.ends.push(end);
                    self.outbox.frames.pop_front();
                    self.outbox.written = 0;
                }
                Err(WriteError::Blocked) => {
                    self.outbox.first_blocked.get_or_insert(now);
                    break;
                }
                Err(error) => panic!("stream write failed: {error:?}"),
            }
        }
        progressed
    }

    /// Bytes the peer's stream yields now.
    fn read_stream(&mut self, stream: StreamId) -> u64 {
        let Some((_, connection)) = &mut self.connection else {
            return 0;
        };
        let mut receive = connection.recv_stream(stream);
        let Ok(mut chunks) = receive.read(true) else {
            return 0;
        };
        let mut read = 0;
        loop {
            match chunks.next(usize::MAX) {
                Ok(Some(chunk)) => read += chunk.bytes.len() as u64,
                Ok(None) | Err(ReadError::Blocked) => break,
                Err(error) => panic!("stream read failed: {error:?}"),
            }
        }
        let _ = chunks.finalize();
        read
    }
}

struct Packet {
    due: Instant,
    bytes: Vec<u8>,
}

/// Two `quinn_proto` endpoints driven sans-IO on a virtual clock: every datagram takes exactly
/// `ONE_WAY_DELAY`, unless the gate drops it, and time jumps straight to the next event.
struct Harness {
    time: Instant,
    sides: [Side; 2],
    /// Datagrams each side sent, still on their way.
    wire: [VecDeque<Packet>; 2],
    gated: bool,
    stream: Option<StreamId>,
    /// The dialer's next motion frame and when motion stops.
    input: Option<(Instant, Instant)>,
}

impl Harness {
    fn connect(congestion: Arc<dyn ControllerFactory + Send + Sync>) -> Self {
        let transport = Arc::new(transport_config_with(congestion));
        let identities = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
        let mut listening =
            SecureQuicConfig::server(&identities[LISTENER], &paired(&identities[DIALER])).unwrap();
        listening.transport_config(Arc::clone(&transport));
        let mut dialing =
            SecureQuicConfig::client(&identities[DIALER], &paired(&identities[LISTENER])).unwrap();
        dialing.transport_config(transport);

        let now = Instant::now();
        let [dialer_config, listener_config] = RNG_SEEDS.map(|seed| {
            let mut config = EndpointConfig::default();
            config.rng_seed(Some(seed));
            Arc::new(config)
        });
        let mut dialer = Endpoint::new(dialer_config, None, true, None);
        let listener = Endpoint::new(listener_config, Some(Arc::new(listening)), true, None);
        let dialed = dialer
            .connect(now, dialing, address(LISTENER), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let side = |endpoint, remote, connection| Side {
            endpoint,
            remote,
            connection,
            timeout: None,
            outbox: Outbox::default(),
            next_beat: None,
            sequence: 0,
            read: 0,
            heard: Vec::new(),
        };
        let mut harness = Self {
            time: now,
            sides: [
                side(dialer, address(LISTENER), Some(dialed)),
                side(listener, address(DIALER), None),
            ],
            wire: Default::default(),
            gated: false,
            stream: None,
            input: None,
        };
        let connected = harness.run_until(now + HANDSHAKE_LIMIT, |harness| {
            harness.sides.iter().all(|side| {
                side.connection
                    .as_ref()
                    .is_some_and(|(_, connection)| !connection.is_handshaking())
            })
        });
        assert!(connected, "handshake");

        // The listener learns of the stream from its first frame, and only then writes on it.
        let stream = harness.sides[DIALER]
            .connection()
            .streams()
            .open(Dir::Bi)
            .unwrap();
        harness.stream = Some(stream);
        harness.beat(DIALER);
        let opened = harness.run_until(harness.time + HANDSHAKE_LIMIT, |harness| {
            !harness.sides[LISTENER].heard.is_empty()
        });
        assert!(opened, "stream");
        let started = harness.time;
        harness.sides[DIALER].next_beat = Some(started + HEARTBEAT_INTERVAL);
        harness.sides[LISTENER].next_beat = Some(started + HEARTBEAT_INTERVAL / 2);
        harness
    }

    fn beat(&mut self, side: usize) {
        let side = &mut self.sides[side];
        let sequence = side.next_sequence();
        side.outbox
            .queue_heartbeat(encoded(sequence, Message::Ping(sequence)));
    }

    fn stats(&mut self) -> [ConnectionStats; 2] {
        let [dialer, listener] = &mut self.sides;
        [dialer.connection().stats(), listener.connection().stats()]
    }

    fn run_for(&mut self, duration: Duration) {
        let deadline = self.time + duration;
        self.run_until(deadline, |_| false);
    }

    /// Steps events until `done`, or until `deadline`; whether `done` held.
    fn run_until(&mut self, deadline: Instant, mut done: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..MAX_STEPS {
            self.settle();
            if done(self) {
                return true;
            }
            match self.next_event() {
                Some(next) if next <= deadline => self.time = next,
                _ => {
                    self.time = deadline;
                    self.settle();
                    return done(self);
                }
            }
        }
        panic!("exceeded {MAX_STEPS} simulated steps");
    }

    fn next_event(&self) -> Option<Instant> {
        let sides = self
            .sides
            .iter()
            .flat_map(|side| [side.timeout, side.next_beat]);
        let wire = self
            .wire
            .iter()
            .map(|sent| sent.front().map(|packet| packet.due));
        let input = self
            .input
            .filter(|(next, until)| next < until)
            .map(|(next, _)| next);
        sides.chain(wire).chain([input]).flatten().min()
    }

    /// Runs everything due at the current time to a fixed point.
    fn settle(&mut self) {
        let now = self.time;
        for _ in 0..MAX_STEPS {
            let mut progressed = self.application(now);
            for (from, to) in [(DIALER, LISTENER), (LISTENER, DIALER)] {
                while self.wire[from]
                    .front()
                    .is_some_and(|packet| packet.due <= now)
                {
                    let packet = self.wire[from].pop_front().unwrap();
                    let mut out = Vec::new();
                    self.sides[to].handle_datagram(now, packet.bytes, &mut out);
                    self.send(to, out);
                    progressed = true;
                }
            }
            for side in [DIALER, LISTENER] {
                if let Some(stream) = self.stream {
                    progressed |= self.sides[side].flush(now, stream);
                }
                let mut out = Vec::new();
                while self.sides[side].drive(now, &mut out) {
                    progressed = true;
                }
                self.send(side, out);
            }
            for (reader, writer) in [(DIALER, LISTENER), (LISTENER, DIALER)] {
                progressed |= self.read(reader, writer);
            }
            if !progressed {
                return;
            }
        }
        panic!("no fixed point after {MAX_STEPS} passes");
    }

    /// Queues each heartbeat and motion frame due by `now`; whether any was.
    fn application(&mut self, now: Instant) -> bool {
        let mut progressed = false;
        for side in [DIALER, LISTENER] {
            while let Some(due) = self.sides[side].next_beat.filter(|due| *due <= now) {
                self.beat(side);
                self.sides[side].next_beat = Some(due + HEARTBEAT_INTERVAL);
                progressed = true;
            }
        }
        while let Some((next, until)) = self.input.filter(|(next, until)| next < until) {
            if next > now {
                break;
            }
            let dialer = &mut self.sides[DIALER];
            let sequence = dialer.next_sequence();
            let motion = Message::Motion(Motion::Relative(Point::new(1.0, 0.0)));
            dialer.outbox.queue_input(encoded(sequence, motion));
            self.input = Some((next + MOTION_INTERVAL, until));
            progressed = true;
        }
        progressed
    }

    fn send(&mut self, from: usize, datagrams: Vec<Vec<u8>>) {
        if self.gated {
            return;
        }
        let due = self.time + ONE_WAY_DELAY;
        self.wire[from].extend(datagrams.into_iter().map(|bytes| Packet { due, bytes }));
    }

    /// Reads what the writer's stream yields and notes each frame now whole; whether it read any.
    fn read(&mut self, reader: usize, writer: usize) -> bool {
        let Some(stream) = self.stream else {
            return false;
        };
        let read = self.sides[reader].read_stream(stream);
        if read == 0 {
            return false;
        }
        let total = self.sides[reader].read + read;
        self.sides[reader].read = total;
        let whole = self.sides[writer]
            .outbox
            .ends
            .iter()
            .take_while(|end| **end <= total)
            .count();
        let now = self.time;
        let heard = &mut self.sides[reader].heard;
        heard.resize(whole.max(heard.len()), now);
        true
    }
}
