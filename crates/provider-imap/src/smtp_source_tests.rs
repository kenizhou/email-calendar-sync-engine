//! Offline tests for source submission ([`ImapProvider::submit_source`], reached
//! through the [`Provider::submit_email_source`] verb): the loopback SMTP server
//! captures the exact `DATA` bytes it received, and the recorded IMAP stream carries
//! the `APPEND` literal, so both sides of the contract — the wire and the Sent copy —
//! are asserted **byte-identical** to the submitted source.

use std::{
    io::{BufRead, BufReader, Write},
    time::Duration,
};

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::Provider;

use crate::{ImapProvider, filing::SmtpSender, mock::MockStream, transport::Connection};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// The caller's final rendered MIME: the headers the envelope and receipt derive
/// from (including a quoted display name with a comma in it, and a `Cc`), a
/// dot-leading body line (SMTP dot-stuffing must round-trip invisibly), and body
/// bytes that are **not valid UTF-8** — signed/encrypted MIME is arbitrary bytes, so
/// the whole path must treat the source as bytes, never text.
fn rendered_source() -> Vec<u8> {
    let mut source = Vec::new();
    source.extend_from_slice(b"Date: Mon, 27 Aug 2026 10:00:00 +0000\r\n");
    source.extend_from_slice(b"Message-ID: <rendered-1@test.local>\r\n");
    source.extend_from_slice(b"From: Alice <alice@test.local>\r\n");
    source.extend_from_slice(b"To: Bob <bob@test.local>, \"Doe, Carol\" <carol@test.local>\r\n");
    source.extend_from_slice(b"Cc: dave@test.local\r\n");
    source.extend_from_slice(b"Subject: Already rendered\r\n");
    source.extend_from_slice(b"Content-Type: text/plain; charset=us-ascii\r\n\r\n");
    source.extend_from_slice(b".dot-leading line\r\n");
    source.extend_from_slice(b"body \xFF\xFE bytes\r\n");
    source
}

/// What the loopback server captured: the whole command conversation (for envelope
/// asserts) and the `DATA` payload as the receiving MTA hands it to the mailbox.
type Captured = (Vec<u8>, Vec<u8>);

/// A blocking loopback SMTP server that records two things: the whole command
/// conversation (for envelope asserts) and the `DATA` payload **as a receiving MTA
/// hands it to the mailbox** — dot-unstuffed per RFC 5321 §4.5.2 — so a test asserts
/// the server's message bytes equal the submitted source exactly.
fn capturing_loopback_smtp() -> (String, std::sync::mpsc::Receiver<Captured>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        socket.write_all(b"220 mock ESMTP\r\n").unwrap();
        let mut conversation = Vec::new();
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).unwrap() != 0 {
            conversation.extend_from_slice(&line);
            let command = String::from_utf8_lossy(&line).trim_end().to_uppercase();
            if command == "DATA" {
                socket.write_all(b"354 go ahead\r\n").unwrap();
                let mut received = Vec::new();
                loop {
                    line.clear();
                    if reader.read_until(b'\n', &mut line).unwrap() == 0 {
                        break;
                    }
                    if line == b".\r\n" {
                        break;
                    }
                    // The receiver deletes one leading '.' per line (the sender's
                    // dot-stuffing); what is left is the message as submitted.
                    if line.first() == Some(&b'.') {
                        received.extend_from_slice(&line[1..]);
                    } else {
                        received.extend_from_slice(&line);
                    }
                }
                tx.send((conversation.clone(), received)).unwrap();
                socket.write_all(b"250 2.0.0 queued\r\n").unwrap();
            } else if command == "QUIT" {
                socket.write_all(b"221 bye\r\n").unwrap();
                break;
            } else {
                socket.write_all(b"250 OK\r\n").unwrap();
            }
            line.clear();
        }
    });
    (addr, rx)
}

/// Builds a provider whose IMAP side is a mock over `after_login` (prefixed with a
/// greeting + login accept, so the connection opens and authenticates) and whose
/// SMTP transport is plaintext to `addr`.
async fn source_provider(addr: String, after_login: Vec<u8>) -> ImapProvider<MockStream> {
    let mut imap = crate::mock::script(&["* OK ready\r\n", "a1 OK LOGIN ok\r\n"]);
    imap.extend_from_slice(&after_login);
    let (stream, _) = MockStream::new(imap);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    ImapProvider::with_connection_and_smtp(
        conn,
        MailboxId::try_from("INBOX").unwrap(),
        SmtpSender::Plaintext { addr },
    )
}

