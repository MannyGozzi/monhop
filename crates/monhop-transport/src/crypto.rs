//! Mutual TLS configuration for explicitly paired MonHop devices.
//!
//! Pairing must compare [`CertificateFingerprint`] values out of band on both
//! physical machines. A pin is never learned from a connection attempt.

use std::{fmt, sync::Arc, time::Duration};

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use rustls::{
    CertificateError, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::{WebPkiClientVerifier, danger::ClientCertVerifier},
    sign::{CertifiedKey, SingleCertAndKey},
};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

/// Fixed logical TLS name carried in every MonHop certificate SAN.
///
/// This name is only checked inside TLS. It does not cause DNS resolution or
/// any network lookup.
pub const LOCAL_TLS_SERVER_NAME: &str = "monhop.invalid";

/// The one ALPN protocol accepted by MonHop transport.
pub const LOCAL_ALPN: &[u8] = b"monhop/1";

/// Certificate DER is public but still bounded before parsing or pinning.
pub const MAX_CERTIFICATE_DER_LENGTH: usize = 16 * 1024;

/// MonHop identity keys are P-256 PKCS#8 documents and must remain small.
pub const MAX_PRIVATE_KEY_DER_LENGTH: usize = 4 * 1024;

const MAX_BIDIRECTIONAL_STREAMS: u32 = 4;
const MAX_UNIDIRECTIONAL_STREAMS: u32 = 0;
const STREAM_RECEIVE_WINDOW: u32 = 16 * 1024;
const CONNECTION_RECEIVE_WINDOW: u32 = 64 * 1024;
const SEND_WINDOW: u64 = 64 * 1024;
const DATAGRAM_BUFFER_SIZE: usize = 8 * 1024;
const CRYPTO_BUFFER_SIZE: usize = 8 * 1024;
const MAX_IDLE_TIMEOUT_MILLIS: u32 = 10_000;
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(3);
const MAX_PENDING_INCOMING: usize = 4;
const INCOMING_BUFFER_SIZE: u64 = 32 * 1024;
const TOTAL_INCOMING_BUFFER_SIZE: u64 = 128 * 1024;

/// Errors intentionally identify a failed security property without exposing
/// certificates, private keys, packet bytes, or other sensitive material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CryptoError {
    EmptyCertificate,
    CertificateTooLarge,
    EmptyPrivateKey,
    PrivateKeyTooLarge,
    InvalidPrivateKey,
    InvalidCertificate,
    KeyCertificateMismatch,
    InvalidFingerprint,
    FingerprintMismatch,
    RootStore,
    CertificateVerifier,
    TlsConfiguration,
    QuicConfiguration,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyCertificate => "certificate DER must not be empty",
            Self::CertificateTooLarge => "certificate DER exceeds the MonHop limit",
            Self::EmptyPrivateKey => "PKCS#8 private key DER must not be empty",
            Self::PrivateKeyTooLarge => "PKCS#8 private key DER exceeds the MonHop limit",
            Self::InvalidPrivateKey => "private key is not a supported P-256 PKCS#8 key",
            Self::InvalidCertificate => "certificate DER is invalid",
            Self::KeyCertificateMismatch => "private key does not match the certificate",
            Self::InvalidFingerprint => {
                "fingerprint must contain exactly 64 hexadecimal characters"
            }
            Self::FingerprintMismatch => "certificate does not match the expected full fingerprint",
            Self::RootStore => "certificate cannot be used as a pinned trust anchor",
            Self::CertificateVerifier => "pinned certificate verifier could not be configured",
            Self::TlsConfiguration => "TLS configuration could not be created",
            Self::QuicConfiguration => "QUIC configuration could not be created",
        })
    }
}

impl std::error::Error for CryptoError {}

/// A complete SHA-256 certificate fingerprint.
///
/// [`fmt::Display`] renders the value as uppercase colon-separated bytes for
/// humans. [`CertificateFingerprint::parse_full`] accepts only the complete,
/// unseparated 64-hex-character representation used for pairing input.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CertificateFingerprint([u8; 32]);

impl CertificateFingerprint {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hash complete certificate DER with SHA-256.
    pub fn from_certificate_der(certificate_der: &[u8]) -> Self {
        Self(Sha256::digest(certificate_der).into())
    }

