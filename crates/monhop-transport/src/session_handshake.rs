//! Bilateral session negotiation on an already-authenticated QUIC connection.
//!
//! This module never creates sockets, changes trust, or enables native input. It only binds the
//! protocol identity to the established TLS peer and exchanges the three control frames required
//! before a caller may create an input sender or receiver.

use std::{error::Error, fmt, time::Duration};

use monhop_core::{DeviceId, Platform};
use monhop_protocol::{
    Capabilities, DeliveryClass, DisplayTopology, Frame, Hello, Message, PROTOCOL_VERSION,
    SessionEpoch, SessionSetup,
};

pub use monhop_protocol::SessionPurpose;

use crate::{
    crypto::{CertificateFingerprint, DeviceIdentity, VerifiedPeer},
    session_wire::{FrameReader, SessionWireError, write_frame},
};

/// Bounds the entire control exchange, including stream creation and all six frame operations.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_millis(500);

const HANDSHAKE_CLOSE_CODE: u32 = 2;
const HANDSHAKE_CLOSE_REASON: &[u8] = b"session handshake failed";
const EPOCH_EXPORT_LABEL: &[u8] = b"monhop/session-epoch/v2";
const EPOCH_EXPORT_CONTEXT: &[u8] = b"authenticated-session";
const HANDSHAKE_FRAME_COUNT: usize = 3;

/// Stable, non-authoritative device identifier derived from the first 16 bytes of a full
/// SHA-256 certificate fingerprint. The full 32-byte fingerprint remains the trust binding.
pub fn device_id_from_fingerprint(fingerprint: CertificateFingerprint) -> DeviceId {
    let mut device_id = [0; 16];
    device_id.copy_from_slice(&fingerprint.as_bytes()[..16]);
    DeviceId(device_id)
}

/// Locally authorized expectations for the session peer and physical input source.
pub struct HandshakeConfig<'a> {
    local_identity: &'a DeviceIdentity,
    expected_peer: &'a VerifiedPeer,
    local_platform: Platform,
    expected_peer_platform: Platform,
    local_capabilities: Capabilities,
    expected_peer_capabilities: Capabilities,
    local_topology: &'a DisplayTopology,
    source: DeviceId,
    purpose: SessionPurpose,
}

