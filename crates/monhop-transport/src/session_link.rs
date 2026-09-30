//! Standing authenticated setup link between two paired computers.
//!
//! The link carries display topology and layout proposals only. It never starts native input,
//! changes pairing trust, or binds anything but the one selected interface and pinned peer.

use std::{
    cell::RefCell,
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
    session_setup::{
        GroupEndpoint, InspectedPeer, MEMBER_RELEASED_REASON, PairedMember, PreparedEndpoint,
        SetupFailure, is_transient, prepare_endpoint, read_native_displays,
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

pub const MAX_LAYOUT_PAYLOAD_BYTES: usize = 32 * 1024;
/// A record summary is a digest plus small membership bounds, never a full layout.
pub const MAX_SUMMARY_PAYLOAD_BYTES: usize = 1024;

const LINK_MAGIC: [u8; 4] = *b"LKLC";
const LINK_FORMAT_VERSION: u8 = 2;
/// Matches the Setup purpose wire value.
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
    /// Either computer's displays changed, or only their labels at the same geometry.
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
    /// The other computer's record summary, sent on connect and at once on every change.
    PeerSummary {
        bytes: Vec<u8>,
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
    /// This computer's record summary; sent to the peer on connect and at once on change.
    Summary(Vec<u8>),
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
    // The setup link carries no input authority.
    let prepared = prepare_endpoint(
        interface_id,
        monhop_protocol::ControlPermissions::BOTH,
        cancel,
        peer,
    )?;
    let connector = EndpointConnector { prepared };
    run_link(&connector, cancel, &persist, &events, &mut commands).await
}

/// Runs the setup link to `member` on the process's one group endpoint, beside every other
/// member's links and sessions, with `run_setup_link`'s dial rule, attempt bounds and retry
/// cadence. The link only ever closes `member`'s own connection, never the endpoint.
///
/// Per member, Share and Setup are exclusive and the newest connection wins: a connection handed
/// out for `member` by another connect ends this link with `Cancelled` instead of being replaced
/// by its next attempt, and `member` forgotten ends it with `PairingRequired`. The future is not
/// `Send`; it runs on the group endpoint's network runtime.
pub async fn run_setup_link_on(
    group: &GroupEndpoint,
    member: CertificateFingerprint,
    cancel: &RevocationSignal,
    persist: LinkPersist,
    events: UnboundedSender<LinkEvent>,
    mut commands: UnboundedReceiver<LinkCommand>,
) -> Result<(), SetupFailure> {
    let connector = GroupConnector {
        dials: group.dials(member)?,
        group,
        member,
        claim: RefCell::new(None),
    };
    run_link(&connector, cancel, &persist, &events, &mut commands).await
}

/// Supplies one connection at a time. The production implementations own a bound, pinned
/// endpoint, or take one member's connections from the shared group endpoint.
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
        cancel: &RevocationSignal,
    ) -> impl Future<Output = Result<(NegotiatedSession, InspectedPeer), SetupFailure>>;

    /// `None` reports a transient enumeration failure, which must not drop a healthy link. The
    /// read runs off the link's runtime thread, which may also serve the hub.
    fn local_displays(
        &self,
        inspection: &InspectedPeer,
    ) -> impl Future<Output = Option<DisplayTopology>>;

    /// Why the link must end instead of connecting again, once something outside it took its
    /// connection. An endpoint of the link's own is never taken.
    fn taken(&self) -> Option<SetupFailure> {
        None
    }

    /// Closes the connection the link just stopped using before it waits to connect again.
    /// Dropping the link's handles already closes a connection nothing else holds.
    fn release(&self) {}

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
        _cancel: &RevocationSignal,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        self.prepared
            .negotiate_session(connection, SessionPurpose::Setup)
            .await
    }

    async fn local_displays(&self, inspection: &InspectedPeer) -> Option<DisplayTopology> {
        read_native_displays(inspection.local_device).await.ok()
    }

    async fn close(&self) {
        let _ = self.prepared.endpoint().close_and_wait_idle().await;
    }
}

/// Takes one member's connections from the group endpoint. The last one handed out stays this
/// link's claim after the link lets it go, so a newer connection for the member, which stops
/// that claim, ends the link rather than being replaced by the link's next attempt.
struct GroupConnector<'a> {
    group: &'a GroupEndpoint,
    member: CertificateFingerprint,
    dials: bool,
    /// The link cancel and connection of the member's last connection handed to this link.
    claim: RefCell<Option<(RevocationSignal, quinn::Connection)>>,
}

impl LinkConnector for GroupConnector<'_> {
    fn dials(&self) -> bool {
        self.dials
    }

    async fn next_connection(
        &self,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        self.group.setup_connection(self.member, cancel).await
    }

    async fn negotiate(
        &self,
        connection: quinn::Connection,
        cancel: &RevocationSignal,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        // A connection handed out while this one was dialed or awaited must not be replaced.
        if let Some(failure) = self.taken() {
            connection.close(0_u32.into(), MEMBER_RELEASED_REASON);
            return Err(failure);
        }
        let PairedMember {
            session,
            inspection,
            cancel: claim,
        } = self
            .group
            .negotiate_setup(self.member, connection, cancel)
            .await?;
        *self.claim.borrow_mut() = Some((claim, session.connection.clone()));
        Ok((session, inspection))
    }

    async fn local_displays(&self, _inspection: &InspectedPeer) -> Option<DisplayTopology> {
        self.group.local_displays().await.ok()
    }

    fn taken(&self) -> Option<SetupFailure> {
        if let Err(failure) = self.group.check_member(self.member) {
            return Some(failure);
        }
        self.claim
            .borrow()
            .as_ref()
            .filter(|(claim, _)| claim.is_stopping())
            .map(|_| SetupFailure::Cancelled)
    }

    /// The endpoint keeps a handle to the connection, so the link closes it itself.
    fn release(&self) {
        if let Some((_, connection)) = self.claim.borrow().as_ref() {
            connection.close(0_u32.into(), MEMBER_RELEASED_REASON);
        }
    }

    async fn close(&self) {
        self.let_go();
    }
}

