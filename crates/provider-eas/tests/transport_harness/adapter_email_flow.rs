// SPDX-License-Identifier: MPL-2.0
//! Adapter `stream_email` scenarios ([MS-ASSYNC]): the Sync class-Email verb
//! mapped onto the engine's `EmailChunk` stream — the bootstrap "0" full
//! round, per-round Additive pagination with per-round checkpoints, the
//! SyncKey-invalidation recovery as a Reconcile pass, the empty round, and
//! the classification of surfaced collection statuses. The wire side rides
//! the real transport against the mock server; the trait side is the thing
//! under test. The wire-shape builders live in `adapter_email_wire.rs`.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    sync::{SyncState, SyncWindow},
};
use engine_provider::{EmailChunk, EmailStream, PassMode, Provider as _};
use futures_util::StreamExt;
use provider_eas::{
    adapter::EasAdapter,
    commands::{AS_COLLECTION_ID, AS_SYNC_KEY, AS_WINDOW_SIZE},
};

use super::{
    adapter_email_wire::{request_field, sync_round},
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

fn folder() -> MailboxId {
    MailboxId::try_from("fid-inbox").unwrap()
}

/// An adapter bound to the mock server's endpoint, per the
/// `GraphProvider::new`-bound-to-one-folder precedent.
fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

/// Drains a stream into its chunks, failing on any error item.
async fn drain(mut stream: EmailStream<'_>) -> Vec<EmailChunk> {
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.expect("chunk"));
    }
    chunks
}

/// The cold shape: `cursor: None` sends the "0" collection key, and the
/// bootstrap round's full page streams as Additive chunks checkpointed at the
/// rotated key — per-round `advance_to`, not a held final marker (each EAS
/// round rotates the key, so every round is a safe resume point — the
/// resumability edge EAS holds over JMAP/Graph). The wire items project onto
/// engine `Message`s keyed by ServerId, member of the bound folder, read →
/// `$seen`, DateReceived → `received_at`.
#[tokio::test]
async fn fresh_sync_bootstraps_from_zero_and_streams_the_round() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        MockResponse::wbxml(&sync_round(
            "1",
            "col-1",
            false,
            &[
                (
                    "sid:1",
                    "First",
                    "alice@example.test",
                    "1",
                    Some("2026-08-20T09:30:00Z"),
                ),
                ("sid:2", "Second", "bob@example.test", "0", None),
            ],
            &[],
            &[],
        ))
    }) as Handler);
    let adapter = adapter_at(&server);

    let chunks = drain(adapter.stream_email(&account(), None, SyncWindow::full(), 25, 0)).await;
    assert_eq!(
        request_field(&server.request(1), AS_SYNC_KEY),
        "0",
        "cursor None bootstraps from the \"0\" collection key"
    );
    assert_eq!(
        request_field(&server.request(1), AS_COLLECTION_ID),
        "fid-inbox",
        "the bound MailboxId IS the CollectionId"
    );
    assert_eq!(
        request_field(&server.request(1), AS_WINDOW_SIZE),
        "25",
        "fetch_batch is the WindowSize"
    );
    assert_eq!(server.count(), 1, "one round when MoreAvailable is unset");
    assert_eq!(chunks.len(), 1, "chunk_size 0 → one chunk per round");

    let chunk = &chunks[0];
    assert_eq!(chunk.mode, PassMode::Additive);
    assert_eq!(
        chunk.advance_to.as_ref().map(SyncState::as_str),
        Some("col-1")
    );
    assert_eq!(
        chunk
            .changed
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["sid:1", "sid:2"],
        "Add items map as changed Messages keyed by ServerId"
    );
    let first = &chunk.changed[0];
    assert_eq!(first.envelope.subject.as_deref(), Some("First"));
    assert_eq!(
        first.envelope.from.first().map(|a| a.email.as_str()),
        Some("alice@example.test")
    );
    assert!(
        first.has_system_keyword(engine_core::mail::SystemKeyword::Seen),
        "Read=1 maps to $seen"
    );
    assert_eq!(
        first.received_at.map(|d| d.to_string()),
        Some("2026-08-20T09:30:00Z".to_owned()),
        "DateReceived maps to received_at"
    );
    assert!(
        first.mailboxes.contains(&folder()) && first.mailboxes.len().get() == 1,
        "membership is the bound folder (the MailboxId IS the CollectionId)"
    );
    assert!(
        !chunk.changed[1].has_system_keyword(engine_core::mail::SystemKeyword::Seen),
        "Read=0 leaves $seen off"
    );
    // The verb ladder: with stream_email landed the mail bit is on.
    assert!(
        adapter.connection_info().capabilities.mail(),
        "mail flips with the message verbs — this is that slice"
    );
}