impl<'a> HandshakeConfig<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local_identity: &'a DeviceIdentity,
        expected_peer: &'a VerifiedPeer,
        local_platform: Platform,
        expected_peer_platform: Platform,
        local_capabilities: Capabilities,
        expected_peer_capabilities: Capabilities,
        local_topology: &'a DisplayTopology,
        source: DeviceId,
        purpose: SessionPurpose,
    ) -> Result<Self, HandshakeError> {
        let config = Self {
            local_identity,
            expected_peer,
            local_platform,
            expected_peer_platform,
            local_capabilities,
            expected_peer_capabilities,
            local_topology,
            source,
            purpose,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn local_device_id(&self) -> DeviceId {
        device_id_from_fingerprint(self.local_identity.fingerprint())
    }

    pub fn expected_peer_device_id(&self) -> DeviceId {
        device_id_from_fingerprint(self.expected_peer.fingerprint())
    }

    pub const fn source(&self) -> DeviceId {
        self.source
    }

    pub const fn purpose(&self) -> SessionPurpose {
        self.purpose
    }

    fn validate(&self) -> Result<(), HandshakeError> {
        Capabilities::new(self.local_capabilities.bits())
            .map_err(|_| HandshakeError::InvalidConfiguration)?;
        Capabilities::new(self.expected_peer_capabilities.bits())
            .map_err(|_| HandshakeError::InvalidConfiguration)?;
        DisplayTopology::new(self.local_topology.displays().to_vec())
            .map_err(|_| HandshakeError::InvalidConfiguration)?;
        let local = self.local_device_id();
        let peer = self.expected_peer_device_id();
        if self.source != local && self.source != peer {
            return Err(HandshakeError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Public peer facts validated by the completed bilateral exchange.
#[derive(Clone, Debug, PartialEq)]
pub struct NegotiatedPeer {
    pub device_id: DeviceId,
    pub platform: Platform,
    pub capabilities: Capabilities,
    pub topology: DisplayTopology,
}

/// The exact continuation for the control stream after negotiation.
///
/// Control Ping/Pong frames retain the exporter-derived initial epoch. Input activation uses
/// [`InputEpochContinuation`] with independent epoch and sequence state on the ordered stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlContinuation {
    epoch: SessionEpoch,
    next_sequence: u64,
}

impl ControlContinuation {
    pub const fn epoch(self) -> SessionEpoch {
        self.epoch
    }

    pub const fn next_sequence(self) -> u64 {
        self.next_sequence
    }

    /// Creates the next baseline-epoch control frame and advances its ordered sequence once.
    pub fn next_heartbeat(&mut self, message: Message) -> Result<Frame, ContinuationError> {
        if !matches!(message, Message::Ping(_) | Message::Pong(_)) {
            return Err(ContinuationError::NotHeartbeat);
        }
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(ContinuationError::SequenceExhausted)?;
        let frame = Frame::new(self.epoch, self.next_sequence, message);
        self.next_sequence = next;
        Ok(frame)
    }
}

/// Coherently advances input activation epochs from the negotiated baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEpochContinuation {
    active_epoch: SessionEpoch,
}

impl InputEpochContinuation {
    pub const fn active_epoch(self) -> SessionEpoch {
        self.active_epoch
    }

    /// Returns the next activation epoch, beginning at `initial_epoch + 1`.
    pub fn begin_activation(&mut self) -> Result<SessionEpoch, ContinuationError> {
        let epoch = self
            .active_epoch
            .get()
            .checked_add(1)
            .and_then(|value| SessionEpoch::new(value).ok())
            .ok_or(ContinuationError::EpochExhausted)?;
        self.active_epoch = epoch;
        Ok(epoch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationError {
    NotHeartbeat,
    SequenceExhausted,
    EpochExhausted,
}

impl fmt::Display for ContinuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotHeartbeat => "control continuation only creates Ping or Pong frames",
            Self::SequenceExhausted => "reliable sequence is exhausted",
            Self::EpochExhausted => "transition epoch is exhausted",
        })
    }
}

impl Error for ContinuationError {}

/// Control stream state left positioned immediately after the three handshake frames.
pub struct ControlStreams {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    pub reader: FrameReader,
}

/// A negotiated session that can be handed to the source or destination runtime.
pub struct NegotiatedSession {
    pub connection: quinn::Connection,
    pub(crate) source: DeviceId,
    pub(crate) destination: DeviceId,
    pub(crate) local_is_source: bool,
    pub peer: NegotiatedPeer,
    pub(crate) local: NegotiatedPeer,
    purpose: SessionPurpose,
    pub initial_epoch: SessionEpoch,
    pub control: ControlContinuation,
    pub input_epochs: InputEpochContinuation,
    pub control_streams: ControlStreams,
}

impl NegotiatedSession {
    /// Returns the source device authenticated by the bilateral exchange.
    pub const fn source(&self) -> DeviceId {
        self.source
    }

    /// Returns the destination device authenticated by the bilateral exchange.
    pub const fn destination(&self) -> DeviceId {
        self.destination
    }

    /// Reports whether this endpoint was authenticated as the session source.
    pub const fn local_is_source(&self) -> bool {
        self.local_is_source
    }

    /// Returns the purpose authenticated by both peers during setup.
    pub const fn purpose(&self) -> SessionPurpose {
        self.purpose
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    InvalidConfiguration,
    TimedOut,
    Stream,
    PeerIdentityUnavailable,
    PeerCertificateMismatch,
    EpochDerivation,
    InvalidFrame,
    UnexpectedFrame,
    PeerHelloMismatch,
    SourceMismatch,
    PurposeMismatch,
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConfiguration => "session handshake configuration is invalid",
            Self::TimedOut => "session handshake exceeded its deadline",
            Self::Stream => "session handshake stream operation failed",
            Self::PeerIdentityUnavailable => "session peer certificate is unavailable",
            Self::PeerCertificateMismatch => "session peer certificate does not match the pin",
            Self::EpochDerivation => "session epoch could not be derived",
            Self::InvalidFrame => "session handshake frame is invalid",
            Self::UnexpectedFrame => "session handshake frame is out of order or unexpected",
            Self::PeerHelloMismatch => "session peer hello does not match the negotiated policy",
            Self::SourceMismatch => "session peer chose a different input source",
            Self::PurposeMismatch => "session peer chose a different session purpose",
        })
    }
}

