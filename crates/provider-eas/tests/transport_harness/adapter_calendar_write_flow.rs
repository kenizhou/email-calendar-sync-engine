// SPDX-License-Identifier: MPL-2.0
//! Adapter calendar WRITE scenarios (P2 Task 3): `create_event` / `patch_event`
//! / `delete_event` over the Sync Commands upsync against the offline mock
//! server — the Add's ServerId backfill through the `Responses` ack, the
//! series Replace, the exception write under the master, the Delete, the
//! already-gone idempotence, and the refusals (put, unbound, cold ledger,
//! the per-item failure class). The wire-conversion goldens live in
//! `src/calendar/convert_write_tests.rs`; the upsync request shapes in
//! `tests/commands_sync/calendar_write.rs`.

use std::sync::Arc;

use engine_core::{
    calendar::{Frequency, NDay, Recurrence, RecurrenceRule, Weekday},
    ids::{EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, Duration, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget, Provider as _};
use provider_eas::{
    calendar::{CAL_EXCEPTION, CAL_EXCEPTIONS, CAL_SUBJECT, PAGE_CALENDAR},
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CHANGE, AS_CLIENT_ID, AS_COLLECTION, AS_COLLECTION_ID,
        AS_COLLECTIONS, AS_COMMANDS, AS_DELETE, AS_RESPONSES, AS_SERVER_ID, AS_STATUS, AS_SYNC,
        AS_SYNC_KEY, PAGE_AIRSYNC, WbxmlElement,
    },
};

use super::{
    adapter_calendar_flow::{account, adapter_at, calendar_id},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

pub(super) fn utc8() -> TimeZoneId {
    TimeZoneId::iana("Etc/GMT-8").unwrap()
}

pub(super) fn zoned(day: &str, wall: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: format!("{day}T{wall}").parse().unwrap(),
        zone: utc8(),
    }
}

pub(super) fn stamp() -> UtcDateTime {
    "2026-08-15T12:00:00Z".parse().unwrap()
}

pub(super) fn draft() -> EventDraft {
    EventDraft::new(
        calendar_id(),
        Uid::new("uid-create-1").unwrap(),
        "Sprint Review",
        zoned("2026-08-18", "09:00:00"),
        zoned("2026-08-18", "10:00:00"),
        stamp(),
    )
    .description("Quarterly review")
    .location("Room 101")
}

pub(super) fn weekly_tuesday() -> RecurrenceRule {
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Tu,
        nth_of_period: None,
    }];
    rule
}

/// A recurring master as the read side would have stored it: weekly
/// Tuesday, one already-deleted occurrence (the third one).
pub(super) fn series_event() -> engine_core::calendar::Event {
    let mut event = engine_core::calendar::Event::new(
        EventId::try_from("srv:ev-9").unwrap(),
        Uid::new("uid-standup").unwrap(),
        Memberships::of_one(calendar_id()),
        zoned("2026-08-11", "09:00:00"),
    );
    event.title = String::from("Weekly Standup");
    event.duration = Duration::from_parts(0, 0, 0, 30, 0, 0).unwrap();
    let mut recurrence = Recurrence::from_rule(weekly_tuesday());
    recurrence.overrides.insert(
        "2026-08-25T09:00:00".parse().unwrap(),
        engine_core::calendar::RecurrenceOverride::Excluded,
    );
    event.recurrence = Some(recurrence);
    event
}

/// A clean Sync upsync response: key rotation + status 1, plus optional
/// `Responses` children (Add acks / item statuses).
pub(super) fn upsync_response(new_key: &str, responses: Vec<WbxmlElement>) -> WbxmlElement {
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
    ];
    if !responses.is_empty() {
        collection.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            responses,
        ));
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

