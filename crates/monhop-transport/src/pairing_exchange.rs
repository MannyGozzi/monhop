//! The pairing exchange on one pairing connection: SPAKE2 on the short code, then key confirmation
//! bound to this TLS session and both certificates, so a relay terminating TLS on each side can
//! never confirm. Only after both confirmations does either side state its address and platform.

use std::{fmt, net::SocketAddrV4, time::Duration};

use monhop_core::Platform;
use ring::{hkdf, hmac};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::{
    crypto::{CertificateFingerprint, DeviceIdentity, presented_certificate},
    pairing::{
        PAIR_FRAME_BYTES, PairFrameKind, PairingError, PairingOffer, encode_pair_frame,
        platform_from_wire, platform_to_wire, validate_pair_frame,
    },
    pairing_code::PairingCode,
};

/// From the connection to both sides' stated endpoints; past it the attempt is not confirmed.
pub const PAIRING_CONFIRM_DEADLINE: Duration = Duration::from_secs(10);
const EXPORTER_LABEL: &[u8] = b"EXPORTER-monhop-pairing-v1";
const EXPORTER_BYTES: usize = 32;
const CONFIRM_INFO: &[u8] = b"monhop pair confirm v1";
const SHOWING_IDENTITY: &[u8] = b"monhop-pair-showing";
const ENTERING_IDENTITY: &[u8] = b"monhop-pair-entering";
const SHOWING_TAG: &[u8] = b"A";
const ENTERING_TAG: &[u8] = b"B";
const FRAME_MAGIC: &[u8; 4] = b"LKP2";
const HEADER_BYTES: usize = FRAME_MAGIC.len() + 1;
/// A side byte and one compressed Ed25519 point.
const PAKE_MESSAGE_BYTES: usize = 33;
const MAC_BYTES: usize = 32;
const ENDPOINT_BYTES: usize = 1 + 4 + 2;
/// The close code a side sends when the attempt fails, so the other side stops at once.
const PAIRING_FAILED: u32 = 1;

/// Which part this computer plays: showing the code (SPAKE2 side A) or typing it (side B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingRole {
    Showing,
    Entering,
}

#[derive(Clone, Copy)]
enum Step {
    Pake = 1,
    Confirm = 2,
    Endpoint = 3,
}

/// Why an exchange ended without pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingFailure {
    /// The code was not confirmed: a wrong code, a relay, a malformed message, or the other side
    /// gave up or stayed silent. The showing side's code is spent either way.
    NotConfirmed,
    /// Confirmed, but the other computer's stated address or platform is not acceptable.
    InvalidPeer,
    /// The connection ended after confirmation, before both sides reported saving.
    Interrupted,
}

impl fmt::Display for PairingFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotConfirmed => "The code was not confirmed. Nothing was saved.",
            Self::InvalidPeer => {
                "The other computer stated an address or platform that does not match its connection. Nothing was enabled."
            }
            Self::Interrupted => {
                "The connection ended before both computers finished saving. Check Computers on both, forget any half-paired entry, then pair again."
            }
        })
    }
}

impl std::error::Error for PairingFailure {}

/// The other computer as its confirmed pairing connection presented it.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfirmedPeer {
    pub certificate: Vec<u8>,
    pub endpoint: SocketAddrV4,
    pub platform: Platform,
}

impl fmt::Debug for ConfirmedPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfirmedPeer")
            .field("fingerprint", &self.fingerprint().short_hex())
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

impl ConfirmedPeer {
    pub fn fingerprint(&self) -> CertificateFingerprint {
        CertificateFingerprint::from_certificate_der(&self.certificate)
    }

    /// The record form, which also requires a private pairing endpoint.
    pub fn offer(&self) -> Result<PairingOffer, PairingError> {
        Ok(PairingOffer::new(self.endpoint, &self.certificate)?.with_platform(self.platform))
    }
}

/// A confirmed exchange waiting for this computer to save the other one.
pub struct ConfirmedPairing {
    connection: quinn::Connection,
    send: quinn::SendStream,
    receive: quinn::RecvStream,
    local: CertificateFingerprint,
    peer: ConfirmedPeer,
}

