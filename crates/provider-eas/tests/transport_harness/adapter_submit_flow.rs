// SPDX-License-Identifier: MPL-2.0
//! Adapter submission scenarios: `submit_email` (the `Draft` → RFC 5322 →
//! SendMail path) and `submit_email_source` (caller-rendered bytes, sent
//! verbatim). The wire proof is the plan's T6 requirement — the sent bytes
// ! are provable through the transport layer: the `<Mime>` element must be
// OPAQUE and byte-exact, `<SaveInSentItems/>` present, and the `<ClientId>`
// deterministic per `Message-ID` (Exchange's dedup key for a lost-response
// retry). Response-shape and refusal contracts ride the same mock server.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{Draft, Provider as _, SentCopy};
use provider_eas::{
    adapter::EasAdapter,
    wbxml::{
        WbxmlElement, WbxmlValue,
        tags::{compose, pages},
    },
};

use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

fn folder() -> engine_core::ids::MailboxId {
    engine_core::ids::MailboxId::try_from("fid-inbox").unwrap()
}

fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

/// The trait-side draft: an original message with a Cc and a Bcc — the Bcc
/// is the interesting wire fact (the filed variant keeps the header so the
/// server can route the blind copy; no other recipient can see it).
fn draft() -> Draft {
    let mut draft = Draft::new(
        MessageIdHeader::new("eas-send-0001@test.local").unwrap(),
        EmailAddress::new("alice@example.test"),
        vec![EmailAddress::new("bob@example.net")],
        "Wire proof",
        "Body text",
    );
    draft.cc = vec![EmailAddress::new("carol@example.net")];
    draft.bcc = vec![EmailAddress::new("bcc-dave@example.org")];
    draft
}

/// Bytes a caller might hand `submit_email_source` — every identifier a
/// reserved name.
fn source_bytes() -> Vec<u8> {
    b"From: alice@example.test\r\n\
       To: bob@example.net\r\n\
       Cc: carol@example.net\r\n\
       Bcc: bcc-dave@example.org\r\n\
       Message-ID: <src-0007@test.local>\r\n\
       Subject: exact bytes\r\n\
       \r\n\
       body\r\n"
        .to_vec()
}

fn find_el(el: &WbxmlElement, page: u8, token: u8) -> Option<&WbxmlElement> {
    if el.page == page && el.token == token {
        return Some(el);
    }
    el.children
        .iter()
        .find_map(|child| find_el(child, page, token))
}

/// A SendMail-family response with an in-body `<Status>` (1 is success).
fn compose_status(root_token: u8, status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::COMPOSE,
        root_token,
        vec![WbxmlElement::text(pages::COMPOSE, compose::STATUS, status)],
    )
}

/// The SendMail root token (page 16, 0x05 — the `compose_flow` convention).
const PAGE_COMPOSE_ROOT_SEND: u8 = 0x05;

/// (a) The happy send: the draft assembles through `engine-rfc5322` (the
/// filed variant — the `Bcc` header stays in the bytes so the server can
/// route the blind copy), rides the wire as an OPAQUE `<Mime>`, asks for
/// the Sent copy with `<SaveInSentItems/>`, and correlates through a
/// `Message-ID`-derived `<ClientId>` that is stable across retries of the
/// same send. The receipt carries the Graph/IMAP `sent:<Message-ID>`
/// placeholder — SendMail's empty body returns no server id.
#[tokio::test]
async fn submit_email_sends_the_filed_mime_opaque_with_a_deterministic_client_id() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);

    let receipt = adapter
        .submit_email(&account(), &draft())
        .await
        .expect("the draft sends");
    assert_eq!(
        receipt.email_key.as_str(),
        "sent:eas-send-0001@test.local",
        "no server id comes back — the placeholder key stands in"
    );
    assert_eq!(receipt.message_id.as_str(), "eas-send-0001@test.local");
    assert!(matches!(receipt.sent_copy, SentCopy::Filed));

    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let mime = find_el(&tree, pages::COMPOSE, compose::MIME).expect("Mime element present");
    let WbxmlValue::Opaque(bytes) = &mime.value else {
        panic!("Mime must be OPAQUE — inline text corrupts binary MIME");
    };
    let text = std::str::from_utf8(bytes).expect("assembled MIME is UTF-8");
    assert!(
        text.contains("eas-send-0001@test.local"),
        "the caller's pre-generated Message-ID travels verbatim: {text}"
    );
    assert!(
        text.contains("To: bob@example.net") && text.contains("Cc: carol@example.net"),
        "the visible recipients route from the bytes"
    );
    assert!(
        text.contains("Bcc:") && text.contains("bcc-dave@example.org"),
        "the filed variant keeps the Bcc header — the server routes the blind \
         copy from it, and no recipient sees the header"
    );
    assert!(
        find_el(&tree, pages::COMPOSE, compose::SAVE_IN_SENT_ITEMS).is_some(),
        "SaveInSentItems asks the server to file the Sent copy"
    );
    let client_id = match &find_el(&tree, pages::COMPOSE, compose::CLIENT_ID)
        .expect("ClientId element present")
        .value
    {
        WbxmlValue::Text(id) => id.clone(),
        other => panic!("ClientId is inline text, got {other:?}"),
    };
    assert!(
        client_id.starts_with("SM") && client_id.len() <= 40,
        "deterministic Message-ID-derived ClientId under the MS-ASCMD cap: {client_id}"
    );

    // The same send retried derives the SAME ClientId — Exchange dedups a
    // lost-response retry by it ([MS-ASCMD] §2.2.3.28.1).
    adapter
        .submit_email(&account(), &draft())
        .await
        .expect("the retry sends");
    let retry_id = match &find_el(
        &server.request(2).wbxml_tree().expect("retry decodes"),
        pages::COMPOSE,
        compose::CLIENT_ID,
    )
    .expect("ClientId element present")
    .value
    {
        WbxmlValue::Text(id) => id.clone(),
        other => panic!("ClientId is inline text, got {other:?}"),
    };
    assert_eq!(client_id, retry_id, "same Message-ID ⇒ same ClientId");

    // The verb ladder: submission (and its scheduling sibling) plus the
    // write bit flip with these verbs; the read bits stay on.
    let caps = adapter.connection_info().capabilities;
    assert!(caps.submission(), "submission flips with submit_email");
    assert!(
        caps.scheduling_submission(),
        "raw-MIME submission carries its own scheduling parameters"
    );
    assert!(caps.mail_writes(), "mail_writes flips with edit_mail");
    assert!(
        caps.mail() && caps.message_source(),
        "the read bits stay on"
    );
}

