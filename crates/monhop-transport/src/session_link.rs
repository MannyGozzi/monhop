//! Standing authenticated setup link between two paired computers.
//!
//! The link carries display topology and layout proposals only. It never starts native input,
//! changes pairing trust, or binds anything but the one selected interface and pinned peer.

use std::{
    collections::VecDeque,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use monhop_core::{DeviceId, RevocationSignal};
use monhop_protocol::{DisplayTopology, Frame, Message, PROTOCOL_VERSION, SessionEpoch, decode};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{
    crypto::CertificateFingerprint,
    session_handshake::{ControlStreams, NegotiatedSession, SessionPurpose},
    session_layout::MAX_LAYOUT_PAYLOAD_BYTES,
    session_native::current_displays,
    session_setup::{
        InspectedPeer, PreparedEndpoint, SetupFailure, is_transient, prepare_endpoint,
    },
};

/// Windows waits this long between dial attempts. It is the only reconnect cadence.
pub const LINK_DIAL_INTERVAL: Duration = Duration::from_secs(2);
/// One dial attempt covers connect plus negotiate; the Mac accepts without this bound.
pub const LINK_ATTEMPT_DEADLINE: Duration = Duration::from_secs(4);
pub const LINK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// A link with no peer frame for this long is dropped and re-established.
pub const LINK_SILENCE_DEADLINE: Duration = Duration::from_secs(5);
/// Bounds every wait in one transaction: the acknowledgement, the completion, and the confirmation.
pub const LINK_ACK_DEADLINE: Duration = Duration::from_secs(10);
pub const LINK_TOPOLOGY_POLL: Duration = Duration::from_secs(2);

const LINK_MAGIC: [u8; 4] = *b"LKLC";
const LINK_FORMAT_VERSION: u8 = 1;
/// Matches the Setup purpose wire value, so a Configure frame can never decode as a link frame.
const LINK_PURPOSE: u8 = 5;
const LINK_HEADER_LEN: usize = 64;
/// The three handshake frames occupy sequences 0 through 2 in each direction.
const LINK_START_SEQUENCE: u64 = 3;
const TOPOLOGY_FRAME_SEQUENCE: u64 = 0;
const LINK_GUARD_INTERVAL: Duration = Duration::from_millis(100);
const LINK_CLOSE_CODE: u32 = 5;
const LINK_CLOSE_REASON: &[u8] = b"setup link closed";
/// Frames read while a write is in flight. A peer that exceeds this is not following the cadence.
const MAX_PENDING_FRAMES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkRejectReason {
    Invalid,
    InspectionChanged,
    SaveFailed,
    Busy,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkDisconnect {
    PeerClosed,
    Silence,
    Transport,
    Handshake,
    Attempt,
}

// The completed-sync variant carries the layout bytes by design; boxing it would change the API.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum LinkEvent {
    Connecting {
        attempt: u32,
    },
    Connected {
        inspection: InspectedPeer,
    },
    TopologyChanged {
        inspection: InspectedPeer,
    },
    SyncStarted {
        sending: bool,
    },
    SyncCompleted {
        inspection: InspectedPeer,
        bytes: Vec<u8>,
        sending: bool,
    },
    SyncRejected {
        reason: LinkRejectReason,
        sending: bool,
    },
    /// The other computer's arranging state, sent on connect and on every change.
    PeerArranging {
        arranging: bool,
    },
    Disconnected {
        reason: LinkDisconnect,
    },
    Closed,
}

#[derive(Debug)]
pub enum LinkCommand {
    Propose {
        bytes: Vec<u8>,
    },
    /// This computer started or stopped arranging displays; the peer is told either way.
    Arranging(bool),
    Close,
}

/// One shared-file step. It runs on its own OS thread, never on the link's async runtime.
pub type LinkPersistStep =
    Arc<dyn Fn(&InspectedPeer, &[u8]) -> Result<(), LinkRejectReason> + Send + Sync>;

#[derive(Clone)]
pub struct LinkPersist {
    pub stage: LinkPersistStep,
    pub commit: LinkPersistStep,
    pub discard: Arc<dyn Fn() + Send + Sync>,
}

pub async fn run_setup_link(
    interface_id: &str,
    peer: CertificateFingerprint,
    cancel: &RevocationSignal,
    persist: LinkPersist,
    events: UnboundedSender<LinkEvent>,
    mut commands: UnboundedReceiver<LinkCommand>,
) -> Result<(), SetupFailure> {
    // The link carries no input authority, so both sides name the dialing computer as the source
    // and agree on it without asking the user anything.
    let prepared = prepare_endpoint(interface_id, None, cancel, peer)?;
    let connector = EndpointConnector { prepared };
    run_link(&connector, cancel, &persist, &events, &mut commands).await
}

/// Supplies one connection at a time. The production implementation owns a bound, pinned endpoint.
trait LinkConnector {
    /// Windows dials the pinned peer; macOS binds once and accepts.
    fn dials(&self) -> bool;

    fn next_connection(
        &self,
        cancel: &RevocationSignal,
    ) -> impl Future<Output = Result<quinn::Connection, SetupFailure>>;

    fn negotiate(
        &self,
        connection: quinn::Connection,
    ) -> impl Future<Output = Result<(NegotiatedSession, InspectedPeer), SetupFailure>>;

    /// `None` reports a transient enumeration failure, which must not drop a healthy link.
    fn local_displays(&self, inspection: &InspectedPeer) -> Option<DisplayTopology>;

    fn close(&self) -> impl Future<Output = ()>;
}

struct EndpointConnector {
    prepared: PreparedEndpoint,
}

impl LinkConnector for EndpointConnector {
    fn dials(&self) -> bool {
        self.prepared.dials()
    }

    async fn next_connection(
        &self,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        // A revoked endpoint can never be rebound, so retrying on it would spin forever.
        if self.prepared.endpoint().is_revoked() {
            return Err(SetupFailure::Cancelled);
        }
        if self.dials() {
            self.prepared.dial(cancel).await
        } else {
            self.prepared.accept(cancel).await
        }
    }

    async fn negotiate(
        &self,
        connection: quinn::Connection,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        self.prepared
            .negotiate_session(connection, SessionPurpose::Setup)
            .await
    }

    fn local_displays(&self, inspection: &InspectedPeer) -> Option<DisplayTopology> {
        current_displays(inspection.local_device).ok()
    }

    async fn close(&self) {
        let _ = self.prepared.endpoint().close_and_wait_idle().await;
    }
}

/// How one established link ended.
enum LinkExit {
    Closed,
    Cancelled,
    Disconnected(LinkDisconnect),
}

/// How a wait between connections ended.
enum IdleExit {
    Closed,
    Cancelled,
}

async fn run_link<C: LinkConnector>(
    connector: &C,
    cancel: &RevocationSignal,
    persist: &LinkPersist,
    events: &UnboundedSender<LinkEvent>,
    commands: &mut UnboundedReceiver<LinkCommand>,
) -> Result<(), SetupFailure> {
    let mut attempt = 0_u32;
    // Survives every reconnect: each fresh connection opens by telling the peer this state, so a
    // user who was already arranging is never reported as idle after a drop.
    let mut arranging = false;
    loop {
        if cancel.is_revoked() {
            return Err(SetupFailure::Cancelled);
        }
        attempt = attempt.saturating_add(1);
        log::debug!("setup link: connecting, attempt {attempt}");
        emit(events, LinkEvent::Connecting { attempt })?;
        let established = while_idle(
            connect_once(connector, cancel),
            cancel,
            events,
            commands,
            &mut arranging,
        )
        .await;
        let established = match established {
            Ok(established) => established,
            Err(IdleExit::Closed) => return closed(connector, events).await,
            Err(IdleExit::Cancelled) => return Err(SetupFailure::Cancelled),
        };
        let (session, inspection) = match established {
            Ok(established) => established,
            Err(SetupFailure::Cancelled) => return Err(SetupFailure::Cancelled),
            // A peer that answered and disagrees will disagree again; only reach failures retry.
            Err(error) if !is_transient(error) => {
                log::warn!("setup link: attempt {attempt} failed for good: {error:?}");
                return Err(error);
            }
            Err(error) => {
                log::debug!("setup link: attempt {attempt} failed: {error:?}");
                emit(
                    events,
                    LinkEvent::Disconnected {
                        reason: attempt_reason(error),
                    },
                )?;
                match wait_between_attempts(cancel, events, commands, &mut arranging).await? {
                    Waited::Continue => continue,
                    Waited::Closed => return closed(connector, events).await,
                }
            }
        };
        if session.purpose() != SessionPurpose::Setup
            || session.control.next_sequence() != LINK_START_SEQUENCE
        {
            emit(
                events,
                LinkEvent::Disconnected {
                    reason: LinkDisconnect::Handshake,
                },
            )?;
            match wait_between_attempts(cancel, events, commands, &mut arranging).await? {
                Waited::Continue => continue,
                Waited::Closed => return closed(connector, events).await,
            }
        }
        attempt = 0;
        log::info!(
            "setup link: connected to {:?} peer on {}",
            inspection.peer_platform,
            inspection.interface_id
        );
        emit(
            events,
            LinkEvent::Connected {
                inspection: inspection.clone(),
            },
        )?;
        let mut link = Link::new(session, inspection, &mut arranging, persist, events, cancel);
        let exit = link.run(commands, connector).await;
        drop(link);
        match exit {
            LinkExit::Closed => return closed(connector, events).await,
            LinkExit::Cancelled => return Err(SetupFailure::Cancelled),
            LinkExit::Disconnected(reason) => {
                log::warn!("setup link: disconnected ({reason:?})");
                emit(events, LinkEvent::Disconnected { reason })?;
                if matches!(
                    wait_between_attempts(cancel, events, commands, &mut arranging).await?,
                    Waited::Closed
                ) {
                    return closed(connector, events).await;
                }
            }
        }
    }
}

async fn connect_once<C: LinkConnector>(
    connector: &C,
    cancel: &RevocationSignal,
) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
    if connector.dials() {
        // One attempt covers connect plus negotiate, so a peer that never answers frees the dialer.
        return tokio::time::timeout(LINK_ATTEMPT_DEADLINE, async {
            let connection = connector.next_connection(cancel).await?;
            connector.negotiate(connection).await
        })
        .await
        .unwrap_or(Err(SetupFailure::Connection));
    }
    let connection = connector.next_connection(cancel).await?;
    tokio::time::timeout(LINK_ATTEMPT_DEADLINE, connector.negotiate(connection))
        .await
        .unwrap_or(Err(SetupFailure::Handshake))
}

