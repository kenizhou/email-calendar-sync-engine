// SPDX-License-Identifier: MPL-2.0
//! Adapter event downsync scenarios (P2 Task 2): `sync_events` — Sync class
// `Calendar` over the adapter's bound calendar folder — mapped onto the
// engine's `ScopeSync<Event>`: the bootstrap snapshot, the incremental
// delta, the SyncKey-invalidation resync recovery, and the Exchange 15.2
// empty-bootstrap follow quirk. Split from `adapter_calendar_flow.rs` at
// the 500-line cap; the shared adapter helpers come from there.

use std::sync::Arc;

use engine_core::{ids::ProviderKey, sync::SyncUpdate};
use engine_provider::Provider as _;
use provider_eas::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_END_TIME, CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_WEEK,
        CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE, CAL_START_TIME, CAL_SUBJECT, CAL_UID,
        PAGE_CALENDAR,
    },
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_COLLECTION, AS_COLLECTION_ID, AS_COLLECTIONS, AS_COMMANDS,
        AS_DELETE, AS_MORE_AVAILABLE, AS_SERVER_ID, AS_STATUS, AS_SYNC, AS_SYNC_KEY, PAGE_AIRSYNC,
    },
    wbxml::{WbxmlElement, WbxmlValue},
};

use super::{
    adapter_calendar_flow::{account, adapter_at, calendar_id},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The `<SyncKey>` text inside a Sync request's Collection element.
fn request_sync_key(req: &CapturedRequest) -> String {
    fn find_key(el: &WbxmlElement) -> Option<String> {
        if el.token == AS_SYNC_KEY
            && let WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(find_key)
    }
    req.wbxml_tree()
        .and_then(|tree| find_key(&tree))
        .expect("request carries a SyncKey")
}

/// The `<CollectionId>` text inside a Sync request.
fn request_collection(req: &CapturedRequest) -> String {
    fn find(el: &WbxmlElement) -> Option<String> {
        if el.token == AS_COLLECTION_ID
            && let WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(find)
    }
    req.wbxml_tree()
        .and_then(|tree| find(&tree))
        .expect("request carries a CollectionId")
}

// ---------------------------------------------------------------------------
// Calendar-class Sync response fixtures (the email-shaped builders in
// `fixtures.rs` do not carry Calendar-page ApplicationData).
// ---------------------------------------------------------------------------

/// A minimal timed Calendar `ApplicationData`: subject + UTC stamps + a UID,
/// optionally a weekly Tuesday recurrence.
fn calendar_app_data(
    subject: &str,
    start: &str,
    end: &str,
    uid: &str,
    weekly: bool,
) -> WbxmlElement {
    let mut children = vec![
        WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, subject),
        WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, start),
        WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, end),
        WbxmlElement::text(PAGE_CALENDAR, CAL_UID, uid),
        WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
    ];
    if weekly {
        children.push(WbxmlElement::container(
            PAGE_CALENDAR,
            CAL_RECURRENCE,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
                // DayOfWeek bitmask: a single Tuesday is bit 4.
                WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "4"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "4"),
            ],
        ));
    }
    WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, children)
}

/// A Calendar-class Sync response: key rotation, optional `MoreAvailable`,
/// and item adds/deletes — the `fixtures::sync_response` shape with
/// Calendar-page ApplicationData payloads.
fn calendar_sync_response(
    status: &str,
    new_key: &str,
    more_available: bool,
    adds: &[(&str, WbxmlElement)],
    deleted: &[&str],
) -> WbxmlElement {
    let mut commands: Vec<WbxmlElement> = adds
        .iter()
        .map(|&(id, ref app)| {
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_ADD,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, id),
                    app.clone(),
                ],
            )
        })
        .collect();
    commands.extend(deleted.iter().map(|&id| {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_DELETE,
            vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, id)],
        )
    }));
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
    ];
    if more_available {
        collection.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE));
    }
    if !commands.is_empty() {
        collection.push(WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, commands));
    }
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                collection,
            )],
        )],
    )
}
/// The event snapshot: `sync_events` with no cursor bootstraps the bound
/// calendar's collection from "0", and the class-Calendar items convert to
/// engine events — id = ServerId, uid = the EAS UID, membership = the bound
/// calendar, recurrence structural.
#[tokio::test]
async fn event_sync_bootstraps_the_bound_calendar_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        assert_eq!(ordinal, 1, "a no-more-available snapshot is one round");
        MockResponse::wbxml(&calendar_sync_response(
            "1",
            "ev-key-2",
            false,
            &[
                (
                    "srv:ev-1",
                    calendar_app_data(
                        "Team Sync",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                ),
                (
                    "srv:ev-2",
                    calendar_app_data(
                        "Weekly Standup",
                        "20260811T090000Z",
                        "20260811T093000Z",
                        "uid-two",
                        true,
                    ),
                ),
            ],
            &[],
        ))
    }) as Handler);
    let adapter = adapter_at(&server);

    let sync = adapter
        .sync_events(&account(), None)
        .await
        .expect("bootstrap Sync succeeds");
    assert_eq!(request_sync_key(&server.request(1)), "0");
    assert_eq!(
        request_collection(&server.request(1)),
        "fid-cal-1",
        "the bound calendar folder IS the Sync CollectionId"
    );
    assert_eq!(sync.next_cursor.as_str(), "ev-key-2");
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a bootstrap round must read as a snapshot");
    };
    assert_eq!(objects.len(), 2);
    assert_eq!(present.len(), 2);

    let single = objects
        .iter()
        .find(|e| e.id.as_str() == "srv:ev-1")
        .unwrap();
    assert_eq!(single.uid.as_str(), "uid-one");
    assert_eq!(single.title, "Team Sync");
    assert!(
        single.calendars.contains(&calendar_id()),
        "membership is the bound calendar folder"
    );
    assert!(!single.is_recurring());

    let master = objects
        .iter()
        .find(|e| e.id.as_str() == "srv:ev-2")
        .unwrap();
    assert!(master.is_recurring(), "the weekly item keeps its rule");
}