/// A minimal class-Calendar Sync downsync response (the seeding pass): one
/// item + the rotated key. The item matters — an empty bootstrap round is
/// the Exchange 15.2 quirk the pass would follow once (`should_follow_empty_bootstrap`),
/// and a one-request seed keeps every scenario's ordinals obvious.
pub(super) fn seed_response(new_key: &str) -> WbxmlElement {
    use provider_eas::calendar::{
        CAL_ALL_DAY_EVENT, CAL_END_TIME, CAL_START_TIME, CAL_SUBJECT, CAL_UID,
    };
    let item = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "srv:ev-0"),
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![
                    WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Seed Item"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260818T010000Z"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260818T013000Z"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_UID, "uid-seed"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
                ],
            ),
        ],
    );
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![item]),
                ],
            )],
        )],
    )
}

/// The server's Add ack ([MS-ASCMD] §2.2.3.7.2).
pub(super) fn add_ack(client_id: &str, server_id: &str, status: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, client_id),
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
        ],
    )
}

/// A failed Change/Delete item status ([MS-ASCMD] §2.2.3.24/§2.2.3.42.2).
pub(super) fn item_status(server_id: &str, status: &str, change_not_delete: bool) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        if change_not_delete {
            AS_CHANGE
        } else {
            AS_DELETE
        },
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
        ],
    )
}

/// Every text value of `(page, token)` elements in a request's decoded
/// tree, depth-first.
pub(super) fn texts(req: &CapturedRequest, page: u8, token: u8) -> Vec<String> {
    fn walk(el: &WbxmlElement, page: u8, token: u8, out: &mut Vec<String>) {
        if el.page == page
            && el.token == token
            && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        {
            out.push(t.clone());
        }
        for child in &el.children {
            walk(child, page, token, out);
        }
    }
    let tree = req.wbxml_tree().expect("the request is WBXML");
    let mut out = Vec::new();
    walk(&tree, page, token, &mut out);
    out
}

/// How many `(page, token)` elements ride a request's decoded tree —
/// containers included, whatever their value shape.
pub(super) fn count_of(req: &CapturedRequest, page: u8, token: u8) -> usize {
    fn walk(el: &WbxmlElement, page: u8, token: u8) -> usize {
        let here = usize::from(el.page == page && el.token == token);
        here + el
            .children
            .iter()
            .map(|c| walk(c, page, token))
            .sum::<usize>()
    }
    let tree = req.wbxml_tree().expect("the request is WBXML");
    walk(&tree, page, token)
}

/// The single text value of `(page, token)` in a request's decoded tree.
pub(super) fn text_of(req: &CapturedRequest, page: u8, token: u8) -> String {
    texts(req, page, token)
        .into_iter()
        .next()
        .expect("the element rides the request")
}

/// Seeds the calendar ledger with one clean `sync_events` pass.
pub(super) async fn seed(adapter: &provider_eas::adapter::EasAdapter) {
    adapter
        .sync_events(&account(), None)
        .await
        .expect("the seeding pass succeeds");
}

// ---------------------------------------------------------------------------
// create_event
// ---------------------------------------------------------------------------

/// A cold calendar ledger (no pass observed yet) refuses `NeedsResync`
/// rather than guessing a key — the mail `edit_mail` cold-ledger rule, and
/// NO request goes out.
#[tokio::test]
async fn a_cold_ledger_refuses_the_create_without_a_round_trip() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = adapter_at(&server);
    let err = adapter
        .create_event(&account(), &draft())
        .await
        .expect_err("cold ledger");
    assert_eq!(err.class(), engine_core::error::FailureClass::NeedsResync);
    assert_eq!(server.count(), 0, "no request went out");
}