/// The literal bytes of the (last) `APPEND` in the recorded client stream — exactly
/// what the server received as the filed copy's content.
fn appended_literal(recorded: &[u8]) -> Vec<u8> {
    let at = recorded
        .windows(6)
        .rposition(|w| w == b"APPEND")
        .expect("an APPEND was issued");
    let rest = &recorded[at..];
    let open = rest
        .iter()
        .position(|&b| b == b'{')
        .expect("the APPEND names a literal size");
    let close = rest
        .iter()
        .position(|&b| b == b'}')
        .expect("the literal size closes");
    let len: usize = String::from_utf8_lossy(&rest[open + 1..close])
        .parse()
        .expect("the literal size is a number");
    // `{N}\r\n` then the raw literal bytes.
    rest[close + 3..close + 3 + len].to_vec()
}

#[tokio::test]
async fn submit_email_source_sends_and_files_the_exact_submitted_bytes() {
    let (addr, received) = capturing_loopback_smtp();
    // The IMAP side resolves the real `\Sent` folder via LIST and accepts the APPEND
    // literal with a UIDPLUS APPENDUID (the same script the Draft-path filing uses).
    let imap = crate::mock::script(&[
        "* OK ready\r\n",
        "a1 OK LOGIN ok\r\n",
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 50 9] APPEND completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(imap);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection_and_smtp(
        conn,
        MailboxId::try_from("INBOX").unwrap(),
        SmtpSender::Plaintext { addr },
    );

    let source = rendered_source();
    let receipt = provider
        .submit_email_source(&account(), &source)
        .await
        .unwrap();

    // The receipt reconciles by the bytes' OWN Message-ID (parsed, not supplied) and
    // carries the real Sent key from APPENDUID.
    assert_eq!(receipt.message_id.as_str(), "rendered-1@test.local");
    assert_eq!(receipt.email_key.as_str(), "imap:v50:u9@Sent");
    assert!(receipt.sent_copy.is_filed());

    let (conversation, message) = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the loopback server received the DATA payload");
    // The envelope is derived from the bytes' own headers: the From addr-spec, every
    // To/Cc recipient (the quoted display name with its comma is ONE address), no
    // invented ones.
    let conversation = String::from_utf8_lossy(&conversation);
    assert!(
        conversation.contains("MAIL FROM:<alice@test.local>\r\n"),
        "{conversation}"
    );
    assert!(
        conversation.contains("RCPT TO:<bob@test.local>\r\n"),
        "{conversation}"
    );
    assert!(
        conversation.contains("RCPT TO:<carol@test.local>\r\n"),
        "{conversation}"
    );
    assert!(
        conversation.contains("RCPT TO:<dave@test.local>\r\n"),
        "{conversation}"
    );
    // The server received EXACTLY the submitted bytes — dot-stuffing round-tripped
    // invisibly and the non-UTF-8 body arrived untouched.
    assert_eq!(message, source);

    // The Sent copy was APPENDed with the SAME bytes, byte for byte — no
    // re-assembly, no Bcc-splitting difference: what the recipients got is what the
    // sender keeps.
    let recorded_bytes = recorded.lock().expect("mock read lock").clone();
    assert_eq!(appended_literal(&recorded_bytes), source);
}

#[tokio::test]
async fn submit_email_source_refuses_bytes_without_a_message_id() {
    let (addr, _received) = capturing_loopback_smtp();
    // No Message-ID: the contract puts stamping on the caller (the Write Contract),
    // so the provider refuses rather than sending an unreconcilable message.
    let provider = source_provider(addr, Vec::new()).await;
    let err = provider
        .submit_email_source(&account(), b"From: alice@test.local\r\n\r\nbody\r\n")
        .await
        .unwrap_err();
    assert!(!err.is_retryable(), "bad bytes never become retryable");
    assert!(
        err.detail().contains("Message-ID"),
        "the error names what is missing: {}",
        err.detail()
    );
}

#[tokio::test]
async fn submit_email_source_refuses_bytes_without_a_derivable_sender() {
    let (addr, _received) = capturing_loopback_smtp();
    // A Message-ID but no From header: the SMTP envelope has no MAIL FROM to derive,
    // which is a property of the bytes, not a transport failure.
    let provider = source_provider(addr, Vec::new()).await;
    let err = provider
        .submit_email_source(
            &account(),
            b"Message-ID: <rendered-2@test.local>\r\nTo: bob@test.local\r\n\r\nbody\r\n",
        )
        .await
        .unwrap_err();
    assert!(!err.is_retryable());
    assert!(
        err.detail().contains("From"),
        "the error names what is missing: {}",
        err.detail()
    );
}

#[tokio::test]
async fn submit_email_source_without_a_transport_is_rejected() {
    // No SMTP configured: the submission capability is absent and the call says so
    // with an InvalidState (the draft path's rule, unchanged).
    let (stream, _) = MockStream::new(crate::mock::script(&[
        "* OK ready\r\n",
        "a1 OK LOGIN ok\r\n",
    ]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());
    assert!(!provider.connection_info().capabilities.submission());
    let err = provider
        .submit_email_source(&account(), &rendered_source())
        .await
        .unwrap_err();
    assert!(!err.is_retryable());
}

/// **The same regression the draft path guards against**, on the source seam: a
/// message delivered whose Sent copy could not be filed must come back a SUCCESSFUL
/// receipt that says `Unfiled` — never an `Err` (the mail has gone; a caller that saw
/// a failure would re-send it), and never a silent clean send.
#[tokio::test]
async fn a_delivered_source_whose_sent_copy_cannot_be_filed_says_so() {
    let (addr, received) = capturing_loopback_smtp();
    // The IMAP session is exhausted after login: every further command reads EOF,
    // exactly as a session dropped while idle behaves, and a mock-backed provider
    // has no redial to fall back on.
    let provider = source_provider(addr, Vec::new()).await;
    let source = rendered_source();
    let receipt = provider
        .submit_email_source(&account(), &source)
        .await
        .expect("a delivered send is never failed for a filing error");
    let (_, message) = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the message really was delivered");
    assert_eq!(message, source);
    assert!(
        !receipt.sent_copy.is_filed(),
        "the copy was not filed, and the receipt has to carry that"
    );
    let detail = receipt
        .sent_copy
        .unfiled_detail()
        .expect("an unfiled copy carries why");
    assert!(!detail.is_empty());
    // The receipt still reconciles by the bytes' own Message-ID, with the
    // Message-ID-derived fallback key (no APPENDUID was ever seen).
    assert_eq!(receipt.message_id.as_str(), "rendered-1@test.local");
    assert_eq!(receipt.email_key.as_str(), "sent:rendered-1@test.local");
}

/// The envelope derivation, on the shapes real rendered mail uses.
#[test]
fn addr_specs_split_only_on_separating_commas() {
    use super::addr_specs;
    // A quoted display name with a comma is ONE address.
    assert_eq!(
        addr_specs("\"Doe, Carol\" <carol@test.local>, dave@test.local"),
        vec!["carol@test.local".to_owned(), "dave@test.local".to_owned()]
    );
    // A comment's comma and a group's members.
    assert_eq!(
        addr_specs("bob@test.local (Bob, a friend), eve@test.local"),
        vec!["bob@test.local".to_owned(), "eve@test.local".to_owned()]
    );
    assert_eq!(
        addr_specs("team: bob@test.local, carol@test.local;"),
        vec!["bob@test.local".to_owned(), "carol@test.local".to_owned()]
    );
    // The empty group names no recipient (the assembler's own Bcc-only `To` form).
    assert!(addr_specs("undisclosed-recipients:;").is_empty());
    assert!(addr_specs("\"a display name only\"").is_empty());
    // Entries stay in order; taking the first `From` is the envelope sender's rule.
    assert_eq!(
        addr_specs("alice@test.local, other@test.local"),
        vec!["alice@test.local".to_owned(), "other@test.local".to_owned()]
    );
}

/// `SourceSubmission::parse` over a whole message: the receipt's id, the envelope,
/// and the EHLO identity all come from the bytes.
#[tokio::test]
async fn the_submission_facts_all_come_from_the_bytes() {
    use super::SourceSubmission;
    let sub = SourceSubmission::parse(&rendered_source()).unwrap();
    assert_eq!(sub.message_id.as_str(), "rendered-1@test.local");
    assert_eq!(sub.from, "alice@test.local");
    assert_eq!(
        sub.to,
        vec![
            "bob@test.local".to_owned(),
            "carol@test.local".to_owned(),
            "dave@test.local".to_owned(),
        ]
    );
    assert_eq!(sub.ehlo, "test.local");
}

/// The envelope de-duplicates a recipient the bytes name twice (the draft path's
/// rule, kept): a strict server rejects a repeated `RCPT`.
#[tokio::test]
async fn the_envelope_deduplicates_recipients_named_twice() {
    use super::SourceSubmission;
    let source = b"Message-ID: <dedup@test.local>\r\n\
                   From: alice@test.local\r\n\
                   To: Bob <bob@test.local>\r\n\
                   Cc: BOB@test.local, carol@test.local\r\n\
                   Bcc: bob@test.local\r\n\r\nbody\r\n";
    let sub = SourceSubmission::parse(source).unwrap();
    assert_eq!(
        sub.to,
        vec!["bob@test.local".to_owned(), "carol@test.local".to_owned()]
    );
}