/// The incremental event delta: the second call threads the rotated key and
/// maps Update/Delete elements onto `changed`/`removed`.
#[tokio::test]
async fn event_sync_incremental_delta_maps_updates_and_deletes() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&calendar_sync_response(
                "1",
                "ev-key-1",
                false,
                &[(
                    "srv:ev-1",
                    calendar_app_data(
                        "Team Sync",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                )],
                &[],
            )),
            2 => MockResponse::wbxml(&calendar_sync_response(
                "1",
                "ev-key-2",
                false,
                &[(
                    "srv:ev-1",
                    calendar_app_data(
                        "Team Sync II",
                        "20260819T090000Z",
                        "20260819T100000Z",
                        "uid-one",
                        false,
                    ),
                )],
                &["srv:ev-2"],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_events(&account(), None)
        .await
        .expect("bootstrap");
    let second = adapter
        .sync_events(&account(), Some(&first.next_cursor))
        .await
        .expect("incremental");
    assert_eq!(request_sync_key(&server.request(2)), "ev-key-1");
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &second.update
    else {
        panic!("an incremental round must read as a delta");
    };
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].title, "Team Sync II");
    assert_eq!(removed, &vec![ProviderKey::new("srv:ev-2").unwrap()]);
    assert_eq!(second.next_cursor.as_str(), "ev-key-2");
}

/// SyncKey invalidation (collection status 3) recovers INSIDE the call —
/// the mail stream's precedent, adapted to the whole-scope verb: since a
/// `ScopeSync` is applied atomically, the accumulated round is discarded and
/// the pass restarts from "0" as a snapshot. A status 3 answered to the
/// bootstrap key itself surfaces as `NeedsResync`.
#[tokio::test]
async fn an_invalidated_event_key_rebootstraps_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&calendar_sync_response(
                "1",
                "ev-key-1",
                false,
                &[(
                    "srv:ev-1",
                    calendar_app_data(
                        "Team Sync",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                )],
                &[],
            )),
            2 => MockResponse::wbxml(&calendar_sync_response("3", "0", false, &[], &[])),
            3 => MockResponse::wbxml(&calendar_sync_response(
                "1",
                "ev-key-2",
                false,
                &[
                    (
                        "srv:ev-1",
                        calendar_app_data(
                            "Team Sync",
                            "20260818T090000Z",
                            "20260818T100000Z",
                            "uid-one",
                            false,
                        ),
                    ),
                    (
                        "srv:ev-2",
                        calendar_app_data(
                            "Extra",
                            "20260820T090000Z",
                            "20260820T100000Z",
                            "uid-two",
                            false,
                        ),
                    ),
                ],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_events(&account(), None)
        .await
        .expect("bootstrap");
    let recovered = adapter
        .sync_events(&account(), Some(&first.next_cursor))
        .await
        .expect("status 3 recovers inside the call");
    assert_eq!(request_sync_key(&server.request(2)), "ev-key-1");
    assert_eq!(request_sync_key(&server.request(3)), "0");
    assert_eq!(server.count(), 3, "exactly one recovery round — no loop");
    assert!(
        recovered.is_snapshot(),
        "the recovery result is a full snapshot"
    );
    assert_eq!(recovered.update.changed().len(), 2);

    // A dead key that IS the bootstrap key has nothing left to retry.
    let dead = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&calendar_sync_response("3", "0", false, &[], &[]))
    }) as Handler);
    let err = adapter_at(&dead)
        .sync_events(&account(), None)
        .await
        .expect_err("status 3 at the bootstrap key surfaces");
    assert_eq!(err.class(), engine_core::error::FailureClass::NeedsResync);
}

/// The Exchange 15.2 empty-bootstrap quirk (the `should_follow_empty_bootstrap`
/// rule): a bootstrap round that returns nothing but a rotated key is
/// followed once, and the items of the follow-up round complete the snapshot.
#[tokio::test]
async fn an_empty_bootstrap_round_is_followed_once() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&calendar_sync_response("1", "ev-key-1", false, &[], &[])),
            2 => MockResponse::wbxml(&calendar_sync_response(
                "1",
                "ev-key-2",
                false,
                &[(
                    "srv:ev-1",
                    calendar_app_data(
                        "Late Arrival",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                )],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let sync = adapter
        .sync_events(&account(), None)
        .await
        .expect("the quirk recovers inside the call");
    assert_eq!(request_sync_key(&server.request(2)), "ev-key-1");
    assert!(sync.is_snapshot());
    assert_eq!(sync.update.changed().len(), 1);
    assert_eq!(sync.next_cursor.as_str(), "ev-key-2");
}

/// A Sync failure outside the recovery set surfaces in the engine's
/// classification (Sync 6 = permanent; HTTP 5xx = retryable) — never as a
/// raw protocol error.
#[tokio::test]
async fn event_sync_failures_surface_in_engine_classes() {
    super::harness::init_logger();
    let status_server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&calendar_sync_response("6", "0", false, &[], &[]))
    }) as Handler);
    let err = adapter_at(&status_server)
        .sync_events(&account(), None)
        .await
        .expect_err("Sync 6 must surface");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);

    let http_server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let err = adapter_at(&http_server)
        .sync_events(&account(), None)
        .await
        .expect_err("HTTP 500 must surface");
    assert_eq!(err.class(), engine_core::error::FailureClass::Retryable);
}