    /// Parse exactly 32 bytes represented as 64 ASCII hexadecimal characters.
    pub fn parse_full(value: &str) -> Result<Self, CryptoError> {
        let bytes = value.as_bytes();
        if bytes.len() != 64 {
            return Err(CryptoError::InvalidFingerprint);
        }

        let mut fingerprint = [0_u8; 32];
        for (index, byte) in fingerprint.iter_mut().enumerate() {
            let high = hex_nibble(bytes[index * 2]).ok_or(CryptoError::InvalidFingerprint)?;
            let low = hex_nibble(bytes[index * 2 + 1]).ok_or(CryptoError::InvalidFingerprint)?;
            *byte = (high << 4) | low;
        }
        Ok(Self(fingerprint))
    }

    /// Return the complete, unseparated uppercase hexadecimal fingerprint.
    pub fn full_hex(self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(value, "{byte:02X}");
        }
        value
    }
}

impl fmt::Display for CertificateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// A local P-256 identity held only in memory.
///
/// The private key does not implement `Debug`, and callers can obtain only the
/// public certificate and its fingerprint. Persisting it requires an explicit
/// platform-protected storage layer outside this module.
pub struct DeviceIdentity {
    certificate_der: Vec<u8>,
    private_key_der: Zeroizing<Vec<u8>>,
}

impl DeviceIdentity {
    /// Generate a fresh P-256 self-signed identity for MonHop.
    pub fn generate() -> Result<Self, CryptoError> {
        let key_pair = ZeroizingKeyPair::generate()?;
        let mut params = CertificateParams::new(vec![LOCAL_TLS_SERVER_NAME.to_owned()])
            .map_err(|_| CryptoError::InvalidCertificate)?;
        params.is_ca = IsCa::ExplicitNoCa;
        params
            .distinguished_name
            .push(DnType::CommonName, LOCAL_TLS_SERVER_NAME);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];

        let certificate = params
            .self_signed(key_pair.as_ref())
            .map_err(|_| CryptoError::InvalidCertificate)?;
        let certificate_der = certificate.der().to_vec();
        let private_key_der = Zeroizing::new(key_pair.serialize_der());

        Self::from_pkcs8_and_certificate(private_key_der.as_slice(), &certificate_der)
    }

    /// Import a bounded P-256 PKCS#8 key and its matching self-signed certificate.
    ///
    /// This operation keeps temporary private-key copies zeroized and rejects a
    /// mismatched public key before the identity can be used in a TLS config.
    pub fn from_pkcs8_and_certificate(
        private_key_der: &[u8],
        certificate_der: &[u8],
    ) -> Result<Self, CryptoError> {
        validate_private_key_length(private_key_der)?;
        validate_certificate_length(certificate_der)?;

        let identity = Self {
            certificate_der: certificate_der.to_vec(),
            private_key_der: Zeroizing::new(private_key_der.to_vec()),
        };
        identity.certified_key()?;
        Ok(identity)
    }

    /// Return the public, DER-encoded identity certificate.
    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    /// Return the full fingerprint that must be compared out of band at both machines.
    pub fn fingerprint(&self) -> CertificateFingerprint {
        CertificateFingerprint::from_certificate_der(&self.certificate_der)
    }

    fn certificate(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.certificate_der.clone())
    }

    fn certified_key(&self) -> Result<Arc<CertifiedKey>, CryptoError> {
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.private_key_der.as_slice()));
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key)
            .map_err(|_| CryptoError::InvalidPrivateKey)?;
        if signing_key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .is_none()
        {
            return Err(CryptoError::InvalidPrivateKey);
        }

        let certified_key = CertifiedKey::new(vec![self.certificate()], signing_key);
        certified_key.keys_match().map_err(map_rustls_error)?;
        Ok(Arc::new(certified_key))
    }
}

pub(crate) fn private_key_der_for_protected_storage(identity: &DeviceIdentity) -> &[u8] {
    identity.private_key_der.as_slice()
}

/// A peer certificate explicitly paired by full out-of-band fingerprint.
///
/// No constructor accepts a fingerprint alone, a certificate alone, or a
/// connection result. Pairing must supply both values and they must agree.
#[derive(Clone)]
pub struct VerifiedPeer {
    certificate_der: Vec<u8>,
    fingerprint: CertificateFingerprint,
}