impl ConfirmedPairing {
    pub fn peer(&self) -> &ConfirmedPeer {
        &self.peer
    }

    /// Once this computer saved the other: says so, and waits until the other says the same.
    pub async fn finish_saved(mut self) -> Result<(), PairingFailure> {
        let result = self.exchange_saved().await;
        if result.is_err() {
            close_failed(&self.connection);
        }
        result
    }

    async fn exchange_saved(&mut self) -> Result<(), PairingFailure> {
        let peer = self.peer.fingerprint();
        self.send
            .write_all(&encode_pair_frame(PairFrameKind::Saved, self.local, peer))
            .await
            .map_err(|_| PairingFailure::Interrupted)?;
        self.send
            .finish()
            .map_err(|_| PairingFailure::Interrupted)?;
        let mut saved = [0; PAIR_FRAME_BYTES];
        self.receive
            .read_exact(&mut saved)
            .await
            .map_err(|_| PairingFailure::Interrupted)?;
        validate_pair_frame(&saved, PairFrameKind::Saved, peer, self.local)
            .map_err(|_| PairingFailure::InvalidPeer)?;
        // After Saved a read error is a teardown race; only real extra data breaks the protocol.
        if matches!(self.receive.read(&mut [0]).await, Ok(Some(_))) {
            return Err(PairingFailure::InvalidPeer);
        }
        let _ = self.send.stopped().await;
        Ok(())
    }
}

/// Runs SPAKE2 on `code` over `connection` and confirms it, then exchanges both computers'
/// stated endpoint and platform. `local_endpoint` is this computer's address on the connection.
/// Any failure closes the connection.
pub async fn confirm_pairing(
    connection: &quinn::Connection,
    role: PairingRole,
    code: &PairingCode,
    local: &DeviceIdentity,
    local_endpoint: SocketAddrV4,
    local_platform: Platform,
) -> Result<ConfirmedPairing, PairingFailure> {
    let mut confirmed = false;
    let attempt = Attempt {
        connection,
        role,
        local,
        local_endpoint,
        local_platform,
    };
    let result = tokio::time::timeout(PAIRING_CONFIRM_DEADLINE, attempt.run(code, &mut confirmed))
        .await
        .unwrap_or(Err(()));
    result.map_err(|()| {
        close_failed(connection);
        if confirmed {
            PairingFailure::Interrupted
        } else {
            PairingFailure::NotConfirmed
        }
    })?
}

struct Attempt<'a> {
    connection: &'a quinn::Connection,
    role: PairingRole,
    local: &'a DeviceIdentity,
    local_endpoint: SocketAddrV4,
    local_platform: Platform,
}