async fn closed<C: LinkConnector>(
    connector: &C,
    events: &UnboundedSender<LinkEvent>,
) -> Result<(), SetupFailure> {
    connector.close().await;
    emit(events, LinkEvent::Closed)?;
    Ok(())
}

enum Waited {
    Continue,
    Closed,
}

async fn wait_between_attempts(
    cancel: &RevocationSignal,
    events: &UnboundedSender<LinkEvent>,
    commands: &mut UnboundedReceiver<LinkCommand>,
    arranging: &mut bool,
) -> Result<Waited, SetupFailure> {
    match while_idle(
        tokio::time::sleep(LINK_DIAL_INTERVAL),
        cancel,
        events,
        commands,
        arranging,
    )
    .await
    {
        Ok(()) => Ok(Waited::Continue),
        Err(IdleExit::Closed) => Ok(Waited::Closed),
        Err(IdleExit::Cancelled) => Err(SetupFailure::Cancelled),
    }
}

/// Runs `work` while the link is down, answering Close immediately and refusing proposals.
/// Arranging is only recorded here; the next connection opens by sending it.
async fn while_idle<T>(
    work: impl Future<Output = T>,
    cancel: &RevocationSignal,
    events: &UnboundedSender<LinkEvent>,
    commands: &mut UnboundedReceiver<LinkCommand>,
    arranging: &mut bool,
) -> Result<T, IdleExit> {
    tokio::pin!(work);
    let mut guard = ticker(LINK_GUARD_INTERVAL);
    loop {
        tokio::select! {
            biased;
            value = &mut work => return Ok(value),
            command = commands.recv() => match command {
                // A dropped command channel means the controller is gone: close the link cleanly.
                Some(LinkCommand::Close) | None => return Err(IdleExit::Closed),
                Some(LinkCommand::Arranging(value)) => *arranging = value,
                // No link carries this proposal; the controller must apply again once connected.
                Some(LinkCommand::Propose { .. }) => {
                    if events
                        .send(LinkEvent::SyncRejected {
                            reason: LinkRejectReason::Cancelled,
                            sending: true,
                        })
                        .is_err()
                    {
                        return Err(IdleExit::Cancelled);
                    }
                }
            },
            _ = guard.tick() => if cancel.is_revoked() {
                return Err(IdleExit::Cancelled);
            },
        }
    }
}

fn emit(events: &UnboundedSender<LinkEvent>, event: LinkEvent) -> Result<(), SetupFailure> {
    events.send(event).map_err(|_| SetupFailure::Cancelled)
}