impl VerifiedPeer {
    /// Pin peer DER only when it matches the expected complete fingerprint.
    pub fn from_certificate_der(
        peer_certificate_der: &[u8],
        expected_full_fingerprint: &str,
    ) -> Result<Self, CryptoError> {
        validate_certificate_length(peer_certificate_der)?;
        let expected = CertificateFingerprint::parse_full(expected_full_fingerprint)?;
        let actual = CertificateFingerprint::from_certificate_der(peer_certificate_der);
        if actual != expected {
            return Err(CryptoError::FingerprintMismatch);
        }

        let certificate = CertificateDer::from(peer_certificate_der.to_vec());
        RootCertStore::empty()
            .add(certificate)
            .map_err(|_| CryptoError::InvalidCertificate)?;

        Ok(Self {
            certificate_der: peer_certificate_der.to_vec(),
            fingerprint: actual,
        })
    }

    /// Return the explicitly verified full peer fingerprint.
    pub fn fingerprint(&self) -> CertificateFingerprint {
        self.fingerprint
    }

    fn certificate(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.certificate_der.clone())
    }
}

/// Builds fail-closed QUIC configurations for one explicitly paired peer.
///
/// The caller must separately bind sockets to the selected physical interface
/// and numeric peer address. These builders do not bind, connect, resolve DNS,
/// fetch certificates, persist trust, or enable input forwarding.
pub struct SecureQuicConfig;

impl SecureQuicConfig {
    /// Build a TLS 1.3 client config that presents `identity` and only accepts
    /// the exact `verified_server` certificate.
    pub fn client(
        identity: &DeviceIdentity,
        verified_server: &VerifiedPeer,
    ) -> Result<quinn::ClientConfig, CryptoError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedServerVerifier::new(verified_server, &provider)?);
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| CryptoError::TlsConfiguration)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(identity.certified_key()?)));
        tls.alpn_protocols = vec![LOCAL_ALPN.to_vec()];
        tls.enable_early_data = false;
        tls.resumption = rustls::client::Resumption::disabled();

        let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls))
            .map_err(|_| CryptoError::QuicConfiguration)?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic_tls));
        config.transport_config(transport_config());
        config.token_store(Arc::new(quinn::NoneTokenStore));
        Ok(config)
    }

    /// Build a TLS 1.3 server config that requires a client certificate and
    /// only accepts the exact `verified_client` certificate.
    pub fn server(
        identity: &DeviceIdentity,
        verified_client: &VerifiedPeer,
    ) -> Result<quinn::ServerConfig, CryptoError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedClientVerifier::new(verified_client, &provider)?);
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| CryptoError::TlsConfiguration)?
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(Arc::new(SingleCertAndKey::from(identity.certified_key()?)));
        tls.alpn_protocols = vec![LOCAL_ALPN.to_vec()];
        tls.max_early_data_size = 0;
        tls.send_tls13_tickets = 0;
        tls.max_tls13_tickets = 0;

        let quic_tls = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
            .map_err(|_| CryptoError::QuicConfiguration)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_tls));
        config.transport_config(transport_config());
        config.migration(false);
        config.max_incoming(MAX_PENDING_INCOMING);
        config.incoming_buffer_size(INCOMING_BUFFER_SIZE);
        config.incoming_buffer_size_total(TOTAL_INCOMING_BUFFER_SIZE);
        Ok(config)
    }
}

struct PinnedServerVerifier {
    expected_certificate: Vec<u8>,
    inner: Arc<WebPkiServerVerifier>,
}

impl PinnedServerVerifier {
    fn new(
        verified_peer: &VerifiedPeer,
        provider: &Arc<rustls::crypto::CryptoProvider>,
    ) -> Result<Self, CryptoError> {
        let roots = peer_root_store(verified_peer)?;
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|_| CryptoError::CertificateVerifier)?;
        Ok(Self {
            expected_certificate: verified_peer.certificate_der.clone(),
            inner,
        })
    }
}