impl Error for HandshakeError {}

/// Negotiates a session on an already-authenticated connection.
///
/// Both peers open and write their own reliable stream, then accept and validate the other's
/// stream. This is symmetric and does not depend on whether either peer dialed or listened. Any
/// error, timeout, or cancellation closes the connection so a partial exchange is never retried.
pub async fn negotiate(
    connection: quinn::Connection,
    config: HandshakeConfig<'_>,
) -> Result<NegotiatedSession, HandshakeError> {
    let mut close_guard = CloseConnectionOnDrop::new(&connection);
    let result = tokio::time::timeout(HANDSHAKE_DEADLINE, negotiate_inner(&connection, &config))
        .await
        .map_err(|_| HandshakeError::TimedOut);
    let parts = match result.and_then(|inner| inner) {
        Ok(parts) => parts,
        Err(error) => {
            log::warn!("handshake failed ({:?}): {error:?}", config.purpose);
            return Err(error);
        }
    };
    log::info!(
        "handshake done ({:?}): local is source={}, peer platform {:?}",
        config.purpose,
        config.source == config.local_device_id(),
        parts.peer.platform
    );
    close_guard.disarm();
    drop(close_guard);

    let destination = if config.source == config.local_device_id() {
        config.expected_peer_device_id()
    } else {
        config.local_device_id()
    };
    Ok(NegotiatedSession {
        connection,
        source: config.source,
        destination,
        local_is_source: config.source == config.local_device_id(),
        peer: parts.peer,
        local: NegotiatedPeer {
            device_id: config.local_device_id(),
            platform: config.local_platform,
            capabilities: config.local_capabilities,
            topology: config.local_topology.clone(),
        },
        purpose: config.purpose,
        initial_epoch: parts.epoch,
        control: ControlContinuation {
            epoch: parts.epoch,
            next_sequence: HANDSHAKE_FRAME_COUNT as u64,
        },
        input_epochs: InputEpochContinuation {
            active_epoch: parts.epoch,
        },
        control_streams: ControlStreams {
            send: parts.send,
            recv: parts.recv,
            reader: parts.reader,
        },
    })
}

struct HandshakeParts {
    epoch: SessionEpoch,
    peer: NegotiatedPeer,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    reader: FrameReader,
}

async fn negotiate_inner(
    connection: &quinn::Connection,
    config: &HandshakeConfig<'_>,
) -> Result<HandshakeParts, HandshakeError> {
    config.validate()?;
    let observed_peer = observed_peer_fingerprint(connection)?;
    let epoch = derive_epoch(connection)?;

    let (mut send, _unused_recv) = connection
        .open_bi()
        .await
        .map_err(|_| HandshakeError::Stream)?;
    write_frame(
        connection,
        &mut send,
        &Frame::new(
            epoch,
            0,
            Message::Hello(Hello {
                device_id: config.local_device_id(),
                platform: config.local_platform,
                protocol_version: PROTOCOL_VERSION,
                capabilities: config.local_capabilities,
            }),
        ),
    )
    .await
    .map_err(map_wire_error)?;
    write_frame(
        connection,
        &mut send,
        &Frame::new(
            epoch,
            1,
            Message::DisplayTopology(config.local_topology.clone()),
        ),
    )
    .await
    .map_err(map_wire_error)?;
    write_frame(
        connection,
        &mut send,
        &Frame::new(
            epoch,
            2,
            Message::SessionSetup(SessionSetup {
                source: config.source,
                purpose: config.purpose,
            }),
        ),
    )
    .await
    .map_err(map_wire_error)?;

    let (_unused_send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|_| HandshakeError::Stream)?;
    let mut reader = FrameReader::new();
    let frames = [
        reader.read_frame(&mut recv).await.map_err(map_wire_error)?,
        reader.read_frame(&mut recv).await.map_err(map_wire_error)?,
        reader.read_frame(&mut recv).await.map_err(map_wire_error)?,
    ];
    let peer = validate_peer_handshake(config, observed_peer, epoch, &frames)?;

    Ok(HandshakeParts {
        epoch,
        peer,
        send,
        recv,
        reader,
    })
}