const fn attempt_reason(error: SetupFailure) -> LinkDisconnect {
    match error {
        SetupFailure::Handshake => LinkDisconnect::Handshake,
        _ => LinkDisconnect::Attempt,
    }
}

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    // A stalled persist callback must not be followed by a burst of catch-up ticks.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Simultaneous proposals resolve to the lower device id, so both sides pick the same winner.
const fn local_proposal_wins(local: DeviceId, peer: DeviceId) -> bool {
    let (local, peer) = (local.0, peer.0);
    let mut index = 0;
    while index < local.len() {
        if local[index] != peer[index] {
            return local[index] < peer[index];
        }
        index += 1;
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkKind {
    Heartbeat,
    Topology,
    Proposal,
    Ack,
    Reject,
    Complete,
    Bye,
    Committed,
    /// One byte, 0 or 1: whether the sender's user is arranging displays right now.
    Arranging,
}

impl LinkKind {
    const fn wire(self) -> u8 {
        match self {
            Self::Heartbeat => 1,
            Self::Topology => 2,
            Self::Proposal => 3,
            Self::Ack => 4,
            Self::Reject => 5,
            Self::Complete => 6,
            Self::Bye => 7,
            Self::Committed => 8,
            Self::Arranging => 9,
        }
    }

    const fn from_wire(value: u8) -> Result<Self, LinkDisconnect> {
        match value {
            1 => Ok(Self::Heartbeat),
            2 => Ok(Self::Topology),
            3 => Ok(Self::Proposal),
            4 => Ok(Self::Ack),
            5 => Ok(Self::Reject),
            6 => Ok(Self::Complete),
            7 => Ok(Self::Bye),
            8 => Ok(Self::Committed),
            9 => Ok(Self::Arranging),
            _ => Err(LinkDisconnect::Transport),
        }
    }
}

const fn arranging_payload(arranging: bool) -> u8 {
    if arranging { 1 } else { 0 }
}

const fn reject_wire(reason: LinkRejectReason) -> u8 {
    match reason {
        LinkRejectReason::Invalid => 1,
        LinkRejectReason::InspectionChanged => 2,
        LinkRejectReason::SaveFailed => 3,
        LinkRejectReason::Busy => 4,
        LinkRejectReason::Cancelled => 5,
    }
}

const fn reject_from_wire(value: u8) -> Result<LinkRejectReason, LinkDisconnect> {
    match value {
        1 => Ok(LinkRejectReason::Invalid),
        2 => Ok(LinkRejectReason::InspectionChanged),
        3 => Ok(LinkRejectReason::SaveFailed),
        4 => Ok(LinkRejectReason::Busy),
        5 => Ok(LinkRejectReason::Cancelled),
        _ => Err(LinkDisconnect::Transport),
    }
}

struct LinkFrame {
    kind: LinkKind,
    reason: Option<LinkRejectReason>,
    epoch: SessionEpoch,
    sequence: u64,
    digest: [u8; 32],
    payload: Vec<u8>,
}

impl LinkFrame {
    fn empty(kind: LinkKind, epoch: SessionEpoch) -> Self {
        Self {
            kind,
            reason: None,
            epoch,
            sequence: 0,
            digest: [0; 32],
            payload: Vec::new(),
        }
    }

    fn with_payload(kind: LinkKind, epoch: SessionEpoch, payload: Vec<u8>) -> Self {
        Self {
            kind,
            reason: None,
            epoch,
            sequence: 0,
            digest: digest(&payload),
            payload,
        }
    }

    fn with_digest(kind: LinkKind, epoch: SessionEpoch, digest: [u8; 32]) -> Self {
        Self {
            kind,
            reason: None,
            epoch,
            sequence: 0,
            digest,
            payload: Vec::new(),
        }
    }

    fn rejection(epoch: SessionEpoch, digest: [u8; 32], reason: LinkRejectReason) -> Self {
        Self {
            kind: LinkKind::Reject,
            reason: Some(reason),
            epoch,
            sequence: 0,
            digest,
            payload: Vec::new(),
        }
    }
}

fn encode_link_frame(frame: &LinkFrame) -> Result<Vec<u8>, LinkDisconnect> {
    validate_link_frame(frame)?;
    let payload_len = u32::try_from(frame.payload.len()).map_err(|_| LinkDisconnect::Transport)?;
    let mut bytes = Vec::with_capacity(LINK_HEADER_LEN + frame.payload.len());
    bytes.extend_from_slice(&LINK_MAGIC);
    bytes.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    bytes.push(LINK_FORMAT_VERSION);
    bytes.push(LINK_PURPOSE);
    bytes.push(frame.kind.wire());
    bytes.push(frame.reason.map_or(0, reject_wire));
    bytes.extend_from_slice(&[0; 2]);
    bytes.extend_from_slice(&frame.epoch.get().to_be_bytes());
    bytes.extend_from_slice(&frame.sequence.to_be_bytes());
    bytes.extend_from_slice(&payload_len.to_be_bytes());
    bytes.extend_from_slice(&frame.digest);
    bytes.extend_from_slice(&frame.payload);
    Ok(bytes)
}

fn decode_link_frame(
    header: &[u8; LINK_HEADER_LEN],
    payload: Vec<u8>,
) -> Result<LinkFrame, LinkDisconnect> {
    if header[..4] != LINK_MAGIC
        || u16::from_be_bytes([header[4], header[5]]) != PROTOCOL_VERSION
        || header[6] != LINK_FORMAT_VERSION
        || header[7] != LINK_PURPOSE
        || header[10..12].iter().any(|byte| *byte != 0)
    {
        return Err(LinkDisconnect::Transport);
    }
    let kind = LinkKind::from_wire(header[8])?;
    let reason = match header[9] {
        0 => None,
        value => Some(reject_from_wire(value)?),
    };
    let epoch = SessionEpoch::new(u64::from_be_bytes(
        header[12..20]
            .try_into()
            .map_err(|_| LinkDisconnect::Transport)?,
    ))
    .map_err(|_| LinkDisconnect::Transport)?;
    let sequence = u64::from_be_bytes(
        header[20..28]
            .try_into()
            .map_err(|_| LinkDisconnect::Transport)?,
    );
    let payload_len = usize::try_from(u32::from_be_bytes(
        header[28..32]
            .try_into()
            .map_err(|_| LinkDisconnect::Transport)?,
    ))
    .map_err(|_| LinkDisconnect::Transport)?;
    if payload_len != payload.len() {
        return Err(LinkDisconnect::Transport);
    }
    let mut digest = [0; 32];
    digest.copy_from_slice(&header[32..64]);
    let frame = LinkFrame {
        kind,
        reason,
        epoch,
        sequence,
        digest,
        payload,
    };
    validate_link_frame(&frame)?;
    Ok(frame)
}

fn validate_link_frame(frame: &LinkFrame) -> Result<(), LinkDisconnect> {
    if frame.payload.len() > MAX_LAYOUT_PAYLOAD_BYTES {
        return Err(LinkDisconnect::Transport);
    }
    let valid = match frame.kind {
        LinkKind::Heartbeat | LinkKind::Bye => {
            frame.reason.is_none() && frame.payload.is_empty() && frame.digest == [0; 32]
        }
        LinkKind::Topology | LinkKind::Proposal => {
            frame.reason.is_none()
                && !frame.payload.is_empty()
                && frame.digest == digest(&frame.payload)
        }
        LinkKind::Ack | LinkKind::Complete | LinkKind::Committed => {
            frame.reason.is_none() && frame.payload.is_empty()
        }
        LinkKind::Reject => frame.reason.is_some() && frame.payload.is_empty(),
        LinkKind::Arranging => {
            frame.reason.is_none()
                && matches!(frame.payload.as_slice(), [0 | 1])
                && frame.digest == digest(&frame.payload)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(LinkDisconnect::Transport)
    }
}

fn encode_topology(
    topology: &DisplayTopology,
    epoch: SessionEpoch,
) -> Result<Vec<u8>, LinkDisconnect> {
    let mut bytes = Vec::new();
    // Reuse the protocol's bounded display encoding; the LKLC header carries the link's ordering.
    Frame::new(
        epoch,
        TOPOLOGY_FRAME_SEQUENCE,
        Message::DisplayTopology(topology.clone()),
    )
    .encode_into(&mut bytes)
    .map_err(|_| LinkDisconnect::Transport)?;
    Ok(bytes)
}

fn decode_topology(payload: &[u8], epoch: SessionEpoch) -> Result<DisplayTopology, LinkDisconnect> {
    let frame = decode(payload).map_err(|_| LinkDisconnect::Transport)?;
    if frame.epoch != epoch || frame.sequence != TOPOLOGY_FRAME_SEQUENCE {
        return Err(LinkDisconnect::Transport);
    }
    match frame.message {
        Message::DisplayTopology(topology) => Ok(topology),
        _ => Err(LinkDisconnect::Transport),
    }
}

fn digest(payload: &[u8]) -> [u8; 32] {
    Sha256::digest(payload).into()
}

/// An incremental reader for one ordered link stream.
///
/// Cancel safe: every await is exactly one `RecvStream::read` and the state advances synchronously
/// after it resolves, so a cancelled read cannot lose bytes that already left the stream.
struct LinkReader {
    header: [u8; LINK_HEADER_LEN],
    filled: usize,
    payload: Vec<u8>,
    payload_filled: usize,
    payload_len: Option<usize>,
    terminal: bool,
}

impl LinkReader {
    fn new() -> Self {
        Self {
            header: [0; LINK_HEADER_LEN],
            filled: 0,
            payload: Vec::new(),
            payload_filled: 0,
            payload_len: None,
            terminal: false,
        }
    }

    async fn next_frame(
        &mut self,
        recv: &mut quinn::RecvStream,
    ) -> Result<LinkFrame, LinkDisconnect> {
        if self.terminal {
            return Err(LinkDisconnect::Transport);
        }
        loop {
            if let Some(frame) = self.take_complete()? {
                return Ok(frame);
            }
            let read = {
                let target = if self.payload_len.is_none() {
                    &mut self.header[self.filled..]
                } else {
                    &mut self.payload[self.payload_filled..]
                };
                recv.read(target).await
            };
            match read {
                Ok(Some(0)) | Ok(None) => {
                    self.terminal = true;
                    // A clean end between frames is the peer leaving; a partial frame is a fault.
                    return Err(if self.filled == 0 && self.payload_len.is_none() {
                        LinkDisconnect::PeerClosed
                    } else {
                        LinkDisconnect::Transport
                    });
                }
                Ok(Some(count)) => {
                    if self.payload_len.is_none() {
                        self.filled += count;
                    } else {
                        self.payload_filled += count;
                    }
                }
                Err(_) => {
                    self.terminal = true;
                    return Err(LinkDisconnect::Transport);
                }
            }
        }
    }

    fn take_complete(&mut self) -> Result<Option<LinkFrame>, LinkDisconnect> {
        if self.payload_len.is_none() {
            if self.filled != LINK_HEADER_LEN {
                return Ok(None);
            }
            let declared = usize::try_from(u32::from_be_bytes([
                self.header[28],
                self.header[29],
                self.header[30],
                self.header[31],
            ]))
            .map_err(|_| LinkDisconnect::Transport)?;
            if declared > MAX_LAYOUT_PAYLOAD_BYTES {
                self.terminal = true;
                return Err(LinkDisconnect::Transport);
            }
            self.payload = vec![0; declared];
            self.payload_filled = 0;
            self.payload_len = Some(declared);
        }
        let Some(declared) = self.payload_len else {
            return Ok(None);
        };
        if self.payload_filled != declared {
            return Ok(None);
        }
        let header = self.header;
        let payload = std::mem::take(&mut self.payload);
        self.filled = 0;
        self.payload_filled = 0;
        self.payload_len = None;
        decode_link_frame(&header, payload)
            .map(Some)
            .inspect_err(|_| self.terminal = true)
    }
}

/// Neither computer commits before the other has staged: Proposal, Ack, Complete, Committed.
/// The receiver commits on Complete; the sender commits only on the Committed that answers it.
enum SyncState {
    Idle,
    Sending {
        bytes: Vec<u8>,
        digest: [u8; 32],
        deadline: Instant,
    },
    /// The sender has staged and told the peer to commit; it awaits the peer's Committed.
    Confirming {
        bytes: Vec<u8>,
        digest: [u8; 32],
        deadline: Instant,
    },
    Receiving {
        bytes: Vec<u8>,
        digest: [u8; 32],
        deadline: Instant,
    },
}

enum Wake {
    Peer(Result<LinkFrame, LinkDisconnect>),
    Command(Option<LinkCommand>),
    Heartbeat,
    Topology,
    Guard,
}

/// One established connection: framing state, the shared peer facts, and the sync transaction.
struct Link<'a> {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    epoch: SessionEpoch,
    next_sequence: u64,
    expected_sequence: u64,
    reader: LinkReader,
    pending: VecDeque<LinkFrame>,
    last_peer: Instant,
    sync: SyncState,
    /// Digest of a proposal dropped after losing the tie-break; its reject unwinds nothing.
    abandoned: Option<[u8; 32]>,
    /// Digest this computer committed as receiver; a later reject for it means the pair disagrees.
    committed: Option<[u8; 32]>,
    inspection: InspectedPeer,
    /// Owned by `run_link` so the state outlives this connection and opens the next one.
    arranging: &'a mut bool,
    persist: &'a LinkPersist,
    events: &'a UnboundedSender<LinkEvent>,
    cancel: &'a RevocationSignal,
}

impl<'a> Link<'a> {
    fn new(
        session: NegotiatedSession,
        inspection: InspectedPeer,
        arranging: &'a mut bool,
        persist: &'a LinkPersist,
        events: &'a UnboundedSender<LinkEvent>,
        cancel: &'a RevocationSignal,
    ) -> Self {
        let epoch = session.initial_epoch;
        let connection = session.connection.clone();
        let ControlStreams { send, recv, .. } = session.control_streams;
        Self {
            connection,
            send,
            recv,
            epoch,
            next_sequence: LINK_START_SEQUENCE,
            expected_sequence: LINK_START_SEQUENCE,
            reader: LinkReader::new(),
            pending: VecDeque::new(),
            last_peer: Instant::now(),
            sync: SyncState::Idle,
            abandoned: None,
            committed: None,
            inspection,
            arranging,
            persist,
            events,
            cancel,
        }
    }

    async fn run<C: LinkConnector>(
        &mut self,
        commands: &mut UnboundedReceiver<LinkCommand>,
        connector: &C,
    ) -> LinkExit {
        match self.exchange(commands, connector).await {
            Ok(exit) | Err(exit) => exit,
        }
    }

    async fn exchange<C: LinkConnector>(
        &mut self,
        commands: &mut UnboundedReceiver<LinkCommand>,
        connector: &C,
    ) -> Result<LinkExit, LinkExit> {
        let mut heartbeat = ticker(LINK_HEARTBEAT_INTERVAL);
        let mut topology = ticker(LINK_TOPOLOGY_POLL);
        let mut guard = ticker(LINK_GUARD_INTERVAL);
        // The peer decides nothing from silence, so every connection opens by naming this state.
        self.publish_arranging().await?;
        loop {
            while let Some(frame) = self.pending.pop_front() {
                if let Some(exit) = self.handle_frame(frame).await? {
                    return Ok(exit);
                }
            }
            // Peer frames are served first. A peer that floods them cannot outlast cancellation:
            // the endpoint watch closes the socket, which ends the read with a transport failure.
            let wake = {
                let Self { reader, recv, .. } = &mut *self;
                tokio::select! {
                    biased;
                    frame = reader.next_frame(recv) => Wake::Peer(frame),
                    command = commands.recv() => Wake::Command(command),
                    _ = heartbeat.tick() => Wake::Heartbeat,
                    _ = topology.tick() => Wake::Topology,
                    _ = guard.tick() => Wake::Guard,
                }
            };
            match wake {
                Wake::Peer(Ok(frame)) => {
                    self.last_peer = Instant::now();
                    self.pending.push_back(frame);
                }
                Wake::Peer(Err(reason)) => {
                    self.close_transaction(LinkRejectReason::Cancelled)?;
                    return Ok(LinkExit::Disconnected(reason));
                }
                Wake::Command(Some(LinkCommand::Propose { bytes })) => self.propose(bytes).await?,
                Wake::Command(Some(LinkCommand::Arranging(value))) => {
                    if *self.arranging != value {
                        *self.arranging = value;
                        self.publish_arranging().await?;
                    }
                }
                // A dropped command channel means the controller is gone: close the link cleanly.
                Wake::Command(Some(LinkCommand::Close) | None) => {
                    self.close_transaction(LinkRejectReason::Cancelled)?;
                    self.farewell().await;
                    return Ok(LinkExit::Closed);
                }
                Wake::Heartbeat => {
                    self.write(LinkFrame::empty(LinkKind::Heartbeat, self.epoch))
                        .await?;
                }
                Wake::Topology => self.publish_local_topology(connector).await?,
                Wake::Guard => self.guard()?,
            }
        }
    }

    fn guard(&mut self) -> Result<(), LinkExit> {
        if self.cancel.is_revoked() {
            self.close_transaction(LinkRejectReason::Cancelled)?;
            return Err(LinkExit::Cancelled);
        }
        if self.last_peer.elapsed() >= LINK_SILENCE_DEADLINE {
            self.close_transaction(LinkRejectReason::Cancelled)?;
            return Err(LinkExit::Disconnected(LinkDisconnect::Silence));
        }
        let expired = match &self.sync {
            SyncState::Idle => false,
            SyncState::Sending { deadline, .. }
            | SyncState::Confirming { deadline, .. }
            | SyncState::Receiving { deadline, .. } => Instant::now() >= *deadline,
        };
        if expired {
            self.close_transaction(LinkRejectReason::Cancelled)?;
        }
        Ok(())
    }

    async fn propose(&mut self, bytes: Vec<u8>) -> Result<(), LinkExit> {
        if bytes.is_empty() || bytes.len() > MAX_LAYOUT_PAYLOAD_BYTES {
            return self.emit(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Invalid,
                sending: true,
            });
        }
        if !matches!(self.sync, SyncState::Idle) {
            return self.emit(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Busy,
                sending: true,
            });
        }
        let digest = digest(&bytes);
        self.emit(LinkEvent::SyncStarted { sending: true })?;
        self.write(LinkFrame::with_payload(
            LinkKind::Proposal,
            self.epoch,
            bytes.clone(),
        ))
        .await?;
        self.sync = SyncState::Sending {
            bytes,
            digest,
            deadline: Instant::now() + LINK_ACK_DEADLINE,
        };
        Ok(())
    }

    async fn handle_frame(&mut self, frame: LinkFrame) -> Result<Option<LinkExit>, LinkExit> {
        if frame.epoch != self.epoch || frame.sequence != self.expected_sequence {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        }
        self.expected_sequence = self
            .expected_sequence
            .checked_add(1)
            .ok_or(LinkExit::Disconnected(LinkDisconnect::Transport))?;
        match frame.kind {
            LinkKind::Heartbeat => {}
            LinkKind::Topology => self.accept_peer_topology(&frame.payload)?,
            LinkKind::Proposal => self.accept_proposal(frame).await?,
            LinkKind::Ack => self.accept_acknowledgement(&frame).await?,
            LinkKind::Complete => self.accept_completion(&frame).await?,
            LinkKind::Committed => self.accept_confirmation(&frame).await?,
            LinkKind::Reject => self.accept_rejection(&frame)?,
            // Validation bounds the payload to one 0-or-1 byte.
            LinkKind::Arranging => self.emit(LinkEvent::PeerArranging {
                arranging: frame.payload[0] == 1,
            })?,
            LinkKind::Bye => {
                self.close_transaction(LinkRejectReason::Cancelled)?;
                return Ok(Some(LinkExit::Disconnected(LinkDisconnect::PeerClosed)));
            }
        }
        Ok(None)
    }

    fn accept_peer_topology(&mut self, payload: &[u8]) -> Result<(), LinkExit> {
        let topology = decode_topology(payload, self.epoch).map_err(LinkExit::Disconnected)?;
        if topology.same_geometry(&self.inspection.peer_displays) {
            return Ok(());
        }
        self.inspection.peer_displays = topology;
        self.emit(LinkEvent::TopologyChanged {
            inspection: self.inspection.clone(),
        })
    }

    async fn accept_proposal(&mut self, frame: LinkFrame) -> Result<(), LinkExit> {
        match self.sync {
            // One transaction at a time; the staged file of the transaction in flight is untouched.
            SyncState::Receiving { .. } | SyncState::Confirming { .. } => {
                return self.reject(frame.digest, LinkRejectReason::Busy).await;
            }
            SyncState::Sending { digest, .. } => {
                if local_proposal_wins(self.inspection.local_device, self.inspection.peer_device) {
                    return self.reject(frame.digest, LinkRejectReason::Busy).await;
                }
                self.sync = SyncState::Idle;
                self.abandoned = Some(digest);
                self.emit(LinkEvent::SyncRejected {
                    reason: LinkRejectReason::Busy,
                    sending: true,
                })?;
            }
            SyncState::Idle => {}
        }
        self.emit(LinkEvent::SyncStarted { sending: false })?;
        let staged = run_persist(
            Arc::clone(&self.persist.stage),
            self.inspection.clone(),
            frame.payload.clone(),
        )
        .await;
        match staged {
            Ok(()) => {
                self.write(LinkFrame::with_digest(
                    LinkKind::Ack,
                    self.epoch,
                    frame.digest,
                ))
                .await?;
                self.sync = SyncState::Receiving {
                    bytes: frame.payload,
                    digest: frame.digest,
                    deadline: Instant::now() + LINK_ACK_DEADLINE,
                };
                Ok(())
            }
            Err(reason) => {
                (self.persist.discard)();
                self.reject(frame.digest, reason).await?;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: false,
                })
            }
        }
    }

    async fn accept_acknowledgement(&mut self, frame: &LinkFrame) -> Result<(), LinkExit> {
        let SyncState::Sending { bytes, digest, .. } = &self.sync else {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        };
        if frame.digest != *digest {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        }
        let bytes = bytes.clone();
        let digest = *digest;
        self.sync = SyncState::Idle;
        // The sender only stages here: it commits on the Committed that answers this Complete.
        match run_persist(
            Arc::clone(&self.persist.stage),
            self.inspection.clone(),
            bytes.clone(),
        )
        .await
        {
            Ok(()) => {
                self.write(LinkFrame::with_digest(
                    LinkKind::Complete,
                    self.epoch,
                    digest,
                ))
                .await?;
                self.sync = SyncState::Confirming {
                    bytes,
                    digest,
                    deadline: Instant::now() + LINK_ACK_DEADLINE,
                };
                Ok(())
            }
            Err(reason) => {
                (self.persist.discard)();
                self.reject(digest, reason).await?;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: true,
                })
            }
        }
    }

    async fn accept_completion(&mut self, frame: &LinkFrame) -> Result<(), LinkExit> {
        let SyncState::Receiving { bytes, digest, .. } = &self.sync else {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        };
        if frame.digest != *digest {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        }
        let bytes = bytes.clone();
        let digest = *digest;
        self.sync = SyncState::Idle;
        match run_persist(
            Arc::clone(&self.persist.commit),
            self.inspection.clone(),
            bytes.clone(),
        )
        .await
        {
            Ok(()) => {
                self.committed = Some(digest);
                self.write(LinkFrame::with_digest(
                    LinkKind::Committed,
                    self.epoch,
                    digest,
                ))
                .await?;
                self.emit(LinkEvent::SyncCompleted {
                    inspection: self.inspection.clone(),
                    bytes,
                    sending: false,
                })
            }
            Err(reason) => {
                (self.persist.discard)();
                self.reject(digest, reason).await?;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: false,
                })
            }
        }
    }

    /// The peer has committed, so the sender's own staged file can be renamed into place.
    async fn accept_confirmation(&mut self, frame: &LinkFrame) -> Result<(), LinkExit> {
        let SyncState::Confirming { bytes, digest, .. } = &self.sync else {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        };
        if frame.digest != *digest {
            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
        }
        let bytes = bytes.clone();
        let digest = *digest;
        self.sync = SyncState::Idle;
        match run_persist(
            Arc::clone(&self.persist.commit),
            self.inspection.clone(),
            bytes.clone(),
        )
        .await
        {
            Ok(()) => self.emit(LinkEvent::SyncCompleted {
                inspection: self.inspection.clone(),
                bytes,
                sending: true,
            }),
            // The peer applied a layout this computer could not, so it must be told to unwind.
            Err(reason) => {
                (self.persist.discard)();
                self.reject(digest, reason).await?;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: true,
                })
            }
        }
    }

    fn accept_rejection(&mut self, frame: &LinkFrame) -> Result<(), LinkExit> {
        let reason = frame
            .reason
            .ok_or(LinkExit::Disconnected(LinkDisconnect::Transport))?;
        if self.abandoned == Some(frame.digest) {
            self.abandoned = None;
            return Ok(());
        }
        match &self.sync {
            SyncState::Sending { digest, .. } if *digest == frame.digest => {
                self.sync = SyncState::Idle;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: true,
                })
            }
            SyncState::Confirming { digest, .. } if *digest == frame.digest => {
                self.sync = SyncState::Idle;
                (self.persist.discard)();
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: true,
                })
            }
            SyncState::Receiving { digest, .. } if *digest == frame.digest => {
                self.sync = SyncState::Idle;
                (self.persist.discard)();
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: false,
                })
            }
            // This computer committed what the peer then could not, so it must stop reporting
            // the layout as applied on both.
            _ if self.committed == Some(frame.digest) => {
                self.committed = None;
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: false,
                })
            }
            // A reject for a transaction already unwound locally carries no further state.
            _ => Ok(()),
        }
    }

    async fn publish_arranging(&mut self) -> Result<(), LinkExit> {
        let payload = vec![arranging_payload(*self.arranging)];
        self.write(LinkFrame::with_payload(
            LinkKind::Arranging,
            self.epoch,
            payload,
        ))
        .await
    }

    async fn reject(&mut self, digest: [u8; 32], reason: LinkRejectReason) -> Result<(), LinkExit> {
        self.write(LinkFrame::rejection(self.epoch, digest, reason))
            .await
    }

    async fn publish_local_topology<C: LinkConnector>(
        &mut self,
        connector: &C,
    ) -> Result<(), LinkExit> {
        let Some(current) = connector.local_displays(&self.inspection) else {
            return Ok(());
        };
        if current.same_geometry(&self.inspection.local_displays) {
            return Ok(());
        }
        let payload = encode_topology(&current, self.epoch).map_err(LinkExit::Disconnected)?;
        self.inspection.local_displays = current;
        self.write(LinkFrame::with_payload(
            LinkKind::Topology,
            self.epoch,
            payload,
        ))
        .await?;
        self.emit(LinkEvent::TopologyChanged {
            inspection: self.inspection.clone(),
        })
    }

    /// Unwinds an unfinished transaction. Any staged file is always discarded.
    fn close_transaction(&mut self, reason: LinkRejectReason) -> Result<(), LinkExit> {
        self.abandoned = None;
        match std::mem::replace(&mut self.sync, SyncState::Idle) {
            SyncState::Idle => Ok(()),
            SyncState::Sending { .. } => self.emit(LinkEvent::SyncRejected {
                reason,
                sending: true,
            }),
            SyncState::Confirming { .. } => {
                (self.persist.discard)();
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: true,
                })
            }
            SyncState::Receiving { .. } => {
                (self.persist.discard)();
                self.emit(LinkEvent::SyncRejected {
                    reason,
                    sending: false,
                })
            }
        }
    }

    async fn farewell(&mut self) {
        if self
            .write(LinkFrame::empty(LinkKind::Bye, self.epoch))
            .await
            .is_ok()
        {
            let _ = self.send.finish();
            // Give the peer a bounded chance to acknowledge the goodbye before the socket closes.
            let _ = tokio::time::timeout(LINK_HEARTBEAT_INTERVAL, self.send.stopped()).await;
        }
        self.connection
            .close(LINK_CLOSE_CODE.into(), LINK_CLOSE_REASON);
    }

    async fn write(&mut self, mut frame: LinkFrame) -> Result<(), LinkExit> {
        frame.sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(LinkExit::Disconnected(LinkDisconnect::Transport))?;
        let bytes = encode_link_frame(&frame).map_err(LinkExit::Disconnected)?;
        let Self {
            send,
            recv,
            reader,
            pending,
            last_peer,
            cancel,
            ..
        } = self;
        let started = Instant::now();
        let mut write = std::pin::pin!(send.write_all(&bytes));
        let mut guard = ticker(LINK_GUARD_INTERVAL);
        loop {
            tokio::select! {
                biased;
                result = &mut write => {
                    return result.map_err(|_| LinkExit::Disconnected(LinkDisconnect::Transport));
                }
                // One payload can exceed the peer's receive window, so reads must continue or two
                // large proposals sent at once would block both writers forever.
                peer = reader.next_frame(recv) => match peer {
                    Ok(peer) => {
                        *last_peer = Instant::now();
                        if pending.len() >= MAX_PENDING_FRAMES {
                            return Err(LinkExit::Disconnected(LinkDisconnect::Transport));
                        }
                        pending.push_back(peer);
                    }
                    Err(reason) => return Err(LinkExit::Disconnected(reason)),
                },
                _ = guard.tick() => {
                    if cancel.is_revoked() {
                        return Err(LinkExit::Cancelled);
                    }
                    if started.elapsed() >= LINK_SILENCE_DEADLINE {
                        return Err(LinkExit::Disconnected(LinkDisconnect::Silence));
                    }
                }
            }
        }
    }

    fn emit(&self, event: LinkEvent) -> Result<(), LinkExit> {
        self.events.send(event).map_err(|_| LinkExit::Cancelled)
    }
}

