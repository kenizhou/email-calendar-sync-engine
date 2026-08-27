// SPDX-License-Identifier: MPL-2.0
//! Adapter `fetch_message_source` scenarios: the ItemOperations MIME-fetch
//! verb ([MS-ASCMD] §4.10.2) mapped onto the engine's single-primitive
//! `RawMime` contract — the small-message whole fetch, the truncated answer
//! reassembled from authoritative server ranges, the mis-placed chunk, and
//! the absent item's classified error. The wire side rides the real
//! transport against the mock server; the trait side is the thing under
//! test. The wire-shape builders live in `adapter_source_wire.rs`.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
};
use engine_provider::Provider as _;
use provider_eas::adapter::EasAdapter;

use super::{
    adapter_source_wire::{
        AIRSYNC, AS_COLLECTION_ID, AS_MIME_SUPPORT, AS_SERVER_ID, body_preference_type,
        fetch_request_field, fetch_status_response, mime_fetch_response, options_child_field,
        request_has_range,
    },
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

fn folder() -> MailboxId {
    MailboxId::try_from("fid-inbox").unwrap()
}

fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

fn message(id: &str) -> Message {
    Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(folder()),
    )
}

/// A 300-byte ASCII MIME fixture (digit rows) — sized so a truncation at 120
/// leaves two ranged rounds.
fn fixture_mime() -> Vec<u8> {
    let pattern = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJ"; // 45 bytes
    let mut out = Vec::with_capacity(300);
    while out.len() < 300 {
        out.extend_from_slice(&pattern[..(300 - out.len()).min(pattern.len())]);
    }
    out
}

/// (a) The small-message shape: one unranged MIME fetch answers the whole
/// RFC 5322 bytes. The wire request is pinned on every addressing axis —
/// Store, CollectionId (the bound MailboxId, T4's identity mapping),
/// ServerId (the engine MessageId verbatim), MIMESupport level 2 +
/// BodyPreference Type 4 (the §4.10.2.1 MIME shape) — and NO Range element.
#[tokio::test]
async fn small_message_fetches_whole_mime_in_one_round() {
    super::harness::init_logger();
    let mime = b"From: alice@example.test\r\nSubject: whole fetch\r\n\r\nsmall body\r\n";
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&mime_fetch_response(mime, None, None, false))
    }) as Handler);
    let adapter = adapter_at(&server);

    let raw = adapter
        .fetch_message_source(&account(), &message("sid:1"))
        .await
        .expect("whole fetch succeeds");
    assert_eq!(raw.as_bytes(), mime, "the whole RFC 5322 bytes, verbatim");
    assert_eq!(server.count(), 1, "one round when nothing was truncated");

    assert_eq!(
        fetch_request_field(&server.request(1), 20, 0x07).as_deref(),
        Some("Mailbox"),
        "Store=Mailbox"
    );
    assert_eq!(
        fetch_request_field(&server.request(1), AIRSYNC, AS_COLLECTION_ID).as_deref(),
        Some("fid-inbox"),
        "the bound MailboxId IS the CollectionId (T4's identity mapping)"
    );
    assert_eq!(
        fetch_request_field(&server.request(1), AIRSYNC, AS_SERVER_ID).as_deref(),
        Some("sid:1"),
        "the engine MessageId IS the wire ServerId"
    );
    assert_eq!(
        options_child_field(&server.request(1), AIRSYNC, AS_MIME_SUPPORT).as_deref(),
        Some("2"),
        "MIMESupport level 2: raw MIME for all messages"
    );
    assert_eq!(
        body_preference_type(&server.request(1)).as_deref(),
        Some("4"),
        "BodyPreference Type 4 = MIME BLOB"
    );
    assert!(
        !request_has_range(&server.request(1)),
        "no Range element on the unranged first fetch"
    );
    // The verb ladder: with fetch_message_source landed the message_source
    // bit is on (and mail still is).
    let caps = adapter.connection_info().capabilities;
    assert!(caps.message_source(), "message_source flips with this verb");
    assert!(caps.mail(), "mail stays on — the read domain");
}