impl Attempt<'_> {
    /// `Err(())` is a failure before or after `confirmed` was set; `Ok(Err)` a refused peer.
    async fn run(
        self,
        code: &PairingCode,
        confirmed: &mut bool,
    ) -> Result<Result<ConfirmedPairing, PairingFailure>, ()> {
        let certificate = presented_certificate(self.connection).ok_or(())?;
        let peer = CertificateFingerprint::from_certificate_der(&certificate);
        let local = self.local.fingerprint();
        let mut exporter = Zeroizing::new([0; EXPORTER_BYTES]);
        self.connection
            .export_keying_material(exporter.as_mut_slice(), EXPORTER_LABEL, b"")
            .map_err(|_| ())?;
        let password = Password::new(code.password().as_slice());
        let (showing, entering) = (
            Identity::new(SHOWING_IDENTITY),
            Identity::new(ENTERING_IDENTITY),
        );
        let (mut send, mut receive) = match self.role {
            PairingRole::Showing => self.connection.accept_bi().await,
            PairingRole::Entering => self.connection.open_bi().await,
        }
        .map_err(|_| ())?;

        let tags = match self.role {
            PairingRole::Showing => (SHOWING_TAG, ENTERING_TAG),
            PairingRole::Entering => (ENTERING_TAG, SHOWING_TAG),
        };
        let confirmation = match self.role {
            PairingRole::Showing => {
                let (spake, ours) = Spake2::<Ed25519Group>::start_a(&password, &showing, &entering);
                let ours = pake_message(&ours)?;
                let theirs: [u8; PAKE_MESSAGE_BYTES] = read_frame(&mut receive, Step::Pake).await?;
                write_frame(&mut send, Step::Pake, &ours).await?;
                let key = Zeroizing::new(spake.finish(&theirs).map_err(|_| ())?);
                confirmation_key(&key, &exporter, [local, peer], [&ours, &theirs])?
            }
            PairingRole::Entering => {
                let (spake, ours) = Spake2::<Ed25519Group>::start_b(&password, &showing, &entering);
                let ours = pake_message(&ours)?;
                write_frame(&mut send, Step::Pake, &ours).await?;
                let theirs: [u8; PAKE_MESSAGE_BYTES] = read_frame(&mut receive, Step::Pake).await?;
                let key = Zeroizing::new(spake.finish(&theirs).map_err(|_| ())?);
                confirmation_key(&key, &exporter, [peer, local], [&theirs, &ours])?
            }
        };
        let own_tag = hmac::sign(&confirmation, tags.0);
        // The entering side proves nothing before the showing side has proven the code.
        if self.role == PairingRole::Showing {
            write_frame(&mut send, Step::Confirm, own_tag.as_ref()).await?;
        }
        let theirs: [u8; MAC_BYTES] = read_frame(&mut receive, Step::Confirm).await?;
        hmac::verify(&confirmation, tags.1, &theirs).map_err(|_| ())?;
        if self.role == PairingRole::Entering {
            write_frame(&mut send, Step::Confirm, own_tag.as_ref()).await?;
        }

        // The showing side states its endpoint only after verifying the other's confirmation, so
        // its endpoint frame tells the entering side that both confirmed.
        let ours = endpoint_frame(self.local_endpoint, self.local_platform);
        let stated = match self.role {
            PairingRole::Showing => {
                *confirmed = true;
                write_frame(&mut send, Step::Endpoint, &ours).await?;
                read_frame(&mut receive, Step::Endpoint).await?
            }
            PairingRole::Entering => {
                let stated = read_frame(&mut receive, Step::Endpoint).await?;
                *confirmed = true;
                write_frame(&mut send, Step::Endpoint, &ours).await?;
                stated
            }
        };
        let Some((endpoint, platform)) = parse_endpoint(&stated) else {
            close_failed(self.connection);
            return Ok(Err(PairingFailure::InvalidPeer));
        };
        if self.connection.remote_address() != endpoint.into() {
            close_failed(self.connection);
            return Ok(Err(PairingFailure::InvalidPeer));
        }
        Ok(Ok(ConfirmedPairing {
            connection: self.connection.clone(),
            send,
            receive,
            local,
            peer: ConfirmedPeer {
                certificate: certificate.to_vec(),
                endpoint,
                platform,
            },
        }))
    }
}

/// HKDF-SHA256 keyed by the SPAKE2 key and salted with this TLS session's exporter, over both
/// certificates as each side saw them and both SPAKE2 messages, showing side first.
fn confirmation_key(
    key: &[u8],
    exporter: &[u8; EXPORTER_BYTES],
    [showing, entering]: [CertificateFingerprint; 2],
    [showing_message, entering_message]: [&[u8; PAKE_MESSAGE_BYTES]; 2],
) -> Result<hmac::Key, ()> {
    let info = [
        CONFIRM_INFO,
        showing.as_bytes(),
        entering.as_bytes(),
        showing_message,
        entering_message,
    ];
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, exporter).extract(key);
    let okm = prk.expand(&info, hmac::HMAC_SHA256).map_err(|_| ())?;
    Ok(hmac::Key::from(okm))
}

fn pake_message(message: &[u8]) -> Result<[u8; PAKE_MESSAGE_BYTES], ()> {
    message.try_into().map_err(|_| ())
}