/// The steady-state shape: a delta spanning several MoreAvailable pages
/// yields Additive chunks per round, each round's completing chunk carrying
/// that round's rotated key as `advance_to` (intermediate sub-chunks within a
/// round hold the cursor — committing a later round's key before its rows
/// would lose them on a crash), and the wire Delete maps to an explicit
/// `removed` key.
#[tokio::test]
async fn multi_page_deltas_checkpoint_the_rotated_key_per_round() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&sync_round(
                "1",
                "col-2",
                true,
                &[
                    ("sid:1", "One", "a@example.test", "0", None),
                    ("sid:2", "Two", "a@example.test", "0", None),
                ],
                &[],
                &[],
            )),
            2 => MockResponse::wbxml(&sync_round(
                "1",
                "col-3",
                false,
                &[("sid:3", "Three", "a@example.test", "1", None)],
                &[("sid:2", "Two (edited)", "a@example.test", "1", None)],
                &["sid:9"],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let chunks = drain(adapter.stream_email(
        &account(),
        Some(&SyncState::new("col-1")),
        SyncWindow::full(),
        2,
        1,
    ))
    .await;
    assert_eq!(
        request_field(&server.request(1), AS_SYNC_KEY),
        "col-1",
        "round 1 sends the cursor's key"
    );
    assert_eq!(
        request_field(&server.request(2), AS_SYNC_KEY),
        "col-2",
        "round 2 sends the key round 1 rotated to"
    );
    assert_eq!(server.count(), 2);

    assert_eq!(
        chunks.len(),
        4,
        "each round splits at chunk_size 1: round 1 (two adds) into two, round 2 (add + update) into two"
    );
    assert!(chunks.iter().all(|c| c.mode == PassMode::Additive));
    // Round 1: the sub-chunk holds the cursor; the completing chunk
    // checkpoints the round's rotated key.
    assert_eq!(chunks[0].advance_to, None, "intra-round sub-chunk holds");
    assert_eq!(chunks[0].changed.len(), 1);
    assert_eq!(
        chunks[1].advance_to.as_ref().map(SyncState::as_str),
        Some("col-2")
    );
    assert_eq!(chunks[1].changed.len(), 1);
    // Round 2: Add + Update both land as changed, Delete as removed, riding
    // the round's completing chunk.
    assert_eq!(chunks[2].advance_to, None, "round 2's sub-chunk holds");
    assert_eq!(
        chunks[2]
            .changed
            .iter()
            .map(|m| (m.id.as_str(), m.envelope.subject.as_deref()))
            .collect::<Vec<_>>(),
        vec![("sid:3", Some("Three"))]
    );
    assert_eq!(
        chunks[3]
            .changed
            .iter()
            .map(|m| (m.id.as_str(), m.envelope.subject.as_deref()))
            .collect::<Vec<_>>(),
        vec![("sid:2", Some("Two (edited)"))]
    );
    assert_eq!(
        chunks[3].removed,
        vec![ProviderKey::new("sid:9").unwrap()],
        "a wire Delete becomes an explicit removed key"
    );
    assert_eq!(
        chunks[3].advance_to.as_ref().map(SyncState::as_str),
        Some("col-3"),
        "the last chunk's checkpoint is the pass's final cursor"
    );
}

/// SyncKey invalidation (collection status 3, [MS-ASCMD] Status (Sync): "MUST
/// return to SyncKey value of 0") recovers inside the stream: the dead key's
/// round is discarded and the pass restarts from "0" as a **Reconcile** pass
/// — intermediate chunks carry the present ids they cover and hold the
/// cursor, the final chunk advances and tombstones against the accumulated
/// present set (the JMAP `cannotCalculateChanges`→snapshot precedent).
#[tokio::test]
async fn an_invalidated_collection_key_reboots_from_zero_as_a_reconcile_pass() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&sync_round("3", "", false, &[], &[], &[])),
            2 => MockResponse::wbxml(&sync_round(
                "1",
                "col-2",
                true,
                &[
                    ("sid:1", "One", "a@example.test", "0", None),
                    ("sid:2", "Two", "a@example.test", "0", None),
                ],
                &[],
                &[],
            )),
            3 => MockResponse::wbxml(&sync_round(
                "1",
                "col-3",
                false,
                &[("sid:3", "Three", "a@example.test", "1", None)],
                &[],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let chunks = drain(adapter.stream_email(
        &account(),
        Some(&SyncState::new("col-1")),
        SyncWindow::full(),
        10,
        0,
    ))
    .await;
    assert_eq!(
        request_field(&server.request(1), AS_SYNC_KEY),
        "col-1",
        "the stale key goes out first, exactly as the cursor said"
    );
    assert_eq!(
        request_field(&server.request(2), AS_SYNC_KEY),
        "0",
        "the recovery round re-bootstraps the collection from \"0\""
    );
    assert_eq!(request_field(&server.request(3), AS_SYNC_KEY), "col-2");
    assert_eq!(server.count(), 3, "exactly one recovery — no loop");

    assert!(
        chunks.iter().all(|c| c.mode == PassMode::Reconcile),
        "the restarted pass reconciles"
    );
    assert_eq!(chunks.len(), 3, "two pages plus the completing marker");
    assert_eq!(
        chunks[0].advance_to, None,
        "reconcile pages hold the cursor"
    );
    assert_eq!(
        chunks[0].present,
        vec!["sid:1", "sid:2"]
            .into_iter()
            .map(|k| ProviderKey::new(k).unwrap())
            .collect::<Vec<_>>(),
        "each page covers its own present ids"
    );
    assert!(!chunks[0].is_reconcile_final());
    assert_eq!(
        chunks[1].present,
        vec![ProviderKey::new("sid:3").unwrap()],
        "the second page carries its own present ids"
    );
    assert!(!chunks[1].is_reconcile_final());
    // The marker chunk carries no ids of its own — the orchestrator
    // accumulates the pages' present sets, and this chunk's `advance_to`
    // triggers the tombstone against that union.
    assert!(chunks[2].is_reconcile_final(), "the marker tombstones");
    assert!(chunks[2].present.is_empty());
    assert_eq!(
        chunks[2].advance_to.as_ref().map(SyncState::as_str),
        Some("col-3")
    );
}

/// A no-changes round (the empty-body shape: success with the request's key
/// echoed) still yields one Additive chunk — no items, the cursor echoed —
/// so a caller always receives a checkpoint to persist.
#[tokio::test]
async fn an_empty_round_yields_one_chunk_with_the_echoed_cursor() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);

    let chunks = drain(adapter.stream_email(
        &account(),
        Some(&SyncState::new("col-7")),
        SyncWindow::full(),
        10,
        0,
    ))
    .await;
    assert_eq!(server.count(), 1);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].mode, PassMode::Additive);
    assert!(chunks[0].changed.is_empty() && chunks[0].removed.is_empty());
    assert_eq!(
        chunks[0].advance_to.as_ref().map(SyncState::as_str),
        Some("col-7"),
        "the empty-body success echoes the request key (an empty key would poison the cursor)"
    );
}

