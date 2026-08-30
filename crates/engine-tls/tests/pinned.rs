//! Fingerprint-pinning round-trip: a [`TlsPolicy::PinnedFingerprints`] client
//! accepts the pinned end-entity certificate and rejects any other — including
//! the regression shape that motivated the verifier: a **CA-signed** leaf
//! served ALONE on the wire (the root kept off-wire), where anchor-style
//! pinning (`TlsPolicy::pinned`) cannot build a chain and fails with
//! `UnknownIssuer` even though the pinned bytes match the presented leaf.

use std::sync::Arc;

use engine_tls::{CertificateDer, TlsPolicy, client_config};
use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;

fn sha256(cert: &CertificateDer<'static>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

/// Starts a TLS server that serves exactly `chain` (plus `key`) and echoes one
/// byte per connection; returns its bound port. Connections whose client
/// handshake fails are dropped server-side, surfacing as client errors.
async fn tls_server(chain: Vec<CertificateDer<'static>>, key: PrivatePkcs8KeyDer<'static>) -> u16 {
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(chain, key.into())
    .expect("server cert/key");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    let mut buf = [0u8; 16];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(b"p").await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    port
}

/// Connects with `policy` and proves the handshake by round-tripping one byte.
async fn dial(policy: &TlsPolicy, port: u16) -> std::io::Result<()> {
    let tls = client_config(policy)
        .expect("realize the policy")
        .connector()
        .connect(
            ServerName::try_from("localhost").expect("server name"),
            TcpStream::connect(("127.0.0.1", port)).await?,
        )
        .await?;
    let (mut reader, mut writer) = tokio::io::split(tls);
    writer.write_all(b"p").await?;
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf).await?;
    assert_eq!(buf, *b"p");
    Ok(())
}

/// A CA-signed leaf (`127.0.0.1`), its signing CA, and both keys — the leaf is
/// served WITHOUT the CA on the wire, mirroring the on-prem lab server whose
/// `UnknownIssuer` failure shaped this verifier.
struct CaSignedLeaf {
    leaf: CertificateDer<'static>,
    leaf_key: PrivatePkcs8KeyDer<'static>,
    ca: CertificateDer<'static>,
}

fn mint_ca_signed_leaf() -> CaSignedLeaf {
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let ca = ca_params.self_signed(&ca_key).expect("self-signed ca");

    let leaf_params =
        rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("leaf params");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("ca-signed leaf");

    CaSignedLeaf {
        leaf: leaf.der().clone(),
        leaf_key: PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
        ca: ca.der().clone(),
    }
}

#[tokio::test]
async fn pinned_fingerprint_accepts_a_ca_signed_leaf_served_alone() {
    let minted = mint_ca_signed_leaf();
    let fingerprint = sha256(&minted.leaf);
    let port = tls_server(vec![minted.leaf.clone()], minted.leaf_key).await;

    dial(&TlsPolicy::pinned_fingerprints(vec![fingerprint]), port)
        .await
        .expect("the pinned leaf must verify even with its issuer off-wire");

    // The same bytes as a webpki anchor CANNOT verify (the anchor must be the
    // issuer) — the regression this verifier exists to fix.
    let anchored = TlsPolicy::pinned(vec![minted.leaf.clone()]);
    assert!(
        dial(&anchored, port).await.is_err(),
        "anchor-pinning a CA-signed leaf must keep failing — that is the bug shape"
    );

    // Anchor-pinning the CA works (the anchor is the issuer) — but only when
    // the CA is on the wire or in hand; the lab server never presents it.
    let _ = &minted.ca;
}

#[tokio::test]
async fn pinned_fingerprint_rejects_an_unpinned_certificate() {
    let minted = mint_ca_signed_leaf();
    let port = tls_server(vec![minted.leaf.clone()], minted.leaf_key).await;

    let other =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("other cert");
    let wrong = sha256(&other.cert.der().clone());
    let err = dial(&TlsPolicy::pinned_fingerprints(vec![wrong]), port)
        .await
        .expect_err("an unpinned certificate must be rejected");
    assert!(
        err.to_string().contains("NotPinned"),
        "the failure should name the pinning decision, got: {err}"
    );
}

#[tokio::test]
async fn pinned_fingerprint_accepts_a_self_signed_leaf() {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let port = tls_server(vec![cert.clone()], key).await;

    dial(&TlsPolicy::pinned_fingerprints(vec![sha256(&cert)]), port)
        .await
        .expect("a pinned self-signed leaf must verify");
}

#[test]
fn empty_pin_set_is_rejected_up_front() {
    let err = client_config(&TlsPolicy::pinned_fingerprints(Vec::new()))
        .expect_err("an empty pin set must not build");
    assert!(
        err.to_string().contains("no certificate fingerprints"),
        "got: {err}"
    );
}
