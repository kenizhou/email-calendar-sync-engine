// SPDX-License-Identifier: MPL-2.0
//! The engine-cli PIM acceptance scenarios: `engine-cli eas-sync --kind
//! calendar` and `--kind contacts` driven end-to-end against the mock
//! server — the P2 counterpart of `engine_cli_flow.rs`'s mail proof. One
//! command drives the whole pass through the ENGINE's own sync
//! (`engine_sync::sync_calendar` / `sync_contacts`) against a real SQLite
//! store, and the report the shell sees is the report a host would read:
//! containers, per-collection applies, the occurrence materialization
//! summary, and (behind `--create`) a create → re-sync round-trip proving
//! the ServerId backfill.
//!
//! Like the mail flow, the mock dispatches by REQUEST SHAPE, not ordinal:
//! every response is a pure function of the command and the decoded
//! request body, so interleaved passes stay answerable.

use std::sync::Arc;

use provider_eas::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_END_TIME, CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_WEEK,
        CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE, CAL_START_TIME, CAL_SUBJECT, CAL_UID,
        PAGE_CALENDAR,
    },
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CLIENT_ID, AS_COLLECTION, AS_COLLECTION_ID, AS_COLLECTIONS,
        AS_COMMANDS, AS_SERVER_ID, AS_STATUS, AS_SYNC, AS_SYNC_KEY, FH_SYNC_KEY, PAGE_AIRSYNC,
        PAGE_FOLDER,
    },
    wbxml::WbxmlElement,
};

use super::{
    adapter_calendar_write_flow::{add_ack, count_of, text_of, upsync_response},
    adapter_contacts_flow::contacts_sync_response,
    fixtures::folder_sync_response,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// One calendar arm run: `--kind calendar --rounds 2` against a fresh
/// store. The hierarchy carries mail and contacts folders the CLI must
/// filter out of the calendar fan-out; the event bootstrap lands a single
/// event plus a 4-count weekly series; round 2's delta adds one single.
/// The occurrence summary counts what materialized over the persisted
/// window, and a `search --kind calendar` proves the rows answer a real
/// query.
#[tokio::test]
async fn engine_cli_syncs_calendars_full_then_incremental_offline() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(calendar_respond) as Handler);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let db_arg = db.to_str().unwrap().to_owned();

    let out = engine_cli::run(&args(&[
        "eas-sync",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--kind",
        "calendar",
        "--url",
        &server.eas_url(),
        "--user",
        "user@example.test",
        "--password",
        "app-password",
        "--rounds",
        "2",
        "--horizon-start",
        "2026-08-01",
        "--horizon-end",
        "2026-10-01",
    ]))
    .await
    .expect("both rounds succeed");
    assert!(
        out.contains("1 calendar(s), protocol 16.1"),
        "the header names the calendar count and the negotiated version: {out}"
    );
    // Round 1: the container snapshot files the one type-8 folder; the
    // event bootstrap lands both items (the mail and contacts folders are
    // filtered out of the calendar fan-out).
    assert!(
        out.contains("containers  +1 -0"),
        "the container snapshot files the calendar folder: {out}"
    );
    assert!(
        out.contains("fid-cal-1  +2 -0"),
        "the event bootstrap: {out}"
    );
    // Round 2: the incremental delta — one new single, nothing else.
    assert!(out.contains("fid-cal-1  +1 -0"), "the event delta: {out}");
    // The occurrence summary: the single + the 4-count weekly series +
    // round 2's single = 6, over the persisted window, in the seeded zone.
    assert!(
        out.contains(
            "occurrences 6 in fid-cal-1 over \
             2026-08-01T00:00:00Z..2026-10-01T00:00:00Z (Etc/UTC)"
        ),
        "the materialization summary names count, calendar, window, zone: {out}"
    );

    // The stored rows answer a real calendar query — the sync produced
    // searchable events under the server's ids, not just counters.
    let found = engine_cli::run(&args(&[
        "search",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--kind",
        "calendar",
        "after:2026-08-01 before:2026-10-01",
    ]))
    .await
    .expect("search over the synced store");
    assert!(
        found.contains("srv:ev-"),
        "hits carry the synced ServerIds: {found}"
    );
}