/// Surfaced collection statuses classify through the engine's classes, via
/// the crate's Sync-family table: a protocol-error status (4) is `Permanent`;
/// a transient server error (5) is `Retryable`; a status-3 answer to the
/// bootstrap key itself — no re-bootstrap left to try — is `NeedsResync` for
/// the orchestrator.
#[tokio::test]
async fn surfaced_statuses_follow_the_sync_family_classifier() {
    super::harness::init_logger();
    let status = |code: &'static str| {
        MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
            MockResponse::wbxml(&sync_round(code, "", false, &[], &[], &[]))
        }) as Handler)
    };

    let permanent = adapter_at(&status("4"))
        .stream_email(
            &account(),
            Some(&SyncState::new("col-1")),
            SyncWindow::full(),
            10,
            0,
        )
        .next()
        .await
        .expect("one item")
        .expect_err("status 4 must surface");
    assert_eq!(
        permanent.class(),
        engine_core::error::FailureClass::Permanent
    );

    let retryable = adapter_at(&status("5"))
        .stream_email(
            &account(),
            Some(&SyncState::new("col-1")),
            SyncWindow::full(),
            10,
            0,
        )
        .next()
        .await
        .expect("one item")
        .expect_err("status 5 must surface");
    assert_eq!(
        retryable.class(),
        engine_core::error::FailureClass::Retryable
    );

    let dead_bootstrap = adapter_at(&status("3"))
        .stream_email(&account(), None, SyncWindow::full(), 10, 0)
        .next()
        .await
        .expect("one item")
        .expect_err("status 3 to the bootstrap key cannot recover internally");
    assert_eq!(
        dead_bootstrap.class(),
        engine_core::error::FailureClass::NeedsResync
    );
}
