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

/// A purpose or source disagreement is symmetric, so the peer reaches the same verdict from our
/// frames; closing before it acknowledges them strands it on [`HandshakeError::Stream`]. Bounded
/// by this grace and by the handshake deadline, whichever comes first.
const DISAGREEMENT_ACK_GRACE: Duration = Duration::from_millis(150);

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
    let deadline = tokio::time::Instant::now() + HANDSHAKE_DEADLINE;
    let result = tokio::time::timeout_at(deadline, negotiate_inner(&connection, &config, deadline))
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
    deadline: tokio::time::Instant,
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
    let peer = match validate_peer_handshake(config, observed_peer, epoch, &frames) {
        Ok(peer) => peer,
        // Only a disagreement waits, and only to let the peer name the same one; a bad certificate,
        // an invalid frame, or a timeout still closes at once.
        Err(error @ (HandshakeError::PurposeMismatch | HandshakeError::SourceMismatch)) => {
            acknowledge_disagreement(&mut send, deadline).await;
            return Err(error);
        }
        Err(error) => return Err(error),
    };

    Ok(HandshakeParts {
        epoch,
        peer,
        send,
        recv,
        reader,
    })
}

/// Waits, bounded, for the peer's transport to have acknowledged the three frames already written
/// before the caller returns and the close guard drops what quinn has not yet transmitted.
/// `stopped()` resolves on that acknowledgement, not on the peer's application having read them.
async fn acknowledge_disagreement(send: &mut quinn::SendStream, deadline: tokio::time::Instant) {
    if send.finish().is_err() {
        return;
    }
    let grace = (tokio::time::Instant::now() + DISAGREEMENT_ACK_GRACE).min(deadline);
    let _ = tokio::time::timeout_at(grace, send.stopped()).await;
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
    // Purpose first: a link meeting a session is a step collision whatever either side believes
    // about the keyboard, and only its own verdict tells the caller to change step. A source
    // disagreement is then reachable only between two sessions whose saved records differ.
    if setup.purpose != config.purpose {
        return Err(HandshakeError::PurposeMismatch);
    }
    if setup.source != config.source {
        return Err(HandshakeError::SourceMismatch);
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use monhop_core::{DisplayId, Point};
    use monhop_protocol::DisplayDescription;

    use super::*;
    use crate::crypto::{LOCAL_TLS_SERVER_NAME, SecureQuicConfig};

    fn topology(id: u64) -> DisplayTopology {
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

    fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
        VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .expect("generated identity has a full matching pin")
    }

    fn capabilities() -> Capabilities {
        Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
            .expect("fixture capabilities are known")
    }

    /// Negotiates both ends of one loopback connection at once, each side told which computer it
    /// believes supplies input (`true` for the dialing one) and which purpose it believes it opened.
    async fn negotiate_both(
        client: (bool, SessionPurpose),
        server: (bool, SessionPurpose),
    ) -> (
        Result<NegotiatedSession, HandshakeError>,
        Result<NegotiatedSession, HandshakeError>,
    ) {
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let server_pin = pin(&server_identity);
        let client_pin = pin(&client_identity);
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = quinn::Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_pin)
                .expect("server TLS configuration"),
            loopback,
        )
        .expect("loopback server endpoint");
        let mut dialer = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
        dialer.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_pin)
                .expect("client TLS configuration"),
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
        let client_device = device_id_from_fingerprint(client_identity.fingerprint());
        let server_device = device_id_from_fingerprint(server_identity.fingerprint());
        let device = |source_is_dialer: bool| {
            if source_is_dialer {
                client_device
            } else {
                server_device
            }
        };
        let client_topology = topology(1);
        let server_topology = topology(2);
        let client_config = HandshakeConfig::new(
            &client_identity,
            &server_pin,
            Platform::Windows,
            Platform::MacOs,
            capabilities(),
            capabilities(),
            &client_topology,
            device(client.0),
            client.1,
        )
        .expect("client handshake configuration");
        let server_config = HandshakeConfig::new(
            &server_identity,
            &client_pin,
            Platform::MacOs,
            Platform::Windows,
            capabilities(),
            capabilities(),
            &server_topology,
            device(server.0),
            server.1,
        )
        .expect("server handshake configuration");
        let verdicts = tokio::join!(
            negotiate(client_connection, client_config),
            negotiate(server_connection, server_config),
        );
        dialer.close(0_u32.into(), b"fixture complete");
        listener.close(0_u32.into(), b"fixture complete");
        verdicts
    }

    #[tokio::test]
    async fn one_computer_arranging_and_one_sharing_both_name_the_purpose_disagreement() {
        let (client, server) =
            negotiate_both((true, SessionPurpose::Setup), (true, SessionPurpose::Share)).await;
        assert_eq!(client.err(), Some(HandshakeError::PurposeMismatch));
        assert_eq!(server.err(), Some(HandshakeError::PurposeMismatch));
    }

    /// A setup link names the dialing computer as its source, so a link meeting a session whose
    /// record puts the keyboard elsewhere disagrees on both fields. Only the purpose verdict tells
    /// the caller to change step, so it must win.
    #[tokio::test]
    async fn a_link_meeting_a_session_names_the_purpose_whoever_holds_the_keyboard() {
        let (client, server) = negotiate_both(
            (true, SessionPurpose::Setup),
            (false, SessionPurpose::Share),
        )
        .await;
        assert_eq!(client.err(), Some(HandshakeError::PurposeMismatch));
        assert_eq!(server.err(), Some(HandshakeError::PurposeMismatch));
    }

    #[tokio::test]
    async fn two_computers_claiming_input_both_name_the_source_disagreement() {
        let (client, server) = negotiate_both(
            (true, SessionPurpose::Share),
            (false, SessionPurpose::Share),
        )
        .await;
        assert_eq!(client.err(), Some(HandshakeError::SourceMismatch));
        assert_eq!(server.err(), Some(HandshakeError::SourceMismatch));
    }

    #[tokio::test]
    async fn agreeing_computers_still_negotiate() {
        let (client, server) =
            negotiate_both((true, SessionPurpose::Share), (true, SessionPurpose::Share)).await;
        assert!(client.is_ok());
        assert!(server.is_ok());
    }
}