impl fmt::Debug for PinnedServerVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PinnedServerVerifier")
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, RustlsError> {
        verify_exact_leaf(end_entity, intermediates, &self.expected_certificate)?;
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, RustlsError> {
        self.inner
            .verify_tls12_signature(message, certificate, signed)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, RustlsError> {
        self.inner
            .verify_tls13_signature(message, certificate, signed)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

struct PinnedClientVerifier {
    expected_certificate: Vec<u8>,
    inner: Arc<dyn ClientCertVerifier>,
}

impl PinnedClientVerifier {
    fn new(
        verified_peer: &VerifiedPeer,
        provider: &Arc<rustls::crypto::CryptoProvider>,
    ) -> Result<Self, CryptoError> {
        let roots = peer_root_store(verified_peer)?;
        let inner = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|_| CryptoError::CertificateVerifier)?;
        Ok(Self {
            expected_certificate: verified_peer.certificate_der.clone(),
            inner,
        })
    }
}

impl fmt::Debug for PinnedClientVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PinnedClientVerifier")
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, RustlsError> {
        verify_exact_leaf(end_entity, intermediates, &self.expected_certificate)?;
        self.inner
            .verify_client_cert(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner
            .verify_tls12_signature(message, certificate, signed)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner
            .verify_tls13_signature(message, certificate, signed)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn validate_certificate_length(certificate_der: &[u8]) -> Result<(), CryptoError> {
    if certificate_der.is_empty() {
        Err(CryptoError::EmptyCertificate)
    } else if certificate_der.len() > MAX_CERTIFICATE_DER_LENGTH {
        Err(CryptoError::CertificateTooLarge)
    } else {
        Ok(())
    }
}

fn validate_private_key_length(private_key_der: &[u8]) -> Result<(), CryptoError> {
    if private_key_der.is_empty() {
        Err(CryptoError::EmptyPrivateKey)
    } else if private_key_der.len() > MAX_PRIVATE_KEY_DER_LENGTH {
        Err(CryptoError::PrivateKeyTooLarge)
    } else {
        Ok(())
    }
}

fn peer_root_store(verified_peer: &VerifiedPeer) -> Result<RootCertStore, CryptoError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(verified_peer.certificate())
        .map_err(|_| CryptoError::RootStore)?;
    Ok(roots)
}

fn verify_exact_leaf(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    expected_certificate: &[u8],
) -> Result<(), RustlsError> {
    if !intermediates.is_empty() || end_entity.as_ref() != expected_certificate {
        return Err(RustlsError::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ));
    }
    Ok(())
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams(quinn::VarInt::from_u32(MAX_BIDIRECTIONAL_STREAMS));
    config.max_concurrent_uni_streams(quinn::VarInt::from_u32(MAX_UNIDIRECTIONAL_STREAMS));
    config.max_idle_timeout(Some(
        quinn::VarInt::from_u32(MAX_IDLE_TIMEOUT_MILLIS).into(),
    ));
    config.stream_receive_window(quinn::VarInt::from_u32(STREAM_RECEIVE_WINDOW));
    config.receive_window(quinn::VarInt::from_u32(CONNECTION_RECEIVE_WINDOW));
    config.send_window(SEND_WINDOW);
    config.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    config.crypto_buffer_size(CRYPTO_BUFFER_SIZE);
    config.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_SIZE));
    config.datagram_send_buffer_size(DATAGRAM_BUFFER_SIZE);
    config.mtu_discovery_config(None);
    config.allow_spin(false);
    Arc::new(config)
}

fn map_rustls_error(error: RustlsError) -> CryptoError {
    match error {
        RustlsError::InconsistentKeys(_) => CryptoError::KeyCertificateMismatch,
        RustlsError::NoCertificatesPresented | RustlsError::InvalidCertificate(_) => {
            CryptoError::InvalidCertificate
        }
        _ => CryptoError::TlsConfiguration,
    }
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

struct ZeroizingKeyPair(KeyPair);

impl ZeroizingKeyPair {
    fn generate() -> Result<Self, CryptoError> {
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map(Self)
            .map_err(|_| CryptoError::InvalidPrivateKey)
    }

    fn as_ref(&self) -> &KeyPair {
        &self.0
    }

    fn serialize_der(&self) -> Vec<u8> {
        self.0.serialize_der()
    }
}

impl Drop for ZeroizingKeyPair {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