/// (b) The truncated shape: the unranged fetch answers a truncated prefix
/// (Truncated flag + Total), and the loop re-fetches the remainder as
/// `Options>Range` rounds. The server's range fulfillment is best-effort
/// ([MS-ASCMD] §2.2.3.143.2: "the byte-range specified by the server in the
/// response is the authoritative value") — round 2 answers SHORTER than
/// asked, round 3 completes — and the assembled bytes must equal the full
/// fixture exactly.
#[tokio::test]
async fn truncated_mime_is_reassembled_from_authoritative_ranges() {
    super::harness::init_logger();
    let fixture = fixture_mime();
    let wire = fixture.clone();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("ItemOperations"));
        match ordinal {
            // Unranged fetch: the first 120 bytes, truncated, Total 300.
            1 => MockResponse::wbxml(&mime_fetch_response(&wire[..120], None, Some(300), true)),
            // Asked 120-299, answers only 120-219 (best-effort shorter).
            2 => MockResponse::wbxml(&mime_fetch_response(
                &wire[120..220],
                Some((120, 219)),
                Some(300),
                true,
            )),
            // Asked 220-299, completes the item.
            3 => MockResponse::wbxml(&mime_fetch_response(
                &wire[220..],
                Some((220, 299)),
                Some(300),
                false,
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let raw = adapter
        .fetch_message_source(&account(), &message("sid:2"))
        .await
        .expect("reassembly succeeds");
    assert_eq!(server.count(), 3, "one unranged round + two ranged rounds");
    assert!(
        !request_has_range(&server.request(1)),
        "round 1 is unranged"
    );
    assert_eq!(
        options_child_field(&server.request(2), 20, 0x09).as_deref(),
        Some("120-299"),
        "round 2 asks from the assembled length, capped at Total-1"
    );
    assert_eq!(
        options_child_field(&server.request(3), 20, 0x09).as_deref(),
        Some("220-299"),
        "round 3 resumes from the AUTHORITATIVE range end + 1"
    );
    assert_eq!(
        raw.as_bytes(),
        fixture.as_slice(),
        "the reassembled bytes equal the full fixture, byte for byte"
    );
}

/// (c) The absent item: a fetch-level status 6 ("object was not found or
/// access denied", [MS-ASCMD] §2.2.3.177.8) classifies as the trait's
/// stale-target class — `Conflict`: the item moved or vanished, the caller
/// re-syncs, then retries. A protocol-error status 2 surfaces `Permanent`.
/// Neither panics; both are classified `ProviderError`s.
#[tokio::test]
async fn absent_item_surfaces_a_classified_error() {
    super::harness::init_logger();
    let absent = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&fetch_status_response("6"))
    }) as Handler);
    let gone = adapter_at(&absent)
        .fetch_message_source(&account(), &message("sid:gone"))
        .await
        .expect_err("status 6 must surface");
    assert_eq!(gone.class(), engine_core::error::FailureClass::Conflict);
    assert!(
        gone.detail().contains('6'),
        "the surfaced detail carries the protocol status: {}",
        gone.detail()
    );

    let malformed = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&fetch_status_response("2"))
    }) as Handler);
    let protocol = adapter_at(&malformed)
        .fetch_message_source(&account(), &message("sid:bad"))
        .await
        .expect_err("status 2 must surface");
    assert_eq!(
        protocol.class(),
        engine_core::error::FailureClass::Permanent
    );
}

/// (d) The contiguity guard: a ranged round whose AUTHORITATIVE span starts
/// past the assembled length leaves a gap the client cannot fill by
/// concatenation — that surfaces `Permanent` (a server placing bytes where
/// it said something else), never silent misassembly.
#[tokio::test]
async fn a_misplaced_range_chunk_surfaces_permanent() {
    super::harness::init_logger();
    let fixture = fixture_mime();
    let wire = fixture.clone();
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, ordinal: usize| {
        match ordinal {
            1 => MockResponse::wbxml(&mime_fetch_response(&wire[..120], None, Some(300), true)),
            // Asked 120-299 but claims to cover 200-299 — bytes 120..200 gap.
            2 => MockResponse::wbxml(&mime_fetch_response(
                &wire[200..300],
                Some((200, 299)),
                Some(300),
                false,
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let err = adapter
        .fetch_message_source(&account(), &message("sid:3"))
        .await
        .expect_err("a gap must surface, not misassemble");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(
        err.detail().contains("200"),
        "the surfaced detail names the misplaced start: {}",
        err.detail()
    );
}
