//! Paired-computer records, the Saved frame and the match badge. The code confirmation that
//! establishes a pairing is in `pairing_exchange`.

use std::{fmt, net::SocketAddrV4};

use sha2::{Digest, Sha256};

use crate::{
    crypto::{CertificateFingerprint, VerifiedPeer},
    policy::is_private_or_link_local,
};
use monhop_core::Platform;

pub const PAIRING_PORT: u16 = 24872;
pub const MAX_PAIRING_CERT_BYTES: usize = 3072;
pub const MAX_PEER_RECORD_BYTES: usize = 4096;
pub const PAIR_FRAME_BYTES: usize = 70;
pub const PAIR_BADGE_COLORS: u8 = 8;
pub const PAIR_BADGE_SYMBOLS: u8 = 32;
const PAIR_BADGE_DOMAIN: &[u8] = b"monhop pair badge v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingError {
    InvalidAddress,
    InvalidCertificate,
    InvalidRecord,
    WrongLocalIdentity,
    SameIdentity,
    InvalidMessage,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidAddress => "The other computer's address is not a private address on this network. Nothing was saved.",
            Self::InvalidCertificate => "The other computer presented an invalid identity. Nothing was saved.",
            Self::InvalidRecord => "The saved pairing is invalid. It was not replaced. Forget it explicitly to pair again.",
            Self::WrongLocalIdentity => "The saved pairing belongs to a different local identity. It was not replaced.",
            Self::SameIdentity => "The other computer presented this computer's own identity. Nothing was saved.",
            Self::InvalidMessage => "The other computer sent an invalid pairing message. Nothing was enabled.",
        })
    }
}

impl std::error::Error for PairingError {}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingOffer {
    endpoint: SocketAddrV4,
    certificate: Vec<u8>,
    // None only for records written before platforms were exchanged (LKMP v1).
    platform: Option<Platform>,
}

