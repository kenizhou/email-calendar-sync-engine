//! The IMAP half of the warm tests, split from `warm_tests.rs` so each file stays
//! under the 500-line ceiling: the pure pipeline functions (UID-set assembly,
//! mailbox/validity grouping, UID→key fan-out) and the wire test — the real
//! `ImapProvider::connect` against an in-process TLS IMAP server, counting every
//! `UID FETCH` command a batch costs.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use engine_core::{error::FailureClass, ids::MailboxId, raw::RawMime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::{
    warm_tests::{account, messages_for, warm_pairs},
    *,
};

#[test]
fn uid_set_compresses_sorted_uids_into_runs() {
    // However the batch ordered them, the command lists them ascending, runs
    // compressed — the one pipelined `UID FETCH` set.
    assert_eq!(uid_set(&[43, 41, 42]), "41:43");
    assert_eq!(uid_set(&[1, 3, 5]), "1,3,5");
    assert_eq!(uid_set(&[1, 2, 3, 7, 9, 10]), "1:3,7,9:10");
    assert_eq!(uid_set(&[5, 5]), "5");
}

#[test]
fn group_imap_batch_groups_by_mailbox_and_validity_and_rejects_foreign_keys() {
    let owned = messages_for(&[
        "imap:v7:u42@INBOX",
        "jmap:Mxyz",
        "imap:v7:u41@INBOX",
        "imap:v9:u5@INBOX",
        "imap:v7:u9@Archive",
    ]);
    let batch = warm_pairs(&owned);

    let (groups, rejects) = group_imap_batch(&batch);

    // (mailbox, UIDVALIDITY) is the group key: two INBOX generations split, and
    // Archive is its own command; a foreign key never reaches the wire.
    assert_eq!(groups.len(), 3, "{groups:?}");
    assert_eq!(groups[0].mailbox, "INBOX");
    assert_eq!(groups[0].uid_validity, 7);
    assert_eq!(
        groups[0]
            .items
            .iter()
            .map(|(_, _, uid)| *uid)
            .collect::<Vec<_>>(),
        vec![42, 41],
        "items keep batch order within the group"
    );
    assert_eq!(rejects.len(), 1);
    assert_eq!(rejects[0].0.as_str(), "jmap:Mxyz");
    assert_eq!(
        rejects[0].1.class(),
        FailureClass::InvalidState,
        "a non-IMAP key is refused before any wire traffic"
    );
}

#[test]
fn fan_out_group_maps_uid_outcomes_onto_keys_in_item_order() {
    use std::collections::HashMap;
    let owned = messages_for(&["imap:v7:u41@INBOX", "imap:v7:u42@INBOX"]);
    let batch = warm_pairs(&owned);
    let items = batch
        .iter()
        .map(|(key, message)| (*key, *message, parse_imap_key(key).unwrap().2))
        .collect();
    let group = MailboxGroup {
        mailbox: "INBOX".to_owned(),
        uid_validity: 7,
        items,
    };

    // Wire order is UID order, not batch order; a UID the server never answered
    // (expunged mid-batch) reads as a Conflict, like the single-fetch path.
    let mut by_uid = HashMap::new();
    by_uid.insert(42u32, Ok(RawMime::new(b"second".to_vec())));
    let out = fan_out_group(&group, &mut by_uid);

    assert_eq!(out[0].0.as_str(), "imap:v7:u41@INBOX");
    assert_eq!(
        out[0].1.as_ref().unwrap_err().class(),
        FailureClass::Conflict
    );
    assert_eq!(out[1].0.as_str(), "imap:v7:u42@INBOX");
    assert_eq!(out[1].1.as_ref().unwrap().as_bytes(), b"second");
}

/// Stands up an in-process implicit-TLS IMAP server that answers the full connect
/// (greeting, LOGIN, CAPABILITY) and then serves `UID FETCH <uid> (BODY.PEEK[])`
/// from a UID-shaped body — the `tls_info` harness shape, counting every `UID
/// FETCH` command it receives.
async fn imap_body_server() -> (engine_tls::CertificateDer<'static>, u16, Arc<AtomicUsize>) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
        generated.key_pair.serialize_der(),
    );
    let server_config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(tokio_rustls::rustls::DEFAULT_VERSIONS)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("server cert/key");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let fetches = Arc::new(AtomicUsize::new(0));
    let counted = fetches.clone();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut stream = tokio::io::BufReader::new(acceptor.accept(tcp).await.expect("handshake"));
        stream
            .write_all(b"* OK [CAPABILITY IMAP4rev1] ready\r\n")
            .await
            .expect("greeting");
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await.expect("read") == 0 {
                break;
            }
            let tag = line.split_whitespace().next().expect("tag").to_owned();
            let reply = if line.contains("LOGIN") {
                format!("{tag} OK LOGIN completed\r\n")
            } else if line.contains("CAPABILITY") {
                format!("* CAPABILITY IMAP4rev1\r\n{tag} OK done\r\n")
            } else if line.contains("EXAMINE") {
                format!("* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\n{tag} OK [READ-ONLY] done\r\n")
            } else if line.contains("UID FETCH") {
                counted.fetch_add(1, Ordering::SeqCst);
                let uid: u32 = line
                    .split_whitespace()
                    .nth(3)
                    .and_then(|token| token.parse().ok())
                    .expect("uid in the command");
                let body = format!("From: a@b\r\n\r\nbody-{uid}\r\n");
                format!(
                    "* 3 FETCH (UID {uid} BODY[] {{{}}}\r\n{body})\r\n{tag} OK FETCH completed\r\n",
                    body.len()
                )
            } else {
                format!("{tag} OK done\r\n")
            };
            stream.write_all(reply.as_bytes()).await.expect("reply");
        }
    });
    (cert, port, fetches)
}

#[tokio::test]
async fn imap_batch_fetch_serves_three_messages_over_one_session() {
    let (cert, port, fetches) = imap_body_server().await;
    let connector = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert]))
        .expect("client config")
        .connector();
    let config = provider_imap::ImapConfig::new(format!("127.0.0.1:{port}"), "127.0.0.1", "u", "p");
    let provider = provider_imap::ImapProvider::connect(
        &config,
        connector,
        MailboxId::try_from("INBOX").unwrap(),
    )
    .await
    .expect("connect");

    let owned = messages_for(&[
        "imap:v7:u41@INBOX",
        "imap:v7:u42@INBOX",
        "imap:v7:u43@INBOX",
    ]);
    let batch = warm_pairs(&owned);
    let out = BatchSourceFetch::fetch_message_sources(&provider, &account(), &batch).await;

    assert_eq!(out.len(), 3);
    for (key, result) in &out {
        let uid = parse_imap_key(key).unwrap().2;
        let raw = result.as_ref().expect("fetched");
        assert!(
            String::from_utf8_lossy(raw.as_bytes()).contains(&format!("body-{uid}")),
            "each key carries its own UID's body"
        );
    }
    // The wire count a batch of three costs today: one `UID FETCH` per item over
    // the one session (see the impl's docs — provider-imap's session is private,
    // so the assembled `uid_set` command cannot be sent as one round trip yet;
    // this count is pinned so the collapse to 1 lands consciously).
    assert_eq!(fetches.load(Ordering::SeqCst), 3);
    // …and the single command those three UIDs form is exactly this set.
    assert_eq!(uid_set(&[43, 41, 42]), "41:43");
}
