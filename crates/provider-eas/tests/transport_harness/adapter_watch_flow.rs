// SPDX-License-Identifier: MPL-2.0
//! Adapter watch scenarios: the `EasPingWatcher` (`Watch` over EAS `Ping`)
//! for the bound folder — the plan's T7 proof set. The three wire states
//! map onto the trait's two events (status 2 + changed folders →
//! `Changed`; status 1 expiry → `KeepAlive` with the heartbeat grown for
//! the next round), the status-5 round adopts the server's interval (the
//! client's retry carries it — and the watcher's subsequent request
//! carries the band-clamped value), a transport drop tunes DOWN and
//! surfaces retryable, and the error statuses classify through the Ping
//! family table (7 → resync, 3 → permanent). The heartbeat threading is
//! asserted ON THE WIRE: every request's `HeartbeatInterval` is the tuned
//! value the previous round earned.

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_provider::{Watch as _, WatchEvent};
use provider_eas::adapter::EasAdapter;

use super::{
    harness::client_at,
    ping_flow::{ping_response, request_heartbeat},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn folder() -> engine_core::ids::MailboxId {
    engine_core::ids::MailboxId::try_from("fid-inbox").unwrap()
}

fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

/// (a) The change state: status 2 with the changed-folder list answers
/// `Changed`, and the session keeps watching — the second `next()` drives
/// another Ping round. The request pins the monitored collection: the
/// bound folder's ServerId and class Email.
#[tokio::test]
async fn status_two_with_folders_answers_changed_and_keeps_watching() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, n: usize| match n {
        1 => MockResponse::wbxml(&ping_response("2", None, &["fid-inbox"])),
        _ => MockResponse::wbxml(&ping_response("1", None, &[])),
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;

    assert_eq!(
        watch.next().await.expect("a change event"),
        WatchEvent::Changed
    );
    assert_eq!(server.count(), 1);
    // The request monitored the bound folder as an Email collection.
    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let mut folder_hits = Vec::new();
    find_texts(&tree, "fid-inbox", &mut folder_hits);
    assert!(
        !folder_hits.is_empty(),
        "the bound folder's ServerId rides the Folders list"
    );
    let mut class_hits = Vec::new();
    find_texts(&tree, "Email", &mut class_hits);
    assert!(
        class_hits.len() == 1,
        "the collection is monitored as class Email"
    );

    // Returning Changed leaves the session watching — the next call is
    // another Ping round (an expiry this time → KeepAlive).
    assert_eq!(
        watch.next().await.expect("the session keeps watching"),
        WatchEvent::KeepAlive
    );
    assert_eq!(server.count(), 2);
}

/// (b) The expiry state: status 1 with no changed folders answers
/// `KeepAlive`, and the heartbeat GROWS by the tuning step for the next
/// round — asserted on request 2's `HeartbeatInterval`.
#[tokio::test]
async fn expiry_answers_keepalive_and_grows_the_heartbeat() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("1", None, &[]))
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;
    assert_eq!(watch.heartbeat_secs(), 300, "the band floor starts it");

    assert_eq!(
        watch.next().await.expect("a clean expiry"),
        WatchEvent::KeepAlive
    );
    assert_eq!(
        watch.heartbeat_secs(),
        600,
        "clean expiry grows by the step"
    );
    assert_eq!(request_heartbeat(&server.request(1)), "300");

    // The next round carries the tuned value on the wire.
    assert_eq!(
        watch.next().await.expect("another clean expiry"),
        WatchEvent::KeepAlive
    );
    assert_eq!(
        request_heartbeat(&server.request(2)),
        "600",
        "the next round carries the tuned value"
    );
    assert_eq!(
        watch.heartbeat_secs(),
        900,
        "growth continues toward the cap"
    );
}

/// (c) The mislabel defense: a status-1 answer that nonetheless carries a
/// changed-folder list is a CHANGE (the folder list only exists when
/// changes occurred — live evidence 2026-08-03), not a keep-alive.
#[tokio::test]
async fn an_expiry_answer_with_folders_is_still_a_change() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("1", None, &["fid-inbox"]))
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;
    assert_eq!(
        watch.next().await.expect("folders mean changes"),
        WatchEvent::Changed
    );
}

/// (d) The status-5 round: the client retries once CARRYING the server's
/// interval (request 2 pins it), and the watcher adopts the value clamped
/// into its band — request 3 carries the clamped value. The surfaced
/// expiry answers `KeepAlive`.
#[tokio::test]
async fn status_five_adopts_the_server_heartbeat_clamped_into_the_band() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, n: usize| match n {
        // 300 is out of this server's range; it demands 60.
        1 => MockResponse::wbxml(&ping_response("5", Some(60), &[])),
        _ => MockResponse::wbxml(&ping_response("1", None, &[])),
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;

    assert_eq!(
        watch
            .next()
            .await
            .expect("the adopted retry expires cleanly"),
        WatchEvent::KeepAlive
    );
    assert_eq!(
        request_heartbeat(&server.request(2)),
        "60",
        "the status-5 retry carries the server's interval"
    );
    assert_eq!(
        watch.heartbeat_secs(),
        300,
        "the adopted value clamps into the band floor"
    );
    watch.next().await.expect("the session continues");
    assert_eq!(
        request_heartbeat(&server.request(3)),
        "300",
        "the watcher's own next round carries the clamped adoption"
    );
}

/// (e) The drop: a transport failure tunes the heartbeat DOWN and surfaces
/// retryable — the host reconnects per its own policy and may keep the
/// session (or persist `heartbeat_secs` across a rebuild).
#[tokio::test]
async fn a_transport_drop_tunes_down_and_surfaces_retryable() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(200)) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;

    let err = watch
        .next()
        .await
        .expect_err("a bare 200 is not a Ping answer");
    assert_eq!(err.class(), FailureClass::Retryable);
    assert_eq!(watch.heartbeat_secs(), 300, "a drop pulls toward the floor");

    // Tuning is observable after growth: expiry → 600, drop → 300.
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, n: usize| match n {
        1 => MockResponse::wbxml(&ping_response("1", None, &[])),
        _ => MockResponse::bare(200),
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;
    watch.next().await.expect("grow to 600");
    assert!(watch.next().await.is_err(), "the drop");
    assert_eq!(watch.heartbeat_secs(), 300, "the drop stepped back down");
}

/// (f) The error statuses classify through the Ping family table: 7
/// (folder hierarchy changed) is the resync shape; 3 (protocol error) is
/// permanent.
#[tokio::test]
async fn error_statuses_classify_per_the_ping_table() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("7", None, &[]))
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;
    let err = watch.next().await.expect_err("status 7 refuses to watch");
    assert_eq!(
        err.class(),
        FailureClass::NeedsResync,
        "hierarchy-changed means re-sync first"
    );

    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("3", None, &[]))
    }) as Handler);
    let adapter = adapter_at(&server);
    let mut watch = adapter.watcher().await;
    let err = watch
        .next()
        .await
        .expect_err("status 3 is a protocol failure");
    assert_eq!(err.class(), FailureClass::Permanent);
}

/// The watched collection's texts anywhere in the tree (id or class).
fn find_texts(el: &provider_eas::wbxml::WbxmlElement, needle: &str, found: &mut Vec<String>) {
    if let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        && t == needle
    {
        found.push(t.clone());
    }
    for child in &el.children {
        find_texts(child, needle, found);
    }
}