impl PairingOffer {
    pub fn new(endpoint: SocketAddrV4, certificate: &[u8]) -> Result<Self, PairingError> {
        if endpoint.port() != PAIRING_PORT || !is_private_or_link_local(*endpoint.ip()) {
            return Err(PairingError::InvalidAddress);
        }
        if certificate.is_empty() || certificate.len() > MAX_PAIRING_CERT_BYTES {
            return Err(PairingError::InvalidCertificate);
        }
        let fingerprint = CertificateFingerprint::from_certificate_der(certificate);
        VerifiedPeer::from_certificate_der(certificate, &fingerprint.full_hex())
            .map_err(|_| PairingError::InvalidCertificate)?;
        Ok(Self {
            endpoint,
            certificate: certificate.to_vec(),
            platform: None,
        })
    }

    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform);
        self
    }

    pub fn platform(&self) -> Option<Platform> {
        self.platform
    }

    pub fn endpoint(&self) -> SocketAddrV4 {
        self.endpoint
    }

    pub fn fingerprint(&self) -> CertificateFingerprint {
        CertificateFingerprint::from_certificate_der(&self.certificate)
    }

    pub fn verified_peer(&self) -> Result<VerifiedPeer, PairingError> {
        VerifiedPeer::from_certificate_der(&self.certificate, &self.fingerprint().full_hex())
            .map_err(|_| PairingError::InvalidCertificate)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConfirmedPeerRecord {
    local: CertificateFingerprint,
    peer: PairingOffer,
}

impl ConfirmedPeerRecord {
    pub fn new(local: CertificateFingerprint, peer: PairingOffer) -> Result<Self, PairingError> {
        if local == peer.fingerprint() {
            return Err(PairingError::SameIdentity);
        }
        Ok(Self { local, peer })
    }

    pub fn peer(&self) -> &PairingOffer {
        &self.peer
    }

    pub fn local(&self) -> CertificateFingerprint {
        self.local
    }

    // v1 records (no platform byte) stay readable so an existing pairing is never replaced.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(46 + self.peer.certificate.len());
        bytes.extend_from_slice(b"LKMP\x02");
        bytes.extend_from_slice(self.local.as_bytes());
        bytes.push(self.peer.platform.map_or(0, platform_to_wire));
        bytes.extend_from_slice(&self.peer.endpoint.ip().octets());
        bytes.extend_from_slice(&self.peer.endpoint.port().to_be_bytes());
        bytes.extend_from_slice(&(self.peer.certificate.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&self.peer.certificate);
        bytes
    }

    pub fn decode(bytes: &[u8], local: CertificateFingerprint) -> Result<Self, PairingError> {
        if bytes.len() < 46 || bytes.len() > MAX_PEER_RECORD_BYTES || &bytes[..4] != b"LKMP" {
            return Err(PairingError::InvalidRecord);
        }
        let (platform, rest) = match bytes[4] {
            1 => (None, &bytes[37..]),
            2 => (
                platform_from_wire(*bytes.get(37).ok_or(PairingError::InvalidRecord)?)?,
                &bytes[38..],
            ),
            _ => return Err(PairingError::InvalidRecord),
        };
        if &bytes[5..37] != local.as_bytes() {
            return Err(PairingError::WrongLocalIdentity);
        }
        if rest.len() < 9 {
            return Err(PairingError::InvalidRecord);
        }
        let ip = [rest[0], rest[1], rest[2], rest[3]].into();
        let port = u16::from_be_bytes([rest[4], rest[5]]);
        let length = usize::from(u16::from_be_bytes([rest[6], rest[7]]));
        if length != rest.len() - 8 {
            return Err(PairingError::InvalidRecord);
        }
        let peer = PairingOffer::new(SocketAddrV4::new(ip, port), &rest[8..])
            .map_err(|_| PairingError::InvalidRecord)?;
        Self::new(
            local,
            match platform {
                Some(platform) => peer.with_platform(platform),
                None => peer,
            },
        )
    }
}

/// Which computer opens the connection. Windows dials macOS; identical platforms fall back to
/// fingerprint order so exactly one side dials. A peer without a known platform is assumed to be
/// the other platform (version 1 records predate same-platform pairing).
pub fn initiates_connection(
    local_platform: Platform,
    local: CertificateFingerprint,
    peer_platform: Option<Platform>,
    peer: CertificateFingerprint,
) -> bool {
    let peer_platform = peer_platform.unwrap_or(opposite_platform(local_platform));
    match (local_platform, peer_platform) {
        (Platform::Windows, Platform::MacOs) => true,
        (Platform::MacOs, Platform::Windows) => false,
        _ => local.as_bytes() < peer.as_bytes(),
    }
}

pub fn opposite_platform(platform: Platform) -> Platform {
    match platform {
        Platform::Windows => Platform::MacOs,
        Platform::MacOs => Platform::Windows,
    }
}

/// The platform byte of saved records and pairing endpoint frames.
pub(crate) fn platform_to_wire(platform: Platform) -> u8 {
    match platform {
        Platform::Windows => 1,
        Platform::MacOs => 2,
    }
}

pub(crate) fn platform_from_wire(value: u8) -> Result<Option<Platform>, PairingError> {
    match value {
        0 => Ok(None),
        1 => Ok(Some(Platform::Windows)),
        2 => Ok(Some(Platform::MacOs)),
        _ => Err(PairingError::InvalidRecord),
    }
}

/// The same picture on both computers' screens after pairing. Reassurance only: the code
/// confirmation is what authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairBadge {
    pub color: u8,
    pub symbols: [u8; 3],
}

/// Symmetric in its arguments; the three symbols always differ.
pub fn pair_badge(a: CertificateFingerprint, b: CertificateFingerprint) -> PairBadge {
    let (low, high) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    let mut input = [0; PAIR_BADGE_DOMAIN.len() + 64];
    input[..PAIR_BADGE_DOMAIN.len()].copy_from_slice(PAIR_BADGE_DOMAIN);
    input[PAIR_BADGE_DOMAIN.len()..][..32].copy_from_slice(low.as_bytes());
    input[PAIR_BADGE_DOMAIN.len() + 32..].copy_from_slice(high.as_bytes());
    let hash = Sha256::digest(input);
    let mut symbols = [0; 3];
    for index in 0..symbols.len() {
        let mut symbol = hash[index + 1] % PAIR_BADGE_SYMBOLS;
        while symbols[..index].contains(&symbol) {
            symbol = (symbol + 1) % PAIR_BADGE_SYMBOLS;
        }
        symbols[index] = symbol;
    }
    PairBadge {
        color: hash[0] % PAIR_BADGE_COLORS,
        symbols,
    }
}