fn observed_peer_fingerprint(
    connection: &quinn::Connection,
) -> Result<CertificateFingerprint, HandshakeError> {
    let certificates = connection
        .peer_identity()
        .ok_or(HandshakeError::PeerIdentityUnavailable)?
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| HandshakeError::PeerIdentityUnavailable)?;
    let [certificate] = certificates.as_slice() else {
        return Err(HandshakeError::PeerIdentityUnavailable);
    };
    Ok(CertificateFingerprint::from_certificate_der(
        certificate.as_ref(),
    ))
}

fn derive_epoch(connection: &quinn::Connection) -> Result<SessionEpoch, HandshakeError> {
    let mut bytes = [0; 8];
    connection
        .export_keying_material(&mut bytes, EPOCH_EXPORT_LABEL, EPOCH_EXPORT_CONTEXT)
        .map_err(|_| HandshakeError::EpochDerivation)?;
    let epoch = SessionEpoch::new(u64::from_be_bytes(bytes))
        .map_err(|_| HandshakeError::EpochDerivation)?;
    // The first input activation must advance from this baseline.
    (epoch.get() != u64::MAX)
        .then_some(epoch)
        .ok_or(HandshakeError::EpochDerivation)
}

/// Validates the peer's complete three-frame proposal without performing I/O.
///
/// This model boundary is shared by the QUIC exchange and tests. `observed_peer` must be the full
/// fingerprint of the certificate presented by the authenticated connection, never a claimed ID.
pub fn validate_peer_handshake(
    config: &HandshakeConfig<'_>,
    observed_peer: CertificateFingerprint,
    epoch: SessionEpoch,
    frames: &[Frame; HANDSHAKE_FRAME_COUNT],
) -> Result<NegotiatedPeer, HandshakeError> {
    if observed_peer != config.expected_peer.fingerprint() {
        return Err(HandshakeError::PeerCertificateMismatch);
    }
    for (sequence, frame) in frames.iter().enumerate() {
        if frame.epoch != epoch || frame.sequence != sequence as u64 {
            return Err(HandshakeError::InvalidFrame);
        }
        if frame.delivery() != DeliveryClass::Reliable {
            return Err(HandshakeError::UnexpectedFrame);
        }
    }

    let Message::Hello(hello) = &frames[0].message else {
        return Err(HandshakeError::UnexpectedFrame);
    };
    if hello.protocol_version != PROTOCOL_VERSION
        || hello.device_id != config.expected_peer_device_id()
        || hello.platform != config.expected_peer_platform
        || hello.capabilities != config.expected_peer_capabilities
    {
        return Err(HandshakeError::PeerHelloMismatch);
    }

    let Message::DisplayTopology(topology) = &frames[1].message else {
        return Err(HandshakeError::UnexpectedFrame);
    };
    let topology = DisplayTopology::new(topology.displays().to_vec())
        .map_err(|_| HandshakeError::InvalidFrame)?;

    let Message::SessionSetup(setup) = frames[2].message else {
        return Err(HandshakeError::UnexpectedFrame);
    };
    if setup.source != config.source {
        return Err(HandshakeError::SourceMismatch);
    }
    if setup.purpose != config.purpose {
        return Err(HandshakeError::PurposeMismatch);
    }

    Ok(NegotiatedPeer {
        device_id: hello.device_id,
        platform: hello.platform,
        capabilities: hello.capabilities,
        topology,
    })
}

fn map_wire_error(error: SessionWireError) -> HandshakeError {
    match error {
        SessionWireError::InvalidFrame
        | SessionWireError::TooLarge
        | SessionWireError::Truncated => HandshakeError::InvalidFrame,
        SessionWireError::Closed
        | SessionWireError::Io
        | SessionWireError::TimedOut
        | SessionWireError::Terminal => HandshakeError::Stream,
    }
}

struct CloseConnectionOnDrop<'a> {
    connection: &'a quinn::Connection,
    armed: bool,
}

impl<'a> CloseConnectionOnDrop<'a> {
    const fn new(connection: &'a quinn::Connection) -> Self {
        Self {
            connection,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CloseConnectionOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.connection
                .close(HANDSHAKE_CLOSE_CODE.into(), HANDSHAKE_CLOSE_REASON);
        }
    }
}