impl GroupConnector<'_> {
    /// Ends the member's connection through the endpoint, unless another connection took it.
    fn let_go(&self) {
        if let Some((claim, _)) = self.claim.take()
            && !claim.is_stopping()
        {
            self.group.close_member(self.member);
        }
    }
}

/// Every way out of the link, a cancel or a dropped future included, lets go of the connection.
impl Drop for GroupConnector<'_> {
    fn drop(&mut self) {
        self.let_go();
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
    // Same reconnect survival as `arranging`: a summary set before the link ever connects still
    // reaches the peer on the first and every later connection.
    let mut summary: Option<Vec<u8>> = None;
    loop {
        if cancel.is_revoked() {
            return Err(SetupFailure::Cancelled);
        }
        if let Some(failure) = connector.taken() {
            return Err(failure);
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
            &mut summary,
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
                match wait_between_attempts(cancel, events, commands, &mut arranging, &mut summary)
                    .await?
                {
                    Waited::Continue => continue,
                    Waited::Closed => return closed(connector, events).await,
                }
            }
        };
        if session.purpose() != SessionPurpose::Setup
            || session.control.next_sequence() != LINK_START_SEQUENCE
        {
            connector.release();
            emit(
                events,
                LinkEvent::Disconnected {
                    reason: LinkDisconnect::Handshake,
                },
            )?;
            match wait_between_attempts(cancel, events, commands, &mut arranging, &mut summary)
                .await?
            {
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
        let mut link = Link::new(
            session,
            inspection,
            &mut arranging,
            &mut summary,
            persist,
            events,
            cancel,
        );
        let exit = link.run(commands, connector).await;
        drop(link);
        match exit {
            LinkExit::Closed => return closed(connector, events).await,
            LinkExit::Cancelled => {
                return Err(connector.taken().unwrap_or(SetupFailure::Cancelled));
            }
            LinkExit::Disconnected(reason) => {
                if let Some(failure) = connector.taken() {
                    return Err(failure);
                }
                connector.release();
                log::warn!("setup link: disconnected ({reason:?})");
                emit(events, LinkEvent::Disconnected { reason })?;
                if matches!(
                    wait_between_attempts(cancel, events, commands, &mut arranging, &mut summary)
                        .await?,
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
            connector.negotiate(connection, cancel).await
        })
        .await
        .unwrap_or(Err(SetupFailure::Connection));
    }
    let connection = connector.next_connection(cancel).await?;
    tokio::time::timeout(
        LINK_ATTEMPT_DEADLINE,
        connector.negotiate(connection, cancel),
    )
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
    summary: &mut Option<Vec<u8>>,
) -> Result<Waited, SetupFailure> {
    match while_idle(
        tokio::time::sleep(LINK_DIAL_INTERVAL),
        cancel,
        events,
        commands,
        arranging,
        summary,
    )
    .await
    {
        Ok(()) => Ok(Waited::Continue),
        Err(IdleExit::Closed) => Ok(Waited::Closed),
        Err(IdleExit::Cancelled) => Err(SetupFailure::Cancelled),
    }
}

/// Runs `work` while the link is down, answering Close immediately and refusing proposals.
/// Arranging and the summary are only recorded here; the next connection opens by sending them.
async fn while_idle<T>(
    work: impl Future<Output = T>,
    cancel: &RevocationSignal,
    events: &UnboundedSender<LinkEvent>,
    commands: &mut UnboundedReceiver<LinkCommand>,
    arranging: &mut bool,
    summary: &mut Option<Vec<u8>>,
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
                Some(LinkCommand::Summary(bytes)) => {
                    if is_valid_summary(&bytes) {
                        *summary = Some(bytes);
                    }
                }
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
    /// The sender's opaque record summary; the peer reports it as `LinkEvent::PeerSummary`.
    Summary,
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
            Self::Summary => 10,
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
            10 => Ok(Self::Summary),
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
        LinkKind::Summary => {
            frame.reason.is_none()
                && !frame.payload.is_empty()
                && frame.payload.len() <= MAX_SUMMARY_PAYLOAD_BYTES
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
    if frame.scope != monhop_protocol::FrameScope::Connection
        || frame.epoch != epoch
        || frame.sequence != TOPOLOGY_FRAME_SEQUENCE
    {
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

/// The digest a `Summary` frame carrying this payload must bind, for callers that verify one
/// before proposing it.
pub fn payload_digest(payload: &[u8]) -> [u8; 32] {
    digest(payload)
}

fn is_valid_summary(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() <= MAX_SUMMARY_PAYLOAD_BYTES
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
    /// Digest of a transaction dropped locally (a lost tie-break, a topology change); the one
    /// answer the peer sent before learning so unwinds nothing.
    abandoned: Option<[u8; 32]>,
    /// Digest this computer committed as receiver; a later reject for it means the pair disagrees.
    committed: Option<[u8; 32]>,
    inspection: InspectedPeer,
    /// Owned by `run_link` so the state outlives this connection and opens the next one.
    arranging: &'a mut bool,
    /// Same lifetime as `arranging`: republished at the start of every connection.
    summary: &'a mut Option<Vec<u8>>,
    persist: &'a LinkPersist,
    events: &'a UnboundedSender<LinkEvent>,
    cancel: &'a RevocationSignal,
}

impl<'a> Link<'a> {
    fn new(
        session: NegotiatedSession,
        inspection: InspectedPeer,
        arranging: &'a mut bool,
        summary: &'a mut Option<Vec<u8>>,
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
            summary,
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
        self.publish_summary().await?;
        loop {
            while let Some(frame) = self.pending.pop_front() {
                if let Some(exit) = self.handle_frame(frame).await? {
                    return Ok(exit);
                }
            }
            // Peer frames are served first. A peer that floods them cannot outlast cancellation:
            // an endpoint of the link's own has its watch close the socket, and on the group
            // endpoint's one-thread runtime the read waits for the connection driver once the
            // received data runs out, which lets the guard run.
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
                Wake::Command(Some(LinkCommand::Summary(bytes))) => {
                    if is_valid_summary(&bytes) && self.summary.as_deref() != Some(bytes.as_slice())
                    {
                        *self.summary = Some(bytes);
                        self.publish_summary().await?;
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
                Wake::Guard => self.guard(connector)?,
            }
        }
    }

    fn guard<C: LinkConnector>(&mut self, connector: &C) -> Result<(), LinkExit> {
        if self.cancel.is_revoked() || connector.taken().is_some() {
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
            LinkKind::Topology => self.accept_peer_topology(&frame.payload).await?,
            LinkKind::Proposal => self.accept_proposal(frame).await?,
            LinkKind::Ack => self.accept_acknowledgement(&frame).await?,
            LinkKind::Complete => self.accept_completion(&frame).await?,
            LinkKind::Committed => self.accept_confirmation(&frame).await?,
            LinkKind::Reject => self.accept_rejection(&frame)?,
            // Validation bounds the payload to one 0-or-1 byte.
            LinkKind::Arranging => self.emit(LinkEvent::PeerArranging {
                arranging: frame.payload[0] == 1,
            })?,
            LinkKind::Summary => self.emit(LinkEvent::PeerSummary {
                bytes: frame.payload,
            })?,
            LinkKind::Bye => {
                self.close_transaction(LinkRejectReason::Cancelled)?;
                return Ok(Some(LinkExit::Disconnected(LinkDisconnect::PeerClosed)));
            }
        }
        Ok(None)
    }

    async fn accept_peer_topology(&mut self, payload: &[u8]) -> Result<(), LinkExit> {
        let topology = decode_topology(payload, self.epoch).map_err(LinkExit::Disconnected)?;
        let moved = !topology.same_geometry(&self.inspection.peer_displays);
        if !moved && topology.same_labels(&self.inspection.peer_displays) {
            return Ok(());
        }
        // No record depends on labels, so a rename leaves the transaction in flight alone.
        if moved {
            self.unwind_for_topology().await?;
        }
        self.inspection.peer_displays = topology;
        self.emit(LinkEvent::TopologyChanged {
            inspection: self.inspection.clone(),
        })
    }

    /// A proposal made for the old displays is void on both computers before anyone hears of the
    /// new ones, so the next proposal finds both idle instead of Busy.
    async fn unwind_for_topology(&mut self) -> Result<(), LinkExit> {
        let digest = match &self.sync {
            SyncState::Idle => return Ok(()),
            SyncState::Sending { digest, .. }
            | SyncState::Confirming { digest, .. }
            | SyncState::Receiving { digest, .. } => *digest,
        };
        self.close_transaction(LinkRejectReason::InspectionChanged)?;
        self.abandoned = Some(digest);
        self.reject(digest, LinkRejectReason::InspectionChanged)
            .await
    }

    /// The peer's answer to a transaction this computer already unwound.
    fn answers_abandoned(&mut self, frame: &LinkFrame) -> bool {
        let late = self.abandoned == Some(frame.digest);
        if late {
            self.abandoned = None;
        }
        late
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
        if self.answers_abandoned(frame) {
            return Ok(());
        }
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
        if self.answers_abandoned(frame) {
            return Ok(());
        }
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
        if self.answers_abandoned(frame) {
            return Ok(());
        }
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
        if self.answers_abandoned(frame) {
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

    /// No-op until a summary has been set; a later `Summary` command publishes it immediately.
    async fn publish_summary(&mut self) -> Result<(), LinkExit> {
        let Some(bytes) = self.summary.clone() else {
            return Ok(());
        };
        self.write(LinkFrame::with_payload(
            LinkKind::Summary,
            self.epoch,
            bytes,
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
        let Some(current) = connector.local_displays(&self.inspection).await else {
            return Ok(());
        };
        let moved = !current.same_geometry(&self.inspection.local_displays);
        // A label read before the OS named a display catches up later, at the same geometry.
        if !moved && current.same_labels(&self.inspection.local_displays) {
            return Ok(());
        }
        let payload = encode_topology(&current, self.epoch).map_err(LinkExit::Disconnected)?;
        if moved {
            self.unwind_for_topology().await?;
        }
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
        net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
        rc::Rc,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use monhop_core::{DisplayId, Platform, Point};
    use monhop_protocol::{Capabilities, ControlPermissions, DisplayDescription};
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
        guarded_endpoint::loopback,
        session_handshake::{HandshakeConfig, device_id_from_fingerprint, negotiate},
        session_setup::{GroupMemberRecord, MEMBER_SUPERSEDED_REASON, handshake_failure},
    };

    const TEST_DEADLINE: Duration = Duration::from_secs(30);

    struct Loopback {
        endpoint: quinn::Endpoint,
        peer_address: Option<SocketAddr>,
        identity: DeviceIdentity,
        peer: VerifiedPeer,
        local_platform: Platform,
        peer_platform: Platform,
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
            _cancel: &RevocationSignal,
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
                monhop_protocol::ControlPermissions::BOTH,
                SessionPurpose::Setup,
            )
            .map_err(|_| SetupFailure::Handshake)?;
            let session = negotiate(connection, config)
                .await
                .map_err(handshake_failure)?;
            let inspection = InspectedPeer {
                local_device: device_id_from_fingerprint(self.identity.fingerprint()),
                peer_device: device_id_from_fingerprint(self.peer.fingerprint()),
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

        async fn local_displays(&self, _inspection: &InspectedPeer) -> Option<DisplayTopology> {
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
        /// The next stage blocks until the test releases it, keeping a transaction in flight.
        stage_hold: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
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
                stage_hold: Mutex::new(None),
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
                let hold = stage.stage_hold.lock().unwrap().take();
                if let Some(release) = hold {
                    let _ = release.recv_timeout(TEST_DEADLINE);
                }
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
        (
            Loopback {
                endpoint: client,
                peer_address: Some(server_address),
                identity: client_identity,
                peer: server_pin,
                local_platform: Platform::Windows,
                peer_platform: Platform::MacOs,
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

        fn summary(&self, bytes: &[u8]) {
            self.commands
                .send(LinkCommand::Summary(bytes.to_vec()))
                .unwrap();
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

    /// Waits for the next reported peer summary, so the state the peer holds is never inferred.
    async fn next_summary(inbox: &mut UnboundedReceiver<LinkEvent>) -> Vec<u8> {
        let event = wait_for(inbox, |event| {
            matches!(event, LinkEvent::PeerSummary { .. })
        })
        .await;
        let LinkEvent::PeerSummary { bytes } = event else {
            unreachable!("filtered above");
        };
        bytes
    }

    #[test]
    fn summary_frames_bind_digest_and_bound() {
        let epoch = SessionEpoch::new(4).unwrap();
        let payload = vec![9; MAX_SUMMARY_PAYLOAD_BYTES];
        let frame = LinkFrame::with_payload(LinkKind::Summary, epoch, payload.clone());
        assert_eq!(frame.digest, payload_digest(&payload));
        let encoded = encode_link_frame(&frame).unwrap();
        let (header, body) = encoded.split_at(LINK_HEADER_LEN);
        let header: [u8; LINK_HEADER_LEN] = header.try_into().unwrap();
        let decoded = decode_link_frame(&header, body.to_vec()).unwrap();
        assert_eq!(decoded.kind, LinkKind::Summary);
        assert_eq!(decoded.payload, payload);

        // Empty and past-bound payloads are both refused at encode time.
        let empty = LinkFrame::with_payload(LinkKind::Summary, epoch, Vec::new());
        assert_eq!(
            encode_link_frame(&empty).err(),
            Some(LinkDisconnect::Transport)
        );
        let oversized = LinkFrame::with_payload(
            LinkKind::Summary,
            epoch,
            vec![1; MAX_SUMMARY_PAYLOAD_BYTES + 1],
        );
        assert_eq!(
            encode_link_frame(&oversized).err(),
            Some(LinkDisconnect::Transport)
        );

        // A tampered digest is rejected on decode, same as every other framed kind.
        let mut tampered = header;
        tampered[32] ^= 1;
        assert_eq!(
            decode_link_frame(&tampered, body.to_vec()).err(),
            Some(LinkDisconnect::Transport)
        );
    }

    #[tokio::test]
    async fn a_summary_set_before_connecting_opens_every_connection() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                // Set while the link has not connected yet: it must still reach the peer.
                driver.dialer.summary(b"member record v1");
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                assert_eq!(
                    next_summary(&mut driver.listener.inbox).await,
                    b"member record v1".to_vec()
                );
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
                // The same value is republished on the reconnect too.
                assert_eq!(
                    next_summary(&mut driver.listener.inbox).await,
                    b"member record v1".to_vec()
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
    async fn the_peer_summary_is_reported_and_a_change_is_sent_at_once() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                driver.dialer.summary(b"revision one");
                assert_eq!(
                    next_summary(&mut driver.listener.inbox).await,
                    b"revision one".to_vec()
                );
                driver.dialer.summary(b"revision two");
                assert_eq!(
                    next_summary(&mut driver.listener.inbox).await,
                    b"revision two".to_vec()
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

    #[tokio::test]
    async fn a_relabel_reaches_the_peer_without_unwinding_the_proposal_in_flight() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                let (release, held) = std::sync::mpsc::channel();
                *driver.listener.recorder.stage_hold.lock().unwrap() = Some(held);
                driver.dialer.propose(b"layout for these displays");
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::SyncStarted { sending: false })
                })
                .await;
                let relabeled = DisplayTopology::new(vec![DisplayDescription {
                    name: "named late".into(),
                    ..topology(1, 100).displays()[0].clone()
                }])
                .unwrap();
                *driver.dialer.displays.lock().unwrap() = relabeled.clone();
                // The receiver is staging and the sender awaits its acknowledgement.
                assert!(matches!(
                    next_sync_or_topology(&mut driver.dialer.inbox).await,
                    LinkEvent::TopologyChanged { .. }
                ));
                release.send(()).unwrap();
                let (mut relabel_seen, mut applied) = (false, false);
                while !(relabel_seen && applied) {
                    match next_sync_or_topology(&mut driver.listener.inbox).await {
                        LinkEvent::TopologyChanged { inspection } => {
                            assert!(inspection.peer_displays.same_labels(&relabeled));
                            relabel_seen = true;
                        }
                        LinkEvent::SyncCompleted { sending: false, .. } => applied = true,
                        _ => panic!("a relabel never unwinds the proposal in flight"),
                    }
                }
                assert!(matches!(
                    next_sync_or_topology(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncCompleted { sending: true, .. }
                ));
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    /// The next transaction outcome or topology report, so a dropped link fails instead of hanging.
    async fn next_sync_or_topology(inbox: &mut UnboundedReceiver<LinkEvent>) -> LinkEvent {
        wait_for(inbox, |event| {
            matches!(
                event,
                LinkEvent::SyncCompleted { .. }
                    | LinkEvent::SyncRejected { .. }
                    | LinkEvent::TopologyChanged { .. }
                    | LinkEvent::Disconnected { .. }
            )
        })
        .await
    }

    #[tokio::test]
    async fn a_topology_change_unwinds_the_proposal_in_flight_so_the_next_is_not_busy() {
        let (dialer, listener) =
            with_link((Duration::ZERO, Duration::ZERO), |mut driver| async move {
                wait_connected(&mut driver.dialer.inbox).await;
                wait_connected(&mut driver.listener.inbox).await;
                let (release, held) = std::sync::mpsc::channel();
                *driver.listener.recorder.stage_hold.lock().unwrap() = Some(held);
                driver.dialer.propose(b"layout for the old displays");
                wait_for(&mut driver.listener.inbox, |event| {
                    matches!(event, LinkEvent::SyncStarted { sending: false })
                })
                .await;
                // The receiver is staging and the sender awaits its acknowledgement.
                *driver.dialer.displays.lock().unwrap() = topology(1, 300);
                assert!(matches!(
                    next_sync_or_topology(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::InspectionChanged,
                        sending: true
                    }
                ));
                assert!(matches!(
                    next_sync_or_topology(&mut driver.dialer.inbox).await,
                    LinkEvent::TopologyChanged { .. }
                ));
                release.send(()).unwrap();
                assert!(matches!(
                    next_sync_or_topology(&mut driver.listener.inbox).await,
                    LinkEvent::SyncRejected {
                        reason: LinkRejectReason::InspectionChanged,
                        sending: false
                    }
                ));
                assert!(matches!(
                    next_sync_or_topology(&mut driver.listener.inbox).await,
                    LinkEvent::TopologyChanged { .. }
                ));
                driver.dialer.propose(b"layout for the new displays");
                assert!(matches!(
                    next_sync_or_topology(&mut driver.listener.inbox).await,
                    LinkEvent::SyncCompleted { sending: false, .. }
                ));
                assert!(matches!(
                    next_sync_or_topology(&mut driver.dialer.inbox).await,
                    LinkEvent::SyncCompleted { sending: true, .. }
                ));
                assert_eq!(driver.dialer.recorder.counts(), (1, 1, 0));
                assert_eq!(driver.listener.recorder.counts(), (2, 1, 1));
                assert_eq!(
                    *driver.listener.recorder.saved.lock().unwrap(),
                    b"layout for the new displays".to_vec()
                );
                driver.dialer.close();
                driver.listener.close();
                driver
            })
            .await;
        assert_eq!(dialer, Ok(()));
        assert_eq!(listener, Ok(()));
    }

    const AGREEMENT: [u8; 32] = [7; 32];

    /// The hub's displays on every group endpoint below.
    fn hub_displays(_: DeviceId) -> Result<DisplayTopology, SetupFailure> {
        Ok(topology(1, 100))
    }

    /// A paired computer before it answers the hub, and which of the two dials.
    struct Computer {
        identity: DeviceIdentity,
        socket: UdpSocket,
        address: SocketAddrV4,
        hub_dials: bool,
    }

    impl Computer {
        fn new(hub_dials: bool) -> Self {
            let socket = loopback::bind();
            Self {
                identity: DeviceIdentity::generate().unwrap(),
                address: loopback::address(&socket),
                socket,
                hub_dials,
            }
        }

        /// This computer on its own plain endpoint, running its side of the link with the hub.
        fn remote(self, hub: &GroupHub) -> Loopback {
            let endpoint = if self.hub_dials {
                loopback::listener(self.socket, &self.identity, &hub.pin)
            } else {
                loopback::dialer(self.socket, &self.identity, &hub.pin)
            };
            Loopback {
                endpoint,
                peer_address: (!self.hub_dials).then_some(hub.address),
                identity: self.identity,
                peer: hub.pin.clone(),
                local_platform: Platform::MacOs,
                peer_platform: Platform::Windows,
                displays: Arc::new(Mutex::new(topology(2, 200))),
                connection: Arc::new(Mutex::new(None)),
            }
        }
    }

    /// The Windows computer whose one group endpoint admits every computer above.
    struct GroupHub {
        group: Rc<GroupEndpoint>,
        pin: VerifiedPeer,
        address: SocketAddr,
        /// The endpoint's bind cancel, which the share connects below also run under.
        cancel: RevocationSignal,
    }

    fn group_hub(computers: &[&Computer], controls: &[GroupMemberRecord]) -> GroupHub {
        let identity = DeviceIdentity::generate().unwrap();
        let socket = loopback::bind();
        let address = socket.local_addr().unwrap();
        let hub_pin = pin(&identity);
        let members: Vec<_> = computers
            .iter()
            .map(|computer| (&computer.identity, computer.address, computer.hub_dials))
            .collect();
        let cancel = RevocationSignal::default();
        let group = GroupEndpoint::over_loopback(
            socket,
            identity,
            Platform::Windows,
            &members,
            controls,
            &cancel,
            hub_displays,
        );
        GroupHub {
            group,
            pin: hub_pin,
            address,
            cancel,
        }
    }

    fn sharing(computer: &Computer) -> GroupMemberRecord {
        GroupMemberRecord {
            fingerprint: computer.identity.fingerprint(),
            control: ControlPermissions::BOTH,
        }
    }

    /// What a test drives on one side of a link.
    struct Party {
        inbox: UnboundedReceiver<LinkEvent>,
        commands: UnboundedSender<LinkCommand>,
        recorder: Arc<Recorder>,
        cancel: RevocationSignal,
    }

    /// What that side's link runs with.
    struct Run {
        persist: LinkPersist,
        events: UnboundedSender<LinkEvent>,
        orders: UnboundedReceiver<LinkCommand>,
        cancel: RevocationSignal,
    }

    fn party(order: &Arc<AtomicUsize>) -> (Party, Run) {
        let recorder = Arc::new(Recorder::new(order));
        let (events, inbox) = unbounded_channel();
        let (commands, orders) = unbounded_channel();
        let cancel = RevocationSignal::default();
        (
            Party {
                inbox,
                commands,
                recorder: Arc::clone(&recorder),
                cancel: cancel.clone(),
            },
            Run {
                persist: persist_for(&recorder),
                events,
                orders,
                cancel,
            },
        )
    }

    async fn hub_link(
        hub: &GroupHub,
        member: CertificateFingerprint,
        run: Run,
    ) -> Result<(), SetupFailure> {
        let Run {
            persist,
            events,
            orders,
            cancel,
        } = run;
        run_setup_link_on(&hub.group, member, &cancel, persist, events, orders).await
    }

    /// A computer that dials waits for the hub's link to claim it first, so no dial is refused.
    async fn remote_link(remote: &Loopback, hub: &GroupHub, run: Run) -> Result<(), SetupFailure> {
        let Run {
            persist,
            events,
            mut orders,
            cancel,
        } = run;
        if remote.dials() {
            let member = remote.identity.fingerprint();
            until(|| hub.group.waiting(member)).await;
        }
        run_link(remote, &cancel, &persist, &events, &mut orders).await
    }

    async fn until(condition: impl Fn() -> bool) {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// `from` proposes `bytes` and both computers commit them.
    async fn applies(from: &mut Party, to: &mut Party, bytes: &[u8]) {
        from.commands
            .send(LinkCommand::Propose {
                bytes: bytes.to_vec(),
            })
            .unwrap();
        assert!(matches!(
            settled(&mut from.inbox).await,
            LinkEvent::SyncCompleted { sending: true, .. }
        ));
        assert!(matches!(
            settled(&mut to.inbox).await,
            LinkEvent::SyncCompleted { sending: false, .. }
        ));
        assert_eq!(*to.recorder.saved.lock().unwrap(), bytes);
    }

    async fn closes(party: &mut Party) {
        party.commands.send(LinkCommand::Close).unwrap();
        wait_for(&mut party.inbox, |event| matches!(event, LinkEvent::Closed)).await;
    }

    async fn wait_peer_closed(inbox: &mut UnboundedReceiver<LinkEvent>) {
        wait_for(inbox, |event| {
            matches!(
                event,
                LinkEvent::Disconnected {
                    reason: LinkDisconnect::PeerClosed
                }
            )
        })
        .await;
    }

    /// Waits for the hub's link to connect and reports the computer it names.
    async fn connected_to(inbox: &mut UnboundedReceiver<LinkEvent>) -> CertificateFingerprint {
        let LinkEvent::Connected { inspection } =
            wait_for(inbox, |event| matches!(event, LinkEvent::Connected { .. })).await
        else {
            unreachable!("filtered above");
        };
        inspection.peer_fingerprint
    }

    /// Drains a link's events until the link has ended, refusing any new attempt among them.
    async fn ends_without_another_attempt(inbox: &mut UnboundedReceiver<LinkEvent>) {
        while let Some(event) = tokio::time::timeout(TEST_DEADLINE, inbox.recv())
            .await
            .expect("the link ended")
        {
            assert!(
                !matches!(event, LinkEvent::Connecting { .. }),
                "the link tried to connect again"
            );
        }
    }

    /// The other computer's side of a share session with the hub, on the pair's dial rule.
    async fn remote_share(remote: &Loopback, hub: &GroupHub) -> NegotiatedSession {
        let connection = match remote.peer_address {
            Some(address) => {
                let member = remote.identity.fingerprint();
                until(|| hub.group.waiting(member)).await;
                remote
                    .endpoint
                    .connect(address, LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
                    .unwrap()
            }
            None => remote.endpoint.accept().await.unwrap().await.unwrap(),
        };
        let displays = remote.displays.lock().unwrap().clone();
        let capabilities =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .unwrap();
        let config = HandshakeConfig::new(
            &remote.identity,
            &remote.peer,
            remote.local_platform,
            remote.peer_platform,
            capabilities,
            capabilities,
            &displays,
            ControlPermissions::BOTH,
            SessionPurpose::Share,
        )
        .unwrap()
        .with_agreement(AGREEMENT);
        negotiate(connection, config).await.unwrap()
    }

    /// A share session between the hub and `remote`, from both ends.
    async fn share(hub: &GroupHub, remote: &Loopback) -> (PairedMember, NegotiatedSession) {
        let member = remote.identity.fingerprint();
        let (paired, answered) = tokio::join!(
            hub.group
                .connect(member, SessionPurpose::Share, AGREEMENT, &hub.cancel),
            remote_share(remote, hub)
        );
        let paired = paired.unwrap();
        assert!(paired.inspection.peer_fingerprint == member);
        (paired, answered)
    }

    async fn still_carries(ours: &quinn::Connection, theirs: &quinn::Connection) {
        for (from, to) in [(ours, theirs), (theirs, ours)] {
            from.send_datagram(b"still connected".to_vec().into())
                .unwrap();
            let received = tokio::time::timeout(TEST_DEADLINE, to.read_datagram())
                .await
                .expect("the datagram crossed")
                .unwrap();
            assert_eq!(received.as_ref(), b"still connected");
        }
    }

    async fn closed_with(connection: &quinn::Connection, reason: &[u8]) {
        let closed = tokio::time::timeout(TEST_DEADLINE, connection.closed())
            .await
            .expect("the connection closed");
        assert!(
            matches!(
                &closed,
                quinn::ConnectionError::ApplicationClosed(close) if close.reason.as_ref() == reason
            ),
            "{closed}"
        );
    }

    /// The connection the other computer's link last made or accepted.
    fn remote_connection(remote: &Loopback) -> quinn::Connection {
        remote.connection.lock().unwrap().clone().unwrap()
    }

    #[tokio::test]
    async fn a_setup_link_runs_over_the_group_endpoint() {
        for hub_dials in [true, false] {
            let computer = Computer::new(hub_dials);
            let hub = group_hub(&[&computer], &[]);
            let remote = computer.remote(&hub);
            let member = remote.identity.fingerprint();
            let order = Arc::new(AtomicUsize::new(0));
            let (mut first, first_run) = party(&order);
            let (mut second, second_run) = party(&order);
            let (mut theirs, their_run) = party(&order);
            let ((closed, cancelled), remote_side, ()) =
                tokio::time::timeout(TEST_DEADLINE, async {
                    tokio::join!(
                        async {
                            let closed = hub_link(&hub, member, first_run).await;
                            (closed, hub_link(&hub, member, second_run).await)
                        },
                        remote_link(&remote, &hub, their_run),
                        async {
                            assert!(connected_to(&mut first.inbox).await == member);
                            wait_connected(&mut theirs.inbox).await;
                            applies(&mut first, &mut theirs, b"layout from the hub").await;
                            applies(&mut theirs, &mut first, b"layout from the member").await;

                            // Closing says goodbye and ends only this member's connection.
                            closes(&mut first).await;
                            wait_peer_closed(&mut theirs.inbox).await;
                            assert!(!hub.group.revocation().is_stopping());

                            // The same endpoint carries the next link, and a cancelled link lets
                            // go of its connection through the endpoint as well.
                            wait_connected(&mut second.inbox).await;
                            wait_connected(&mut theirs.inbox).await;
                            let connection = remote_connection(&remote);
                            second.cancel.revoke();
                            closed_with(&connection, MEMBER_RELEASED_REASON).await;
                            assert!(hub.group.admits(member));
                            assert!(!hub.group.revocation().is_stopping());
                            theirs.cancel.revoke();
                        }
                    )
                })
                .await
                .expect("bounded group setup link test");
            assert_eq!(closed, Ok(()), "hub dials: {hub_dials}");
            assert_eq!(cancelled, Err(SetupFailure::Cancelled));
            assert_eq!(remote_side, Err(SetupFailure::Cancelled));
        }
    }

    #[tokio::test]
    async fn two_members_setup_links_run_concurrently_on_one_endpoint() {
        let computers = [Computer::new(true), Computer::new(false)];
        let hub = group_hub(&computers.each_ref(), &[]);
        let [dialed, awaited] = computers.map(|computer| computer.remote(&hub));
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours_dialed, ours_dialed_run) = party(&order);
        let (mut ours_awaited, ours_awaited_run) = party(&order);
        let (mut theirs_dialed, theirs_dialed_run) = party(&order);
        let (mut theirs_awaited, theirs_awaited_run) = party(&order);
        let (hub_dialed, hub_awaited, remote_dialed, remote_awaited, ()) =
            tokio::time::timeout(TEST_DEADLINE, async {
                tokio::join!(
                    hub_link(&hub, dialed.identity.fingerprint(), ours_dialed_run),
                    hub_link(&hub, awaited.identity.fingerprint(), ours_awaited_run),
                    remote_link(&dialed, &hub, theirs_dialed_run),
                    remote_link(&awaited, &hub, theirs_awaited_run),
                    async {
                        assert!(
                            connected_to(&mut ours_dialed.inbox).await
                                == dialed.identity.fingerprint()
                        );
                        assert!(
                            connected_to(&mut ours_awaited.inbox).await
                                == awaited.identity.fingerprint()
                        );
                        wait_connected(&mut theirs_dialed.inbox).await;
                        wait_connected(&mut theirs_awaited.inbox).await;
                        // One transaction on each member's connection at the same time.
                        tokio::join!(
                            applies(&mut ours_dialed, &mut theirs_dialed, b"layout one"),
                            applies(&mut theirs_awaited, &mut ours_awaited, b"layout two")
                        );
                        closes(&mut ours_dialed).await;
                        closes(&mut ours_awaited).await;
                        theirs_dialed.cancel.revoke();
                        theirs_awaited.cancel.revoke();
                    }
                )
            })
            .await
            .expect("bounded group setup link test");
        assert_eq!(hub_dialed, Ok(()));
        assert_eq!(hub_awaited, Ok(()));
        assert_eq!(remote_dialed, Err(SetupFailure::Cancelled));
        assert_eq!(remote_awaited, Err(SetupFailure::Cancelled));
    }

    #[tokio::test]
    async fn closing_one_members_link_keeps_the_other_members_link_and_session() {
        let computers = [
            Computer::new(true),
            Computer::new(false),
            Computer::new(true),
        ];
        let hub = group_hub(&computers.each_ref(), &[sharing(&computers[2])]);
        let [closing, kept, shared] = computers.map(|computer| computer.remote(&hub));
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours_closing, ours_closing_run) = party(&order);
        let (mut ours_kept, ours_kept_run) = party(&order);
        let (mut theirs_closing, theirs_closing_run) = party(&order);
        let (mut theirs_kept, theirs_kept_run) = party(&order);
        let (hub_closing, hub_kept, remote_closing, remote_kept, ()) =
            tokio::time::timeout(TEST_DEADLINE, async {
                tokio::join!(
                    hub_link(&hub, closing.identity.fingerprint(), ours_closing_run),
                    hub_link(&hub, kept.identity.fingerprint(), ours_kept_run),
                    remote_link(&closing, &hub, theirs_closing_run),
                    remote_link(&kept, &hub, theirs_kept_run),
                    async {
                        for party in [
                            &mut ours_closing,
                            &mut ours_kept,
                            &mut theirs_closing,
                            &mut theirs_kept,
                        ] {
                            wait_connected(&mut party.inbox).await;
                        }
                        let (session, answered) = share(&hub, &shared).await;

                        closes(&mut ours_closing).await;
                        wait_peer_closed(&mut theirs_closing.inbox).await;

                        // The other link and the share session carry on over the same endpoint.
                        applies(&mut ours_kept, &mut theirs_kept, b"layout after a close").await;
                        still_carries(&session.session.connection, &answered.connection).await;
                        assert!(!session.cancel.is_stopping());
                        assert!(!hub.group.revocation().is_stopping());

                        closes(&mut ours_kept).await;
                        wait_peer_closed(&mut theirs_kept.inbox).await;
                        still_carries(&session.session.connection, &answered.connection).await;
                        theirs_closing.cancel.revoke();
                        theirs_kept.cancel.revoke();
                    }
                )
            })
            .await
            .expect("bounded group setup link test");
        assert_eq!(hub_closing, Ok(()));
        assert_eq!(hub_kept, Ok(()));
        assert_eq!(remote_closing, Err(SetupFailure::Cancelled));
        assert_eq!(remote_kept, Err(SetupFailure::Cancelled));
    }

    #[tokio::test]
    async fn a_setup_link_and_a_share_session_with_different_members_coexist() {
        let computers = [Computer::new(false), Computer::new(false)];
        let hub = group_hub(&computers.each_ref(), &[sharing(&computers[1])]);
        let [linked, shared] = computers.map(|computer| computer.remote(&hub));
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours, our_run) = party(&order);
        let (mut theirs, their_run) = party(&order);
        let (hub_side, remote_side, ()) = tokio::time::timeout(TEST_DEADLINE, async {
            tokio::join!(
                hub_link(&hub, linked.identity.fingerprint(), our_run),
                remote_link(&linked, &hub, their_run),
                async {
                    // Both computers wait on the one accept router at once.
                    let (session, answered) = share(&hub, &shared).await;
                    wait_connected(&mut ours.inbox).await;
                    wait_connected(&mut theirs.inbox).await;
                    applies(&mut ours, &mut theirs, b"layout beside a share").await;
                    still_carries(&session.session.connection, &answered.connection).await;

                    // Ending the share leaves the link alone.
                    hub.group.close_member(shared.identity.fingerprint());
                    closed_with(&answered.connection, MEMBER_RELEASED_REASON).await;
                    assert!(session.cancel.is_stopping());
                    applies(&mut theirs, &mut ours, b"layout after the share").await;

                    closes(&mut ours).await;
                    theirs.cancel.revoke();
                }
            )
        })
        .await
        .expect("bounded group setup link test");
        assert_eq!(hub_side, Ok(()));
        assert_eq!(remote_side, Err(SetupFailure::Cancelled));
    }

    /// Per H2 two computers hold either a setup link or a share session, never both. The newest
    /// connection wins: the setup link it replaced ends as cancelled instead of redialing and
    /// replacing the share in turn.
    #[tokio::test]
    async fn a_new_share_connect_supersedes_a_members_setup_link() {
        let computer = Computer::new(false);
        let hub = group_hub(&[&computer], &[sharing(&computer)]);
        let remote = computer.remote(&hub);
        let member = remote.identity.fingerprint();
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours, our_run) = party(&order);
        let (mut theirs, their_run) = party(&order);
        let (hub_side, remote_side, ()) = tokio::time::timeout(TEST_DEADLINE, async {
            tokio::join!(
                hub_link(&hub, member, our_run),
                remote_link(&remote, &hub, their_run),
                async {
                    wait_connected(&mut ours.inbox).await;
                    wait_connected(&mut theirs.inbox).await;
                    let link = remote_connection(&remote);
                    let (session, answered) = share(&hub, &remote).await;
                    closed_with(&link, MEMBER_SUPERSEDED_REASON).await;
                    ends_without_another_attempt(&mut ours.inbox).await;

                    // The other computer's link dials again and nothing waits for it.
                    wait_for(&mut theirs.inbox, |event| {
                        matches!(
                            event,
                            LinkEvent::Disconnected {
                                reason: LinkDisconnect::Attempt
                            }
                        )
                    })
                    .await;
                    still_carries(&session.session.connection, &answered.connection).await;
                    assert!(!session.cancel.is_stopping());
                    theirs.cancel.revoke();
                }
            )
        })
        .await
        .expect("bounded group setup link test");
        assert_eq!(hub_side, Err(SetupFailure::Cancelled));
        assert_eq!(remote_side, Err(SetupFailure::Cancelled));
    }

    /// The same holds while the replaced link waits to redial: its next attempt never starts.
    #[tokio::test]
    async fn a_share_connected_while_a_setup_link_waits_to_redial_ends_that_link() {
        let computer = Computer::new(false);
        let hub = group_hub(&[&computer], &[sharing(&computer)]);
        let remote = computer.remote(&hub);
        let member = remote.identity.fingerprint();
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours, our_run) = party(&order);
        let (mut theirs, their_run) = party(&order);
        let (hub_side, remote_side, ()) = tokio::time::timeout(TEST_DEADLINE, async {
            tokio::join!(
                hub_link(&hub, member, our_run),
                remote_link(&remote, &hub, their_run),
                async {
                    wait_connected(&mut ours.inbox).await;
                    wait_connected(&mut theirs.inbox).await;
                    // The other computer's link goes away without a goodbye.
                    theirs.cancel.revoke();
                    remote_connection(&remote).close(0_u32.into(), b"gone");
                    wait_for(&mut ours.inbox, |event| {
                        matches!(event, LinkEvent::Disconnected { .. })
                    })
                    .await;
                    let (session, answered) = share(&hub, &remote).await;
                    ends_without_another_attempt(&mut ours.inbox).await;
                    still_carries(&session.session.connection, &answered.connection).await;
                    assert!(!session.cancel.is_stopping());
                }
            )
        })
        .await
        .expect("bounded group setup link test");
        assert_eq!(hub_side, Err(SetupFailure::Cancelled));
        assert_eq!(remote_side, Err(SetupFailure::Cancelled));
    }

    #[tokio::test]
    async fn forgetting_a_member_ends_its_setup_link_as_pairing_required() {
        let computer = Computer::new(true);
        let hub = group_hub(&[&computer], &[]);
        let remote = computer.remote(&hub);
        let member = remote.identity.fingerprint();
        let order = Arc::new(AtomicUsize::new(0));
        let (mut ours, our_run) = party(&order);
        let (mut theirs, their_run) = party(&order);
        let (hub_side, remote_side, ()) = tokio::time::timeout(TEST_DEADLINE, async {
            tokio::join!(
                hub_link(&hub, member, our_run),
                remote_link(&remote, &hub, their_run),
                async {
                    wait_connected(&mut ours.inbox).await;
                    wait_connected(&mut theirs.inbox).await;
                    hub.group.forget(member);
                    ends_without_another_attempt(&mut ours.inbox).await;
                    theirs.cancel.revoke();
                }
            )
        })
        .await
        .expect("bounded group setup link test");
        assert_eq!(hub_side, Err(SetupFailure::PairingRequired));
        assert_eq!(remote_side, Err(SetupFailure::Cancelled));
    }
}