/// Runs one persistence callback on a dedicated OS thread.
///
/// Disk work must never run on the link's async runtime: a blocking rename or fsync there would
/// stall the reads and heartbeats that keep the link alive. The link still waits for the result,
/// so a callback slower than the silence deadline drops the connection and reconnects.
async fn run_persist(
    call: LinkPersistStep,
    inspection: InspectedPeer,
    bytes: Vec<u8>,
) -> Result<(), LinkRejectReason> {
    let (done, wait) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("monhop-link-persist".into())
        .spawn(move || {
            let _ = done.send(call(&inspection, &bytes));
        });
    if spawned.is_err() {
        return Err(LinkRejectReason::SaveFailed);
    }
    wait.await.unwrap_or(Err(LinkRejectReason::SaveFailed))
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use monhop_core::{DisplayId, Platform, Point};
    use monhop_protocol::{Capabilities, DisplayDescription};
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
        session_handshake::{HandshakeConfig, device_id_from_fingerprint, negotiate},
        session_setup::handshake_failure,
    };

    const TEST_DEADLINE: Duration = Duration::from_secs(30);

    struct Loopback {
        endpoint: quinn::Endpoint,
        peer_address: Option<SocketAddr>,
        identity: DeviceIdentity,
        peer: VerifiedPeer,
        local_platform: Platform,
        peer_platform: Platform,
        source: DeviceId,
        displays: Arc<Mutex<DisplayTopology>>,
        connection: Arc<Mutex<Option<quinn::Connection>>>,
    }

    impl LinkConnector for Loopback {
        fn dials(&self) -> bool {
            self.peer_address.is_some()
        }

        async fn next_connection(
            &self,
            cancel: &RevocationSignal,
        ) -> Result<quinn::Connection, SetupFailure> {
            let connection = match self.peer_address {
                Some(address) => self
                    .endpoint
                    .connect(address, LOCAL_TLS_SERVER_NAME)
                    .map_err(|_| SetupFailure::Connection)?
                    .await
                    .map_err(|_| SetupFailure::Connection)?,
                None => self
                    .endpoint
                    .accept()
                    .await
                    .ok_or(SetupFailure::Connection)?
                    .accept()
                    .map_err(|_| SetupFailure::Connection)?
                    .await
                    .map_err(|_| SetupFailure::Connection)?,
            };
            if cancel.is_revoked() {
                return Err(SetupFailure::Cancelled);
            }
            *self.connection.lock().unwrap() = Some(connection.clone());
            Ok(connection)
        }

        async fn negotiate(
            &self,
            connection: quinn::Connection,
        ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
            let local_displays = self.displays.lock().unwrap().clone();
            let capabilities =
                Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                    .map_err(|_| SetupFailure::Handshake)?;
            let config = HandshakeConfig::new(
                &self.identity,
                &self.peer,
                self.local_platform,
                self.peer_platform,
                capabilities,
                capabilities,
                &local_displays,
                self.source,
                SessionPurpose::Setup,
            )
            .map_err(|_| SetupFailure::Handshake)?;
            let session = negotiate(connection, config)
                .await
                .map_err(handshake_failure)?;
            let inspection = InspectedPeer {
                local_device: device_id_from_fingerprint(self.identity.fingerprint()),
                peer_device: device_id_from_fingerprint(self.peer.fingerprint()),
                source: self.source,
                local_fingerprint: self.identity.fingerprint(),
                peer_fingerprint: self.peer.fingerprint(),
                local_platform: self.local_platform,
                peer_platform: self.peer_platform,
                local_displays,
                peer_displays: session.peer.topology.clone(),
                interface_id: "fixture".into(),
            };
            Ok((session, inspection))
        }

        fn local_displays(&self, _inspection: &InspectedPeer) -> Option<DisplayTopology> {
            Some(self.displays.lock().unwrap().clone())
        }

        async fn close(&self) {
            self.endpoint.close(0_u32.into(), b"fixture complete");
        }
    }

    struct Recorder {
        staged: AtomicUsize,
        committed: AtomicUsize,
        discarded: AtomicUsize,
        /// Place of this side's commit in the pair's shared order; `usize::MAX` until it runs.
        commit_position: AtomicUsize,
        order: Arc<AtomicUsize>,
        stage_result: Mutex<Result<(), LinkRejectReason>>,
        commit_result: Mutex<Result<(), LinkRejectReason>>,
        saved: Mutex<Vec<u8>>,
    }

    impl Recorder {
        fn new(order: &Arc<AtomicUsize>) -> Self {
            Self {
                staged: AtomicUsize::new(0),
                committed: AtomicUsize::new(0),
                discarded: AtomicUsize::new(0),
                commit_position: AtomicUsize::new(usize::MAX),
                order: Arc::clone(order),
                stage_result: Mutex::new(Ok(())),
                commit_result: Mutex::new(Ok(())),
                saved: Mutex::new(Vec::new()),
            }
        }

        fn counts(&self) -> (usize, usize, usize) {
            (
                self.staged.load(Ordering::Acquire),
                self.committed.load(Ordering::Acquire),
                self.discarded.load(Ordering::Acquire),
            )
        }

        fn commit_position(&self) -> usize {
            self.commit_position.load(Ordering::Acquire)
        }
    }

    fn persist_for(recorder: &Arc<Recorder>) -> LinkPersist {
        let stage = Arc::clone(recorder);
        let commit = Arc::clone(recorder);
        let discard = Arc::clone(recorder);
        LinkPersist {
            stage: Arc::new(move |_, bytes| {
                stage.staged.fetch_add(1, Ordering::AcqRel);
                *stage.saved.lock().unwrap() = bytes.to_vec();
                *stage.stage_result.lock().unwrap()
            }),
            commit: Arc::new(move |_, _| {
                commit.commit_position.store(
                    commit.order.fetch_add(1, Ordering::AcqRel),
                    Ordering::Release,
                );
                commit.committed.fetch_add(1, Ordering::AcqRel);
                *commit.commit_result.lock().unwrap()
            }),
            discard: Arc::new(move || {
                discard.discarded.fetch_add(1, Ordering::AcqRel);
            }),
        }
    }

    fn topology(id: u64, width: u32) -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(id),
            name: "fixture".into(),
            native_width: width,
            native_height: 100,
            logical_origin: Point::default(),
            logical_size: Point::new(f64::from(width), 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap()
    }

    fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
        VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .unwrap()
    }

    fn loopback_pair() -> (Loopback, Loopback) {
        let client_identity = DeviceIdentity::generate().unwrap();
        let server_identity = DeviceIdentity::generate().unwrap();
        let server_pin = pin(&server_identity);
        let client_pin = pin(&client_identity);
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let server = quinn::Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_pin).unwrap(),
            loopback,
        )
        .unwrap();
        let mut client = quinn::Endpoint::client(loopback).unwrap();
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_pin).unwrap(),
        );
        let server_address = server.local_addr().unwrap();
        let source = device_id_from_fingerprint(client_identity.fingerprint());
        (
            Loopback {
                endpoint: client,
                peer_address: Some(server_address),
                identity: client_identity,
                peer: server_pin,
                local_platform: Platform::Windows,
                peer_platform: Platform::MacOs,
                source,
                displays: Arc::new(Mutex::new(topology(1, 100))),
                connection: Arc::new(Mutex::new(None)),
            },
            Loopback {
                endpoint: server,
                peer_address: None,
                identity: server_identity,
                peer: client_pin,
                local_platform: Platform::MacOs,
                peer_platform: Platform::Windows,
                source,
                displays: Arc::new(Mutex::new(topology(2, 200))),
                connection: Arc::new(Mutex::new(None)),
            },
        )
    }

    struct Side {
        inbox: UnboundedReceiver<LinkEvent>,
        commands: UnboundedSender<LinkCommand>,
        recorder: Arc<Recorder>,
        device: DeviceId,
        displays: Arc<Mutex<DisplayTopology>>,
        connection: Arc<Mutex<Option<quinn::Connection>>>,
    }

    impl Side {
        fn close(&self) {
            self.commands.send(LinkCommand::Close).unwrap();
        }

        fn propose(&self, bytes: &[u8]) {
            self.commands
                .send(LinkCommand::Propose {
                    bytes: bytes.to_vec(),
                })
                .unwrap();
        }

        fn arranging(&self, value: bool) {
            self.commands.send(LinkCommand::Arranging(value)).unwrap();
        }
    }

    struct Driver {
        dialer: Side,
        listener: Side,
    }

    async fn wait_for(
        inbox: &mut UnboundedReceiver<LinkEvent>,
        matches: impl Fn(&LinkEvent) -> bool,
    ) -> LinkEvent {
        loop {
            let event = tokio::time::timeout(TEST_DEADLINE, inbox.recv())
                .await
                .expect("link event within the test deadline")
                .expect("open link event channel");
            if matches(&event) {
                return event;
            }
        }
    }

    async fn wait_connected(inbox: &mut UnboundedReceiver<LinkEvent>) {
        wait_for(inbox, |event| matches!(event, LinkEvent::Connected { .. })).await;
    }

    async fn with_link<D, Fut>(
        start: (Duration, Duration),
        drive: D,
    ) -> (Result<(), SetupFailure>, Result<(), SetupFailure>)
    where
        D: FnOnce(Driver) -> Fut,
        // The driver owns the event inboxes, so it must outlive both links.
        Fut: Future<Output = Driver>,
    {
        let (client, server) = loopback_pair();
        let cancel = RevocationSignal::default();
        let order = Arc::new(AtomicUsize::new(0));
        let dialer_recorder = Arc::new(Recorder::new(&order));
        let listener_recorder = Arc::new(Recorder::new(&order));
        let dialer_persist = persist_for(&dialer_recorder);
        let listener_persist = persist_for(&listener_recorder);
        let (dialer_events, dialer_inbox) = unbounded_channel();
        let (listener_events, listener_inbox) = unbounded_channel();
        let (dialer_commands, mut dialer_orders) = unbounded_channel();
        let (listener_commands, mut listener_orders) = unbounded_channel();
        let driver = Driver {
            dialer: Side {
                inbox: dialer_inbox,
                commands: dialer_commands,
                recorder: Arc::clone(&dialer_recorder),
                device: device_id_from_fingerprint(client.identity.fingerprint()),
                displays: Arc::clone(&client.displays),
                connection: Arc::clone(&client.connection),
            },
            listener: Side {
                inbox: listener_inbox,
                commands: listener_commands,
                recorder: Arc::clone(&listener_recorder),
                device: device_id_from_fingerprint(server.identity.fingerprint()),
                displays: Arc::clone(&server.displays),
                connection: Arc::clone(&server.connection),
            },
        };
        let (dialer, listener, _driver) = tokio::time::timeout(TEST_DEADLINE, async {
            tokio::join!(
                async {
                    tokio::time::sleep(start.0).await;
                    run_link(
                        &client,
                        &cancel,
                        &dialer_persist,
                        &dialer_events,
                        &mut dialer_orders,
                    )
                    .await
                },
                async {
                    tokio::time::sleep(start.1).await;
                    run_link(
                        &server,
                        &cancel,
                        &listener_persist,
                        &listener_events,
                        &mut listener_orders,
                    )
                    .await
                },
                drive(driver),
            )
        })
        .await
        .expect("bounded setup link test");
        (dialer, listener)
    }

    #[test]
    fn link_frames_bind_version_purpose_kind_epoch_and_digest() {
        let epoch = SessionEpoch::new(9).unwrap();
        let source =
            LinkFrame::with_payload(LinkKind::Proposal, epoch, vec![7; MAX_LAYOUT_PAYLOAD_BYTES]);
        let encoded = encode_link_frame(&source).unwrap();
        let (header, payload) = encoded.split_at(LINK_HEADER_LEN);
        let header: [u8; LINK_HEADER_LEN] = header.try_into().unwrap();
        let decoded = decode_link_frame(&header, payload.to_vec()).unwrap();
        assert_eq!(decoded.kind, LinkKind::Proposal);
        assert_eq!(decoded.epoch, epoch);
        assert_eq!(decoded.digest, source.digest);
        assert_eq!(decoded.payload, source.payload);

        // Version, purpose and digest bytes are all binding.
        for index in [4, 7, 32] {
            let mut invalid = header;
            invalid[index] ^= 1;
            assert_eq!(
                decode_link_frame(&invalid, payload.to_vec()).err(),
                Some(LinkDisconnect::Transport),
                "header byte {index} must be validated"
            );
        }
        for kind in [0_u8, 10] {
            let mut invalid = header;
            invalid[8] = kind;
            assert_eq!(
                decode_link_frame(&invalid, payload.to_vec()).err(),
                Some(LinkDisconnect::Transport),
                "unknown kind {kind} must be refused"
            );
        }
        let mut oversize = header;
        oversize[28..32].copy_from_slice(
            &u32::try_from(MAX_LAYOUT_PAYLOAD_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        assert_eq!(
            decode_link_frame(&oversize, payload.to_vec()).err(),
            Some(LinkDisconnect::Transport)
        );
        let rejection = LinkFrame::rejection(epoch, [3; 32], LinkRejectReason::Busy);
        let encoded = encode_link_frame(&rejection).unwrap();
        let header: [u8; LINK_HEADER_LEN] = encoded[..LINK_HEADER_LEN].try_into().unwrap();
        assert_eq!(
            decode_link_frame(&header, Vec::new()).unwrap().reason,
            Some(LinkRejectReason::Busy)
        );
    }

    #[test]
    fn simultaneous_proposals_pick_the_same_winner_on_both_sides() {
        let lower = DeviceId([1; 16]);
        let higher = DeviceId([2; 16]);
        assert!(local_proposal_wins(lower, higher));
        assert!(!local_proposal_wins(higher, lower));
        assert!(!local_proposal_wins(lower, lower));
    }

    #[tokio::test]
    async fn link_connects_when_the_dialer_starts_first() {
        let (dialer, listener) = with_link(
            (Duration::ZERO, Duration::from_millis(400)),
            |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.close();
                driver.listener.close();
                driver
            },
        )
        .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_connects_when_the_listener_starts_first_and_close_reports_closed() {
        let (dialer, listener) = with_link(
            (Duration::from_millis(400), Duration::ZERO),
            |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.close();
                driver.listener.close();
                wait_for(&mut driver.dialer.inbox, |event| {
                    matches!(event, LinkEvent::Closed)
                })
                .await;
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::Closed)
                })
                .await;
                driver
            },
        )
        .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    async fn proposal_from(dialer_proposes: bool) {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                let (sender, receiver) = if dialer_proposes {
                    (&mut driver.dialer, &mut driver.listener)
                } else {
                    (&mut driver.listener, &mut driver.dialer)
                };
                sender.propose(b"reviewed layout");
                let completed = wait_for(&mut sender.inbox, |event| {
                    matches!(event, LinkEvent::SyncCompleted { .. })
                })
                .await;
                assert!(matches!(
                    completed,
                    LinkEvent::SyncCompleted { sending: true, .. }
                ));
                let applied = wait_for(&mut receiver.inbox, |event| {
                    matches!(event, LinkEvent::SyncCompleted { .. })
                })
                .await;
                let LinkEvent::SyncCompleted { bytes, sending, .. } = applied else {
                    unreachable!("filtered above");
                };
                assert!(!sending);
                assert_eq!(bytes, b"reviewed layout");
                assert_eq!(sender.recorder.counts(), (1, 1, 0));
                assert_eq!(receiver.recorder.counts(), (1, 1, 0));
                assert_eq!(
                    *receiver.recorder.saved.lock().unwrap(),
                    b"reviewed layout".to_vec()
                );
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_applies_a_proposal_from_the_dialer() {
        proposal_from(true).await;
    }

    #[tokio::test]
    async fn link_applies_a_proposal_from_the_listener() {
        proposal_from(false).await;
    }

    /// Waits for the next settled outcome, so a regression fails the assertion instead of hanging.
    async fn settled(inbox: &mut UnboundedReceiver<LinkEvent>) -> LinkEvent {
        wait_for(inbox, |event| {
            matches!(
                event,
                LinkEvent::SyncCompleted { .. } | LinkEvent::SyncRejected { .. }
            )
        })
        .await
    }

    #[tokio::test]
    async fn both_computers_report_a_layout_applied_only_after_both_commits() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"agreed layout");
                assert!(matches!(
                    settled(&mut driver.listener.inbox).await,
                    LinkEvent::SyncCompleted { sending: false, .. }
                ));
                assert!(matches!(
                    settled(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncCompleted { sending: true, .. }
                ));
                // The receiver commits first; the sender commits only on the confirmation.
                assert_eq!(driver.listener.recorder.commit_position(), 0);
                assert_eq!(driver.dialer.recorder.commit_position(), 1);
                assert_eq!(driver.dialer.recorder.counts(), (1, 1, 0));
                assert_eq!(driver.listener.recorder.counts(), (1, 1, 0));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn a_receiver_that_cannot_commit_leaves_the_sender_uncommitted() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                *driver.listener.recorder.commit_result.lock().unwrap() =
                    Err(LinkRejectReason::SaveFailed);
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"layout the receiver cannot save");
                assert!(matches!(
                    settled(&mut driver.listener.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::SaveFailed,
                        sending: false
                    }
                ));
                assert!(matches!(
                    settled(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::SaveFailed,
                        sending: true
                    }
                ));
                // The sender staged, then discarded without ever renaming its own file into place.
                assert_eq!(driver.dialer.recorder.counts(), (1, 0, 1));
                assert_eq!(driver.listener.recorder.counts(), (1, 1, 1));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn a_sender_that_cannot_commit_unwinds_the_receiver_that_already_did() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                *driver.dialer.recorder.commit_result.lock().unwrap() =
                    Err(LinkRejectReason::SaveFailed);
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"layout the sender cannot save");
                assert!(matches!(
                    settled(&mut driver.listener.inbox).await,
                    LinkEvent::SyncCompleted { sending: false, .. }
                ));
                assert!(matches!(
                    settled(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::SaveFailed,
                        sending: true
                    }
                ));
                // The receiver applied it, so it must be told the pair no longer agrees.
                assert!(matches!(
                    settled(&mut driver.listener.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::SaveFailed,
                        sending: false
                    }
                ));
                assert_eq!(driver.dialer.recorder.counts(), (1, 1, 1));
                assert_eq!(driver.listener.recorder.counts(), (1, 1, 0));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    /// Collects one side's sync events up to its completion, reporting whether it was refused.
    async fn sync_outcome(inbox: &mut UnboundedReceiver<LinkEvent>) -> (bool, bool) {
        let mut refused_as_busy = false;
        loop {
            let event = wait_for(inbox, |event| {
                matches!(
                    event,
                    LinkEvent::SyncCompleted { .. } | LinkEvent::SyncRejected { .. }
                )
            })
            .await;
            match event {
                LinkEvent::SyncCompleted { sending, .. } => return (refused_as_busy, sending),
                LinkEvent::SyncRejected {
                    reason: LinkRejectReason::Busy,
                    sending: true,
                } => refused_as_busy = true,
                LinkEvent::SyncRejected { reason, sending } => {
                    panic!("unexpected refusal {reason:?} while sending={sending}")
                }
                _ => unreachable!("filtered above"),
            }
        }
    }

    #[tokio::test]
    async fn link_refuses_one_of_two_simultaneous_proposals_as_busy() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"dialer layout");
                driver.listener.propose(b"listener layout");
                let (dialer_busy, dialer_sent) = sync_outcome(&mut driver.dialer.inbox).await;
                let (listener_busy, listener_sent) = sync_outcome(&mut driver.listener.inbox).await;
                assert!(
                    dialer_busy != listener_busy,
                    "exactly one side must be refused as busy"
                );
                assert!(
                    dialer_sent != listener_sent,
                    "exactly one layout may be applied"
                );
                // The refused side applies the winner's layout on the same link.
                assert_eq!(dialer_busy, !dialer_sent);
                assert_eq!(driver.dialer.recorder.counts(), (1, 1, 0));
                assert_eq!(driver.listener.recorder.counts(), (1, 1, 0));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_rejects_a_proposal_the_receiver_cannot_stage_without_committing() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                *driver.listener.recorder.stage_result.lock().unwrap() =
                    Err(LinkRejectReason::Invalid);
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"unusable layout");
                let refused = wait_for(&mut driver.dialer.inbox, |event| {
                    matches!(event, LinkEvent::SyncRejected { .. })
                })
                .await;
                assert!(matches!(
                    refused,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::Invalid,
                        sending: true
                    }
                ));
                let local = wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::SyncRejected { .. })
                })
                .await;
                assert!(matches!(
                    local,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::Invalid,
                        sending: false
                    }
                ));
                assert_eq!(driver.dialer.recorder.counts(), (0, 0, 0));
                assert_eq!(driver.listener.recorder.counts(), (1, 0, 1));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_reconnects_on_the_same_endpoint_after_a_lost_connection() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver
                    .dialer
                    .connection
                    .lock()
                    .unwrap()
                    .as_ref()
                    .expect("established connection")
                    .close(9_u32.into(), b"fixture drop");
                wait_for(&mut driver.dialer.inbox, |event| {
                    matches!(event, LinkEvent::Disconnected { .. })
                })
                .await;
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::Disconnected { .. })
                })
                .await;
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.propose(b"layout after reconnect");
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::SyncCompleted { sending: false, .. })
                })
                .await;
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_survives_past_the_silence_deadline_on_heartbeats_alone() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                tokio::time::sleep(LINK_SILENCE_DEADLINE + Duration::from_millis(1_500)).await;
                driver.dialer.propose(b"layout after idle");
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::SyncCompleted { sending: false, .. })
                })
                .await;
                // A dropped link would have produced a reconnect before the proposal landed.
                assert_eq!(driver.listener.recorder.counts(), (1, 1, 0));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    /// Waits for the next arranging report, so the state the peer holds is never inferred.
    async fn next_arranging(inbox: &mut UnboundedReceiver<LinkEvent>) -> bool {
        let event = wait_for(inbox, |event| {
            matches!(event, LinkEvent::PeerArranging { .. })
        })
        .await;
        let LinkEvent::PeerArranging { arranging } = event else {
            unreachable!("filtered above");
        };
        arranging
    }

    #[tokio::test]
    async fn arranging_on_one_computer_is_reported_to_the_other_and_withdrawn_again() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                // Each side opens by naming its own state, so neither has to infer idleness.
                assert!(!next_arranging(&mut driver.dialer.inbox).await);
                assert!(!next_arranging(&mut driver.listener.inbox).await);
                driver.dialer.arranging(true);
                assert!(next_arranging(&mut driver.listener.inbox).await);
                driver.dialer.arranging(false);
                assert!(!next_arranging(&mut driver.listener.inbox).await);
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    #[tokio::test]
    async fn link_publishes_a_changed_local_topology_to_the_peer() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                *driver.dialer.displays.lock().unwrap() = topology(1, 300);
                let changed = wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::TopologyChanged { .. })
                })
                .await;
                let LinkEvent::TopologyChanged { inspection } = changed else {
                    unreachable!("filtered above");
                };
                assert!(inspection.peer_displays.same_geometry(&topology(1, 300)));
                assert_eq!(inspection.peer_device, driver.dialer.device);
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }
}