/// The create happy path: Sync `Add` riding the ledger's key, the server's
/// `Responses` ack assigns the ServerId, and the receipt keys it and echoes
/// the uid — the only id-reveal point.
#[tokio::test]
async fn create_adds_and_resolves_the_server_id_through_the_ack() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                // The Add itself: assert the request's wire shape.
                assert_eq!(count_of(req, PAGE_AIRSYNC, AS_ADD), 1, "exactly one Add");
                let client_id = text_of(req, PAGE_AIRSYNC, AS_CLIENT_ID);
                assert!(
                    client_id.starts_with("CalAdd-") && client_id.len() <= 40,
                    "the ClientId is synthesized under the 40-char cap: {client_id}"
                );
                MockResponse::wbxml(&upsync_response(
                    "ev-key-3",
                    vec![add_ack(&client_id, "srv:new-1", "1")],
                ))
            }
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    let receipt = adapter
        .create_event(&account(), &draft())
        .await
        .expect("the Add lands");
    assert_eq!(receipt.event.as_str(), "srv:new-1");
    assert_eq!(receipt.uid.as_str(), "uid-create-1");

    let add_request = server.request(2);
    assert_eq!(
        text_of(&add_request, PAGE_AIRSYNC, AS_SYNC_KEY),
        "ev-key-2",
        "the Add rides the ledger's key"
    );
    assert_eq!(
        text_of(&add_request, PAGE_AIRSYNC, AS_COLLECTION_ID),
        "fid-cal-1",
        "the bound calendar folder IS the CollectionId"
    );
    assert_eq!(
        texts(&add_request, PAGE_CALENDAR, CAL_SUBJECT),
        vec!["Sprint Review".to_owned()]
    );
}

/// A failed Add ack surfaces as a permanent error naming the item status —
/// the live-probed Status 6 (conversion error) class.
#[tokio::test]
async fn a_failed_add_ack_surfaces_with_its_item_status() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                let client_id = text_of(req, PAGE_AIRSYNC, AS_CLIENT_ID);
                MockResponse::wbxml(&upsync_response(
                    "ev-key-3",
                    vec![add_ack(&client_id, "", "6")],
                ))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    let err = adapter
        .create_event(&account(), &draft())
        .await
        .expect_err("status 6 rejects");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(
        err.detail().contains('6'),
        "names the item status: {}",
        err.detail()
    );
}

// ---------------------------------------------------------------------------
// patch_event
// ---------------------------------------------------------------------------

/// A series patch rides a Sync `Change` naming the master's ServerId with a
/// complete ApplicationData document.
#[tokio::test]
async fn a_series_patch_rides_a_change_of_the_master() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                assert_eq!(
                    count_of(req, PAGE_AIRSYNC, AS_CHANGE),
                    1,
                    "exactly one Change"
                );
                assert_eq!(
                    text_of(req, PAGE_AIRSYNC, AS_SERVER_ID),
                    "srv:ev-9",
                    "the Change names the master"
                );
                MockResponse::wbxml(&upsync_response("ev-key-3", vec![]))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;

    let base = series_event();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed Standup"),
    );
    let receipt = adapter
        .patch_event(&account(), &base, &edit)
        .await
        .expect("the Change lands");
    assert_eq!(receipt.event.as_str(), "srv:ev-9");
    assert_eq!(receipt.uid.as_str(), "uid-standup");
    assert!(
        receipt.revisions.is_empty(),
        "EAS carries no revision tokens"
    );
}

/// An instance patch rides a `Change` of the master whose `Exceptions`
/// container carries the target occurrence as a modified exception.
#[tokio::test]
async fn an_instance_patch_writes_the_exception_under_the_master() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                assert_eq!(
                    count_of(req, PAGE_CALENDAR, CAL_EXCEPTIONS),
                    1,
                    "the Exceptions container rides"
                );
                assert_eq!(
                    count_of(req, PAGE_CALENDAR, CAL_EXCEPTION),
                    2,
                    "the existing exclusion rides plus the new one"
                );
                MockResponse::wbxml(&upsync_response("ev-key-3", vec![]))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;

    let base = series_event();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Instance(Occurrence::starting(zoned("2026-09-01", "09:00:00"))),
        EventPatch::new(stamp()).summary("Special Tuesday"),
    );
    adapter
        .patch_event(&account(), &base, &edit)
        .await
        .expect("the exception write lands");
}

// ---------------------------------------------------------------------------
// delete_event
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The refusals and the capability ladder
// ---------------------------------------------------------------------------