fn endpoint_frame(endpoint: SocketAddrV4, platform: Platform) -> [u8; ENDPOINT_BYTES] {
    let mut frame = [0; ENDPOINT_BYTES];
    frame[0] = platform_to_wire(platform);
    frame[1..5].copy_from_slice(&endpoint.ip().octets());
    frame[5..].copy_from_slice(&endpoint.port().to_be_bytes());
    frame
}

fn parse_endpoint(frame: &[u8; ENDPOINT_BYTES]) -> Option<(SocketAddrV4, Platform)> {
    let platform = platform_from_wire(frame[0]).ok()??;
    let ip = [frame[1], frame[2], frame[3], frame[4]];
    let port = u16::from_be_bytes([frame[5], frame[6]]);
    Some((SocketAddrV4::new(ip.into(), port), platform))
}

async fn write_frame(send: &mut quinn::SendStream, step: Step, payload: &[u8]) -> Result<(), ()> {
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    frame.extend_from_slice(FRAME_MAGIC);
    frame.push(step as u8);
    frame.extend_from_slice(payload);
    send.write_all(&frame).await.map_err(|_| ())
}

/// Exactly one fixed-length frame of `step`; anything else fails the attempt.
async fn read_frame<const N: usize>(
    receive: &mut quinn::RecvStream,
    step: Step,
) -> Result<[u8; N], ()> {
    let mut header = [0; HEADER_BYTES];
    receive.read_exact(&mut header).await.map_err(|_| ())?;
    if &header[..FRAME_MAGIC.len()] != FRAME_MAGIC || header[FRAME_MAGIC.len()] != step as u8 {
        return Err(());
    }
    let mut payload = [0; N];
    receive.read_exact(&mut payload).await.map_err(|_| ())?;
    Ok(payload)
}

fn close_failed(connection: &quinn::Connection) {
    connection.close(PAIRING_FAILED.into(), b"pairing not completed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_frames_round_trip_and_reject_unknown_platforms() {
        let endpoint: SocketAddrV4 = "192.168.1.30:24872".parse().unwrap();
        for platform in [Platform::Windows, Platform::MacOs] {
            let frame = endpoint_frame(endpoint, platform);
            assert_eq!(parse_endpoint(&frame), Some((endpoint, platform)));
        }
        let mut unknown = endpoint_frame(endpoint, Platform::MacOs);
        for value in [0, 3, 255] {
            unknown[0] = value;
            assert_eq!(parse_endpoint(&unknown), None);
        }
    }

    #[test]
    fn the_confirmation_key_binds_exporter_certificates_and_messages() {
        let key = [7; 32];
        let exporter = [1; EXPORTER_BYTES];
        let a = CertificateFingerprint::from_certificate_der(b"a");
        let b = CertificateFingerprint::from_certificate_der(b"b");
        let messages = ([2; PAKE_MESSAGE_BYTES], [3; PAKE_MESSAGE_BYTES]);
        fn tag(
            key: &[u8],
            exporter: &[u8; EXPORTER_BYTES],
            fingerprints: [CertificateFingerprint; 2],
            showing: &[u8; PAKE_MESSAGE_BYTES],
            entering: &[u8; PAKE_MESSAGE_BYTES],
        ) -> Vec<u8> {
            let key = confirmation_key(key, exporter, fingerprints, [showing, entering]).unwrap();
            hmac::sign(&key, SHOWING_TAG).as_ref().to_vec()
        }
        let base = tag(&key, &exporter, [a, b], &messages.0, &messages.1);
        assert_eq!(base, tag(&key, &exporter, [a, b], &messages.0, &messages.1));
        assert_eq!(base.len(), MAC_BYTES);
        for other in [
            tag(&[8; 32], &exporter, [a, b], &messages.0, &messages.1),
            tag(&key, &[2; EXPORTER_BYTES], [a, b], &messages.0, &messages.1),
            tag(&key, &exporter, [b, a], &messages.0, &messages.1),
            tag(&key, &exporter, [a, a], &messages.0, &messages.1),
            tag(&key, &exporter, [a, b], &messages.1, &messages.0),
        ] {
            assert_ne!(base, other);
        }
    }
}