/// (b) The caller-rendered path: the EXACT bytes go out — byte-for-byte,
/// the Write Contract for a source the caller may already have signed —
/// with the receipt keyed by the bytes' own `Message-ID`.
#[tokio::test]
async fn submit_email_source_sends_the_exact_bytes() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);
    let source = source_bytes();

    let receipt = adapter
        .submit_email_source(&account(), &source, &[])
        .await
        .expect("the bytes send");
    assert_eq!(receipt.message_id.as_str(), "src-0007@test.local");
    assert_eq!(receipt.email_key.as_str(), "sent:src-0007@test.local");

    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let mime = find_el(&tree, pages::COMPOSE, compose::MIME).expect("Mime element present");
    match &mime.value {
        WbxmlValue::Opaque(bytes) => assert_eq!(
            bytes, &source,
            "the submitted bytes travel verbatim — never re-rendered"
        ),
        other => panic!("Mime must be OPAQUE, got {other:?}"),
    }
}

/// (c) An envelope this transport cannot honor is refused BEFORE the wire:
/// SendMail routes recipients from the bytes' own headers, so a recipients
/// list that is not the header set would silently mis-deliver.
#[tokio::test]
async fn submit_email_source_refuses_an_envelope_the_bytes_cannot_honor() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);
    let source = source_bytes();

    // An extra recipient not present in any header.
    let err = adapter
        .submit_email_source(&account(), &source, &["extra@example.net".to_owned()])
        .await
        .expect_err("a superset envelope is refused");
    assert_eq!(err.class(), FailureClass::Permanent);
    // A subset envelope (the Bcc stripped from the list) — those recipients
    // would never be delivered.
    let err = adapter
        .submit_email_source(
            &account(),
            &source,
            &["bob@example.net".to_owned(), "carol@example.net".to_owned()],
        )
        .await
        .expect_err("a subset envelope is refused");
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(server.count(), 0, "refusals happen before any wire round");
}

/// (d) The seam's permanent shape contract: bytes without a `Message-ID`
/// (nothing to reconcile the sent copy by) or without a trailing line
/// terminator (an unterminated body) never reach the wire.
#[tokio::test]
async fn submit_email_source_refuses_unstamped_or_unterminated_bytes() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);

    let no_id = b"From: alice@example.test\r\nTo: bob@example.net\r\nSubject: x\r\n\r\nbody\r\n";
    let err = adapter
        .submit_email_source(&account(), no_id, &[])
        .await
        .expect_err("no Message-ID is permanent");
    assert_eq!(err.class(), FailureClass::Permanent);

    let mut unterminated = source_bytes();
    unterminated.pop(); // drop the final LF
    unterminated.pop(); // drop the final CR
    let err = adapter
        .submit_email_source(&account(), &unterminated, &[])
        .await
        .expect_err("an unterminated body is permanent");
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(server.count(), 0, "neither refusal touches the wire");
}

/// (e) SendMail's in-body failure shape: an HTTP 200 whose body carries a
/// non-1 `<Status>` is a failure — the client surfaces the value, and the
/// adapter classifies it through the SendMail family table (132 = server
/// temporarily unavailable ⇒ retryable).
#[tokio::test]
async fn send_mail_in_body_status_classifies() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&compose_status(PAGE_COMPOSE_ROOT_SEND, "132"))
    }) as Handler);
    let adapter = adapter_at(&server);

    let err = adapter
        .submit_email(&account(), &draft())
        .await
        .expect_err("in-body 132 is a failure, not a success");
    assert_eq!(err.class(), FailureClass::Retryable);
    let err = adapter
        .submit_email_source(&account(), &source_bytes(), &[])
        .await
        .expect_err("the source path classifies identically");
    assert_eq!(err.class(), FailureClass::Retryable);
}
