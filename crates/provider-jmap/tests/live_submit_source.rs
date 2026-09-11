//! Gated live check for `submit_email_source` (`Email/import` +
//! `EmailSubmission/set`): caller-rendered bytes go out VERBATIM, and the
//! provider files the same bytes as the Sent copy.
//!
//! This is the decision-A proof for the source seam. The draft path
//! (`submit_email`) re-renders structured fields and provably cannot carry an
//! iMIP `method=` parameter (`live_imip.rs`); the source path ships the bytes
//! themselves, so an iMIP-shaped or PGP/MIME message must survive the round
//! trip byte-for-byte — and the Sent copy must be those same bytes, because
//! the `Email/import` files it directly and the host files nothing.
//!
//! Every message is destroyed after its assertions, leaving Sent as found.
//! Skips with no `STALWART_HTTP_ADDR`.

mod common;

use common::*;
use engine_core::ids::ProviderKey;
use engine_provider::{MailEdit, Provider};
use provider_jmap::JmapProvider;

/// An iMIP-shaped message: the `text/calendar; method=REPLY` part the draft
/// path provably cannot encode (`live_imip.rs`). CRLF-terminated throughout,
/// as RFC 5322 produces.
fn imip_source() -> String {
    [
        "From: Alice Tester <alice@test.local>",
        "To: Bob <bob@test.local>",
        "Subject: Accepted: Sprint planning (source seam)",
        "Message-ID: <jmap-src-imip@test.local>",
        "MIME-Version: 1.0",
        "Content-Type: multipart/mixed; boundary=src-boundary",
        "",
        "--src-boundary",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "Alice has accepted this invitation.",
        "--src-boundary",
        "Content-Type: text/calendar; charset=utf-8; method=REPLY",
        "Content-Transfer-Encoding: 7bit",
        "",
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Engine//Live//EN",
        "METHOD:REPLY",
        "BEGIN:VEVENT",
        "UID:jmap-src-imip-event@test.local",
        "DTSTAMP:20260501T080000Z",
        "ORGANIZER;CN=Bob:mailto:bob@test.local",
        "ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:mailto:alice@test.local",
        "SEQUENCE:0",
        "END:VEVENT",
        "END:VCALENDAR",
        "--src-boundary--",
        "",
    ]
    .join("\r\n")
}

/// A PGP/MIME-signed message (RFC 3156): `multipart/signed` with a static
/// armored signature. No key server is involved — the assertion is byte
/// survival, not signature verification.
fn pgp_source() -> String {
    [
        "From: Alice Tester <alice@test.local>",
        "To: Bob <bob@test.local>",
        "Subject: signed through the source seam",
        "Message-ID: <jmap-src-pgp@test.local>",
        "MIME-Version: 1.0",
        "Content-Type: multipart/signed; micalg=pgp-sha256;",
        " protocol=\"application/pgp-signature\"; boundary=pgp-boundary",
        "",
        "--pgp-boundary",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "This body is covered by the signature part.",
        "--pgp-boundary",
        "Content-Type: application/pgp-signature; name=signature.asc",
        "Content-Description: OpenPGP digital signature",
        "Content-Disposition: attachment; filename=signature.asc",
        "",
        "-----BEGIN PGP SIGNATURE-----",
        "",
        "iQGzBAEBCgAdFiEE3v5y8Z0kQzJtZW1wbGUtc2lnLWJ5dGVzACgkQc291cmNlLXNl",
        "YW0KZ2V0cy10aGUtYnl0ZXMtdGhyb3VnaC11bnRvdWNoZWQtd2hlbi1pdC1nb2Vz",
        "=s3am",
        "-----END PGP SIGNATURE-----",
        "--pgp-boundary--",
        "",
    ]
    .join("\r\n")
}

/// Byte-for-byte equality, tolerating only a trailing line-terminator
/// normalization.
fn assert_verbatim(fetched: &[u8], sent: &[u8]) {
    fn strip_trailing_terminators(bytes: &[u8]) -> &[u8] {
        let mut end = bytes.len();
        while end > 0 && matches!(bytes[end - 1], b'\r' | b'\n') {
            end -= 1;
        }
        &bytes[..end]
    }
    assert_eq!(
        strip_trailing_terminators(fetched),
        strip_trailing_terminators(sent),
        "the filed copy is not the bytes that were sent"
    );
}

/// The raw source of the message `key` resolves to on the next sync — the Sent
/// copy the import filed.
async fn sent_source(provider: &JmapProvider, key: &ProviderKey) -> Vec<u8> {
    let sync = provider
        .sync_email(&account(), None)
        .await
        .expect("sync mail");
    let message = sync
        .update
        .changed()
        .iter()
        .find(|m| m.id.key() == key)
        .unwrap_or_else(|| panic!("the imported message {key:?} syncs back"))
        .clone();
    provider
        .fetch_message_source(&account(), &message)
        .await
        .expect("fetch the filed source")
        .as_bytes()
        .to_vec()
}

/// Destroys a test message, leaving Sent as found.
async fn destroy(provider: &JmapProvider, key: ProviderKey) {
    provider
        .edit_mail(&account(), &MailEdit::delete(key))
        .await
        .expect("destroy the test message");
}

#[tokio::test]
async fn live_submit_source_imports_verbatim_and_files_sent() {
    let Some(provider) = setup("submit_source").await else {
        return;
    };

    // ---- iMIP: the shape the draft path provably refuses. ----
    let imip = imip_source();
    let receipt = provider
        .submit_email_source(&account(), imip.as_bytes(), &[])
        .await
        .expect("an iMIP-shaped source submits through the import seam");
    assert!(
        receipt.sent_copy.is_filed(),
        "the import files the Sent copy itself"
    );
    assert_eq!(receipt.message_id.as_str(), "jmap-src-imip@test.local");

    let fetched = sent_source(&provider, &receipt.email_key).await;
    assert_verbatim(&fetched, imip.as_bytes());
    let text = String::from_utf8_lossy(&fetched);
    assert!(text.contains("text/calendar"), "the calendar part survived");
    assert!(
        text.contains("method=REPLY"),
        "the iMIP method parameter survived: {text}"
    );
    assert!(
        text.contains("BEGIN:VEVENT"),
        "the iCalendar payload survived"
    );
    destroy(&provider, receipt.email_key).await;

    // ---- PGP/MIME: signed bytes must not be re-rendered. ----
    let pgp = pgp_source();
    let receipt = provider
        .submit_email_source(&account(), pgp.as_bytes(), &[])
        .await
        .expect("a PGP/MIME source submits through the import seam");
    assert!(receipt.sent_copy.is_filed());
    assert_eq!(receipt.message_id.as_str(), "jmap-src-pgp@test.local");

    let fetched = sent_source(&provider, &receipt.email_key).await;
    assert_verbatim(&fetched, pgp.as_bytes());
    let text = String::from_utf8_lossy(&fetched);
    assert!(text.contains("application/pgp-signature"));
    assert!(
        text.contains("-----BEGIN PGP SIGNATURE-----"),
        "the armored signature survived"
    );
    destroy(&provider, receipt.email_key).await;
}