/// The `--create` round-trip: after the sync rounds, the command creates
/// one probe event through the engine's outbox (the facade verb's own
/// composition), then re-syncs — and the re-sync's apply carries the
/// SERVER's copy of the event under the ServerId the Add ack returned,
/// which is the backfill a host re-reads after a write.
#[tokio::test]
async fn engine_cli_creates_an_event_and_backfills_the_server_id_offline() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(create_respond) as Handler);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let db_arg = db.to_str().unwrap().to_owned();

    let out = engine_cli::run(&args(&[
        "eas-sync",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--kind",
        "calendar",
        "--url",
        &server.eas_url(),
        "--user",
        "user@example.test",
        "--password",
        "app-password",
        "--rounds",
        "1",
        "--create",
        "--horizon-start",
        "2026-08-01",
        "--horizon-end",
        "2026-10-01",
    ]))
    .await
    .expect("the round-trip succeeds");
    assert!(
        out.contains("created srv:ev-new (uid engine-cli-acct-eas-1)"),
        "the create names the acked ServerId and the probe uid: {out}"
    );
    assert!(
        out.contains("fid-cal-1  +1 -0"),
        "the re-sync applies the server's copy of the created event: {out}"
    );
    assert!(
        out.contains(
            "occurrences 6 in fid-cal-1 over \
             2026-08-01T00:00:00Z..2026-10-01T00:00:00Z (Etc/UTC)"
        ),
        "the created event materializes like any other: {out}"
    );
}

/// One contacts arm run: `--kind contacts --rounds 2` against a fresh
/// store — address-book discovery (type 9), the card bootstrap, the delta,
/// and the people count the rebuild derived from the cards.
#[tokio::test]
async fn engine_cli_syncs_contacts_full_then_delta_offline() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(contacts_respond) as Handler);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let db_arg = db.to_str().unwrap().to_owned();

    let out = engine_cli::run(&args(&[
        "eas-sync",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--kind",
        "contacts",
        "--url",
        &server.eas_url(),
        "--user",
        "user@example.test",
        "--password",
        "app-password",
        "--rounds",
        "2",
    ]))
    .await
    .expect("both rounds succeed");
    assert!(
        out.contains("1 address book(s), protocol 16.1"),
        "the header names the address-book count and the negotiated version: {out}"
    );
    // Round 1: discovery files the one type-9 folder; the card bootstrap
    // lands both cards.
    assert!(
        out.contains("books       +1 -0"),
        "the discovery snapshot files the address book: {out}"
    );
    assert!(
        out.contains("fid-contacts-1 +2 -0"),
        "the card bootstrap: {out}"
    );
    // Round 2: the incremental delta — one new card.
    assert!(
        out.contains("fid-contacts-1 +1 -0"),
        "the card delta: {out}"
    );
    assert!(
        out.contains("people 3"),
        "the people count the rebuild derived from the three cards: {out}"
    );
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// The calendar mock's whole protocol: OPTIONS negotiates 16.1, FolderSync
/// answers the bootstrap or an empty delta, and class-Calendar Sync answers
/// the collection's bootstrap (`"0"` → two items), its first incremental
/// (`evk1` → one new single), or an empty steady round.
fn calendar_respond(req: &CapturedRequest, _ordinal: usize) -> MockResponse {
    common_respond(req, |_req, collection, key| {
        if key == "0" {
            events_sync(
                "evk1",
                &[
                    event_item(
                        "srv:ev-1",
                        "Team Sync",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                    event_item(
                        "srv:ev-2",
                        "Weekly Standup",
                        "20260811T090000Z",
                        "20260811T093000Z",
                        "uid-two",
                        true,
                    ),
                ],
            )
        } else if key == "evk1" {
            let _ = collection;
            events_sync(
                "evk2",
                &[event_item(
                    "srv:ev-3",
                    "Team Sync II",
                    "20260819T090000Z",
                    "20260819T100000Z",
                    "uid-three",
                    false,
                )],
            )
        } else {
            events_sync(&key, &[])
        }
    })
}

/// The create-round-trip mock: the bootstrap lands two items; the create's
/// upsync round (a request carrying a `ClientId`) is acked with the new
/// ServerId and rotates the server's collection key; the re-sync reads
/// from the STORE's cursor (`evk1` — a write does not advance the read
/// cursor), and a real server answers that read with the item the write
/// created, under the acked ServerId.
fn create_respond(req: &CapturedRequest, _ordinal: usize) -> MockResponse {
    common_respond(req, |req, _collection, key| {
        if count_of(req, PAGE_AIRSYNC, AS_CLIENT_ID) > 0 {
            let client_id = text_of(req, PAGE_AIRSYNC, AS_CLIENT_ID);
            return MockResponse::wbxml(&upsync_response(
                "evk2",
                vec![add_ack(&client_id, "srv:ev-new", "1")],
            ));
        }
        if key == "0" {
            events_sync(
                "evk1",
                &[
                    event_item(
                        "srv:ev-1",
                        "Team Sync",
                        "20260818T090000Z",
                        "20260818T100000Z",
                        "uid-one",
                        false,
                    ),
                    event_item(
                        "srv:ev-2",
                        "Weekly Standup",
                        "20260811T090000Z",
                        "20260811T093000Z",
                        "uid-two",
                        true,
                    ),
                ],
            )
        } else if key == "evk1" {
            events_sync(
                "evk3",
                &[event_item(
                    "srv:ev-new",
                    "engine-cli probe",
                    "20260801T010000Z",
                    "20260801T020000Z",
                    "engine-cli-acct-eas-1",
                    false,
                )],
            )
        } else {
            events_sync(&key, &[])
        }
    })
}

/// The contacts mock's whole protocol: OPTIONS negotiates 16.1, FolderSync
/// answers the bootstrap or an empty delta, and class-Contacts Sync answers
/// the collection's bootstrap (`"0"` → two cards), its first incremental
/// (`conk1` → one new card), or an empty steady round.
fn contacts_respond(req: &CapturedRequest, _ordinal: usize) -> MockResponse {
    common_respond(req, |_req, _collection, key| {
        if key == "0" {
            MockResponse::wbxml(&contacts_sync_response(
                "conk1",
                &[
                    ("srv:con-1", "Zhou, Felix", "felixzhou@kylins.local"),
                    ("srv:con-2", "Kerry, Anat", "anat@example.test"),
                ],
            ))
        } else if key == "conk1" {
            MockResponse::wbxml(&contacts_sync_response(
                "conk2",
                &[("srv:con-3", "Ito, Mari", "mari@example.test")],
            ))
        } else {
            MockResponse::wbxml(&contacts_sync_response(&key, &[]))
        }
    })
}

/// The OPTIONS + FolderSync halves every scenario shares, with the Sync
/// half delegated to `events` keyed by the request's collection + key.
fn common_respond(
    req: &CapturedRequest,
    events: impl Fn(&CapturedRequest, String, String) -> MockResponse,
) -> MockResponse {
    if req.method == "OPTIONS" {
        return MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "14.0,14.1,16.0,16.1")
            .with_header("MS-ASProtocolCommands", "Sync,FolderSync,Ping,SendMail");
    }
    let Some(cmd) = req.cmd() else {
        return MockResponse::empty_wbxml();
    };
    let Some(tree) = req.wbxml_tree() else {
        return MockResponse::empty_wbxml();
    };
    match cmd.as_str() {
        "FolderSync" => {
            let key = text_under(&tree, PAGE_FOLDER, FH_SYNC_KEY);
            if key == "0" {
                MockResponse::wbxml(&folder_sync_response(
                    "hier-1",
                    &[
                        ("fid-inbox", "0", "Inbox", "2"),
                        ("fid-cal-1", "0", "Calendar", "8"),
                        ("fid-contacts-1", "0", "Contacts", "9"),
                    ],
                ))
            } else {
                MockResponse::wbxml(&folder_sync_response(&key, &[]))
            }
        }
        "Sync" => {
            let collection = text_under(&tree, PAGE_AIRSYNC, AS_COLLECTION_ID);
            let key = text_under(&tree, PAGE_AIRSYNC, AS_SYNC_KEY);
            events(req, collection, key)
        }
        _ => MockResponse::empty_wbxml(),
    }
}

