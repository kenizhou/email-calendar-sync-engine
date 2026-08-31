//! Trust-on-first-use certificate pinning by end-entity fingerprint.
//!
//! [`TlsPolicy::Roots`](crate::TlsPolicy::Roots) treats its certificates as
//! webpki trust **anchors** — they validate only as the *issuer* of the
//! server's certificate. Pinning a CA root (or a presented intermediate)
//! works there, but pinning a CA-signed **end-entity** certificate does not:
//! webpki builds no chain from the leaf to itself, so the handshake fails
//! with `UnknownIssuer`. Lab and on-prem servers routinely present a lone
//! CA-signed leaf with the root kept off-wire, which makes anchor-pinning
//! unusable exactly where pinning is most needed.
//!
//! This verifier is the answer: accept iff the presented end-entity
//! certificate's SHA-256 fingerprint is in the pinned set (the SSH/TOFU
//! model). The fingerprint IS the identity check, so no chain building or
//! DNS-name matching applies; handshake signatures are still verified against
//! the crypto provider, so the TLS handshake itself stays well-formed.
//! Certificate rotation invalidates the pin — the host's re-pin flow is the
//! intended recovery, and a rotated or substituted certificate fails loudly
//! rather than silently re-trusting.

use std::sync::Arc;

use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

/// The presented end-entity certificate's fingerprint was not in the pinned
/// set — surfaced as the handshake failure's root cause so logs name the
/// pinning decision rather than a generic chain error.
#[derive(Debug)]
struct NotPinned;

impl std::fmt::Display for NotPinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server's certificate fingerprint is not in the pinned set")
    }
}

impl std::error::Error for NotPinned {}

/// Accepts exactly the end-entity certificates whose SHA-256 fingerprints
/// are pinned; everything else is rejected. Handshake signatures are still
/// verified against the provider (same discipline as the test-only
/// accept-any verifier).
#[derive(Debug)]
pub(crate) struct PinnedVerifier {
    fingerprints: Vec<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub(crate) fn new(fingerprints: Vec<[u8; 32]>, provider: Arc<CryptoProvider>) -> Self {
        Self {
            fingerprints,
            provider,
        }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if self.fingerprints.contains(&presented) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(rustls::OtherError(Arc::new(NotPinned))),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Builds a client config that accepts only the pinned fingerprints.
pub(crate) fn pinned_config(
    fingerprints: Vec<[u8; 32]>,
    provider: Arc<CryptoProvider>,
    builder: rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier>,
) -> ClientConfig {
    builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier::new(fingerprints, provider)))
        .with_no_client_auth()
}