#[derive(Clone, Copy)]
pub enum PairFrameKind {
    Saved = 2,
}

pub fn encode_pair_frame(
    kind: PairFrameKind,
    local: CertificateFingerprint,
    peer: CertificateFingerprint,
) -> [u8; PAIR_FRAME_BYTES] {
    let mut frame = [0; PAIR_FRAME_BYTES];
    frame[..4].copy_from_slice(b"LKP1");
    frame[4] = 1;
    frame[5] = kind as u8;
    frame[6..38].copy_from_slice(local.as_bytes());
    frame[38..].copy_from_slice(peer.as_bytes());
    frame
}

pub fn validate_pair_frame(
    bytes: &[u8],
    kind: PairFrameKind,
    sender: CertificateFingerprint,
    intended: CertificateFingerprint,
) -> Result<(), PairingError> {
    if bytes != encode_pair_frame(kind, sender, intended) {
        return Err(PairingError::InvalidMessage);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::DeviceIdentity;

    fn offer(identity: &DeviceIdentity) -> PairingOffer {
        PairingOffer::new(
            "192.168.1.2:24872".parse().unwrap(),
            identity.certificate_der(),
        )
        .unwrap()
    }

    #[test]
    fn protected_peer_record_round_trips() {
        let local = DeviceIdentity::generate().unwrap();
        let remote = DeviceIdentity::generate().unwrap();
        let record = ConfirmedPeerRecord::new(local.fingerprint(), offer(&remote)).unwrap();
        assert!(
            ConfirmedPeerRecord::decode(&record.encode(), local.fingerprint()).unwrap() == record
        );
        assert!(record.encode().len() <= MAX_PEER_RECORD_BYTES);
    }

    #[test]
    fn platform_travels_in_the_record_and_legacy_records_stay_readable() {
        let local = DeviceIdentity::generate().unwrap();
        let remote = DeviceIdentity::generate().unwrap();
        let peer = offer(&remote).with_platform(Platform::MacOs);
        let record = ConfirmedPeerRecord::new(local.fingerprint(), peer.clone()).unwrap();
        let decoded = ConfirmedPeerRecord::decode(&record.encode(), local.fingerprint()).unwrap();
        assert_eq!(decoded.peer().platform(), Some(Platform::MacOs));
        assert!(decoded == record);

        let mut legacy = Vec::new();
        legacy.extend_from_slice(b"LKMP\x01");
        legacy.extend_from_slice(local.fingerprint().as_bytes());
        legacy.extend_from_slice(&[192, 168, 1, 2]);
        legacy.extend_from_slice(&PAIRING_PORT.to_be_bytes());
        legacy.extend_from_slice(&(remote.certificate_der().len() as u16).to_be_bytes());
        legacy.extend_from_slice(remote.certificate_der());
        let decoded = ConfirmedPeerRecord::decode(&legacy, local.fingerprint()).unwrap();
        assert_eq!(decoded.peer().platform(), None);
        assert!(decoded.peer() == &offer(&remote));
    }

    #[test]
    fn exactly_one_side_initiates_for_every_platform_pairing() {
        let a = DeviceIdentity::generate().unwrap().fingerprint();
        let b = DeviceIdentity::generate().unwrap().fingerprint();
        assert!(initiates_connection(
            Platform::Windows,
            a,
            Some(Platform::MacOs),
            b
        ));
        assert!(!initiates_connection(
            Platform::MacOs,
            b,
            Some(Platform::Windows),
            a
        ));
        assert!(initiates_connection(Platform::Windows, a, None, b));
        assert!(!initiates_connection(Platform::MacOs, a, None, b));
        for platform in [Platform::MacOs, Platform::Windows] {
            let ab = initiates_connection(platform, a, Some(platform), b);
            let ba = initiates_connection(platform, b, Some(platform), a);
            assert_ne!(ab, ba);
        }
    }

    #[test]
    fn an_offer_needs_a_private_pairing_endpoint_and_a_valid_certificate() {
        let identity = DeviceIdentity::generate().unwrap();
        for address in [
            "0.0.0.0:24872",
            "127.0.0.1:24872",
            "8.8.8.8:24872",
            "224.0.0.1:24872",
            "192.168.1.2:0",
            "192.168.1.2:24873",
        ] {
            assert!(
                PairingOffer::new(address.parse().unwrap(), identity.certificate_der()).is_err()
            );
        }
        assert!(
            PairingOffer::new(
                "192.168.1.2:24872".parse().unwrap(),
                &[1; MAX_PAIRING_CERT_BYTES + 1]
            )
            .is_err()
        );
        assert!(PairingOffer::new("192.168.1.2:24872".parse().unwrap(), b"not DER").is_err());
    }

    #[test]
    fn trust_record_rejects_self_wrong_local_and_nonexact_records() {
        let local = DeviceIdentity::generate().unwrap();
        let remote = DeviceIdentity::generate().unwrap();
        assert!(ConfirmedPeerRecord::new(local.fingerprint(), offer(&local)).is_err());
        let bytes = ConfirmedPeerRecord::new(local.fingerprint(), offer(&remote))
            .unwrap()
            .encode();
        assert!(matches!(
            ConfirmedPeerRecord::decode(&bytes, remote.fingerprint()),
            Err(PairingError::WrongLocalIdentity)
        ));
        for end in 0..bytes.len() {
            assert!(ConfirmedPeerRecord::decode(&bytes[..end], local.fingerprint()).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(ConfirmedPeerRecord::decode(&trailing, local.fingerprint()).is_err());
        let mut version = bytes;
        version[4] = 3;
        assert!(ConfirmedPeerRecord::decode(&version, local.fingerprint()).is_err());
        assert!(
            ConfirmedPeerRecord::decode(&vec![0; MAX_PEER_RECORD_BYTES + 1], local.fingerprint())
                .is_err()
        );
    }

    #[test]
    fn the_saved_frame_binds_both_complete_identities() {
        let a = CertificateFingerprint::from_certificate_der(b"a");
        let b = CertificateFingerprint::from_certificate_der(b"b");
        let message = encode_pair_frame(PairFrameKind::Saved, a, b);
        assert!(validate_pair_frame(&message, PairFrameKind::Saved, a, b).is_ok());
        assert!(validate_pair_frame(&message, PairFrameKind::Saved, b, a).is_err());
        for index in 0..message.len() {
            let mut bad = message;
            bad[index] ^= 1;
            assert!(validate_pair_frame(&bad, PairFrameKind::Saved, a, b).is_err());
        }
        assert!(validate_pair_frame(&message[..69], PairFrameKind::Saved, a, b).is_err());
    }

    #[test]
    fn the_badge_is_symmetric_deterministic_and_in_range_with_distinct_symbols() {
        let mut colors = [false; PAIR_BADGE_COLORS as usize];
        for index in 0_u16..512 {
            let a = CertificateFingerprint::from_certificate_der(&index.to_be_bytes());
            let b = CertificateFingerprint::from_certificate_der(
                &[index.to_le_bytes(), [9, 9]].concat(),
            );
            let badge = pair_badge(a, b);
            assert_eq!(badge, pair_badge(b, a));
            assert_eq!(badge, pair_badge(a, b));
            assert!(badge.color < PAIR_BADGE_COLORS);
            colors[usize::from(badge.color)] = true;
            assert!(
                badge
                    .symbols
                    .iter()
                    .all(|symbol| *symbol < PAIR_BADGE_SYMBOLS)
            );
            let [first, second, third] = badge.symbols;
            assert!(
                first != second && second != third && first != third,
                "{badge:?}"
            );
        }
        assert!(colors.iter().all(|seen| *seen), "every color occurs");
        let a = CertificateFingerprint::from_certificate_der(b"a");
        let b = CertificateFingerprint::from_certificate_der(b"b");
        let c = CertificateFingerprint::from_certificate_der(b"c");
        assert_ne!(pair_badge(a, b), pair_badge(a, c));
    }
}