/// One wire calendar item: subject + UTC stamps + a UID, optionally a
/// 4-count weekly Tuesday recurrence (the `adapter_events_flow` shape).
fn event_item(
    id: &str,
    subject: &str,
    start: &str,
    end: &str,
    uid: &str,
    weekly: bool,
) -> (String, WbxmlElement) {
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
                WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "4"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "4"),
            ],
        ));
    }
    (
        id.to_owned(),
        WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, children),
    )
}

/// A class-Calendar Sync response: key rotation + one Add per item (the
/// `adapter_events_flow::calendar_sync_response` shape, restated for the
/// `(String, …)` item tuples this file builds).
fn events_sync(new_key: &str, items: &[(String, WbxmlElement)]) -> MockResponse {
    let commands: Vec<WbxmlElement> = items
        .iter()
        .map(|(id, app)| {
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
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
    ];
    if !commands.is_empty() {
        collection.push(WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, commands));
    }
    MockResponse::wbxml(&WbxmlElement::container(
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
    ))
}

/// The first text element with `(page, token)` anywhere under `root` (the
/// `engine_cli_flow` helper, restated).
fn text_under(root: &WbxmlElement, page: u8, token: u8) -> String {
    fn find(el: &WbxmlElement, page: u8, token: u8) -> Option<String> {
        if el.page == page
            && el.token == token
            && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(|c| find(c, page, token))
    }
    find(root, page, token).unwrap_or_default()
}
