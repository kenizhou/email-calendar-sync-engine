// SPDX-License-Identifier: MPL-2.0
//! Calendar upsync: build_calendar_change_request golden structure and round-trips.

use super::*;

// ============================================================================
// M8 calendar upsync Task 2 — build_calendar_change_request golden tests
// ([MS-ASSYNC] §2.2.2 / [MS-ASWBXML] §2.1.2.1.1 page 0)
// ============================================================================

/// A minimal-but-valid [`CalendarEventWrite`] for the golden tests. The
/// subject is per-call so a crossed wire between the Add and Replace items
/// cannot hide behind equal props (the crate's fixture convention); the
/// TZI blob is a real fixed-offset UTC+8 blob via the T1 constructor.
fn calendar_write_fixture(subject: &str) -> CalendarEventWrite {
    CalendarEventWrite {
        start_time: "20260821T090000Z".to_string(),
        end_time: "20260821T100000Z".to_string(),
        all_day_event: false,
        time_zone_base64: build_fixed_offset_tzi_base64(480),
        subject: Some(subject.to_string()),
        ..Default::default()
    }
}

/// Walk to the single Collection's `Commands` element and assert the shared
/// envelope: Sync > Collections > Collection with EXACTLY [SyncKey,
/// CollectionId, Commands] as direct children — in particular NO
/// `airsync:Class` (14.0+ rejects it; CollectionId identifies the
/// collection) and NO `GetChanges` (invalid in 16.1), the same gates as the
/// email `build_sync_change_request`. Returns the Commands element.
fn assert_calendar_envelope<'a>(
    tree: &'a WbxmlElement,
    sync_key: &str,
    collection_id: &str,
) -> &'a WbxmlElement {
    assert_eq!((tree.page, tree.token), (PAGE_AIRSYNC, AS_SYNC));
    assert_eq!(tree.children.len(), 1);
    let collections = &tree.children[0];
    assert_eq!(
        (collections.page, collections.token),
        (PAGE_AIRSYNC, AS_COLLECTIONS)
    );
    assert_eq!(collections.children.len(), 1);
    let collection = &collections.children[0];
    assert_eq!(
        (collection.page, collection.token),
        (PAGE_AIRSYNC, AS_COLLECTION)
    );

    // Collection children: SyncKey, CollectionId, Commands — nothing else.
    assert_eq!(collection.children.len(), 3);
    let key = &collection.children[0];
    assert_eq!((key.page, key.token), (PAGE_AIRSYNC, AS_SYNC_KEY));
    assert_eq!(text_value(key).unwrap(), sync_key);
    let cid = &collection.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).unwrap(), collection_id);
    let commands = &collection.children[2];
    assert_eq!((commands.page, commands.token), (PAGE_AIRSYNC, AS_COMMANDS));

    // Class / GetChanges must be absent among the Collection's DIRECT
    // children (Add 0x07 lives deeper, under Commands — walking direct
    // children only keeps the assertion unambiguous).
    assert!(
        collection.children.iter().all(
            |c| !(c.page == PAGE_AIRSYNC && (c.token == AS_CLASS || c.token == AS_GET_CHANGES))
        ),
        "calendar upsync Collection must not carry Class or GetChanges"
    );
    commands
}

/// Golden test (Add): `CalendarChange::Add` emits the wire `airsync:Add`
/// container { ClientId, ApplicationData }, with the ApplicationData being
/// the T1 serializer's output unmodified ([MS-ASSYNC] §2.2.2.1).
#[test]
fn calendar_change_add_matches_golden_structure() {
    let props = calendar_write_fixture("Sprint Review");
    let client_id = new_calendar_client_id();
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Add {
            client_id: client_id.clone(),
            props: props.clone(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let add = &commands.children[0];
    assert_eq!((add.page, add.token), (PAGE_AIRSYNC, AS_ADD));
    assert_eq!(
        add.children.len(),
        2,
        "ClientId + ApplicationData — nothing else"
    );

    let cid_el = &add.children[0];
    assert_eq!((cid_el.page, cid_el.token), (PAGE_AIRSYNC, AS_CLIENT_ID));
    assert_eq!(
        text_value(cid_el).unwrap(),
        client_id,
        "ClientId is the caller's value, verbatim"
    );

    let app_data = &add.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(
        *app_data,
        build_calendar_application_data(&props, "16.1"),
        "ApplicationData must be the T1 serializer's output, unmodified"
    );
}

/// Golden test (Replace): OUR "Replace" vocabulary maps to the wire
/// `airsync:Change` container carrying ServerId ([MS-ASSYNC] §2.2.2 — the
/// Change command updates an existing item): { ServerId, ApplicationData }.
#[test]
fn calendar_change_replace_maps_to_wire_change_with_server_id() {
    let props = calendar_write_fixture("Moved Standup");
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: props.clone(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let change = &commands.children[0];
    assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
    assert_eq!(
        change.children.len(),
        2,
        "ServerId + ApplicationData — nothing else"
    );

    let sid = &change.children[0];
    assert_eq!((sid.page, sid.token), (PAGE_AIRSYNC, AS_SERVER_ID));
    assert_eq!(text_value(sid).unwrap(), "cal7:9");

    let app_data = &change.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(*app_data, build_calendar_application_data(&props, "16.1"));
}

/// Golden test (Remove): OUR "Remove" vocabulary maps to the wire
/// `airsync:Delete` container whose ServerId is a CHILD element
/// ([MS-ASCMD] §2.2.3.42.2) — the server's soft-delete semantics,
/// acceptable for v1 per spec D1.
#[test]
fn calendar_change_remove_maps_to_wire_delete_with_server_id() {
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Remove {
            server_id: "cal7:12".into(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let delete = &commands.children[0];
    assert_eq!((delete.page, delete.token), (PAGE_AIRSYNC, AS_DELETE));
    assert_eq!(
        delete.children.len(),
        1,
        "ServerId only — no ApplicationData"
    );

    let sid = &delete.children[0];
    assert_eq!((sid.page, sid.token), (PAGE_AIRSYNC, AS_SERVER_ID));
    assert_eq!(text_value(sid).unwrap(), "cal7:12");
}

/// A mixed batch [Add, Replace, Remove] preserves input order under
/// Commands, each command carrying its own identifier kind.
#[test]
fn calendar_change_mixed_batch_preserves_order() {
    let changes = vec![
        CalendarChange::Add {
            client_id: "CalAdd-fixed".into(),
            props: calendar_write_fixture("Add Subject"),
        },
        CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: calendar_write_fixture("Replace Subject"),
        },
        CalendarChange::Remove {
            server_id: "cal7:12".into(),
        },
    ];
    let tree = build_calendar_change_request("cal7", "{sk3}", &changes, "16.1");

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 3);
    let tokens: Vec<u8> = commands.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![AS_ADD, AS_CHANGE, AS_DELETE],
        "input order preserved: Add, Replace (→wire Change), Remove (→wire Delete)"
    );

    // Each command's FIRST child carries its identifier (ClientId / ServerId).
    assert_eq!(
        text_value(&commands.children[0].children[0]).unwrap(),
        "CalAdd-fixed"
    );
    assert_eq!(
        text_value(&commands.children[1].children[0]).unwrap(),
        "cal7:9"
    );
    assert_eq!(
        text_value(&commands.children[2].children[0]).unwrap(),
        "cal7:12"
    );
}

/// ClientId discipline: the builder is infallible and never synthesizes or
/// clamps — a caller-supplied value of exactly [`CLIENT_ID_MAX_LEN`] chars
/// (the [MS-ASCMD] cap; Exchange enforces it with Status 103 per task-11
/// live evidence) must be emitted verbatim, and the constructors guarantee
/// the cap by construction.
#[test]
fn calendar_change_client_id_emitted_verbatim_at_40_char_cap() {
    // "CalAdd-" + filler = exactly the cap — the longest accepted value.
    let id = format!("CalAdd-{}", "x".repeat(CLIENT_ID_MAX_LEN - "CalAdd-".len()));
    assert_eq!(id.len(), CLIENT_ID_MAX_LEN);
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Add {
            client_id: id.clone(),
            props: calendar_write_fixture("Cap Test"),
        }],
        "16.1",
    );
    let cid_el = &tree.children[0].children[0].children[2].children[0].children[0];
    assert_eq!((cid_el.page, cid_el.token), (PAGE_AIRSYNC, AS_CLIENT_ID));
    assert_eq!(
        text_value(cid_el).unwrap(),
        id,
        "builder emits the caller's ClientId verbatim — it never synthesizes or clamps"
    );

    // The constructors guarantee the cap: the fixed "CalAdd-" prefix
    // (7 + 32-hex uuid = 39 ≤ 40), and the shared clamp still yields
    // exactly CLIENT_ID_MAX_LEN for an over-long prefix input.
    let synthesized = new_calendar_client_id();
    assert!(synthesized.len() <= CLIENT_ID_MAX_LEN);
    assert!(synthesized.starts_with("CalAdd-"));
    let clamped = new_send_client_id(&"P".repeat(100));
    assert_eq!(clamped.len(), CLIENT_ID_MAX_LEN);
}

/// The mixed calendar upsync request survives a WBXML serialize/deserialize
/// round trip — the page-4 ApplicationData children exercise the
/// SWITCH_PAGE path in both directions.
#[test]
fn calendar_change_request_round_trips() {
    let changes = vec![
        CalendarChange::Add {
            client_id: new_calendar_client_id(),
            props: calendar_write_fixture("RT Add"),
        },
        CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: calendar_write_fixture("RT Replace"),
        },
        CalendarChange::Remove {
            server_id: "cal7:12".into(),
        },
    ];
    let tree = build_calendar_change_request("cal7", "{sk3}", &changes, "16.1");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

// ============================================================================
// P2 Task 3 — the Exceptions container + FirstDayOfWeek goldens
// ============================================================================

use provider_eas::{
    calendar::{
        CAL_DELETED, CAL_END_TIME, CAL_EXCEPTION, CAL_EXCEPTION_START_TIME, CAL_EXCEPTIONS,
        CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_WEEK, CAL_RECURRENCE_FIRST_DAY_OF_WEEK,
        CAL_RECURRENCE_TYPE, CAL_START_TIME, CAL_SUBJECT, CalendarException, CalendarRecurrence,
        PAGE_CALENDAR,
    },
    wbxml::tags::{base, pages},
};

/// The `Exceptions` container lands after `Recurrence` inside the 16.1
/// ApplicationData, mirroring the parse side's shapes: a deleted marker is
/// `ExceptionStartTime` + `Deleted` and NOTHING else ([MS-ASCAL] §2.2.2.16),
/// a modified occurrence carries `ExceptionStartTime` then its changed
/// subset, with Location in the 16.x `airsyncbase:Location` container form
/// inside exceptions exactly as at the top level.
#[test]
fn exceptions_container_matches_the_parse_side_shapes() {
    let mut props = calendar_write_fixture("Weekly Standup");
    props.exceptions = vec![
        CalendarException {
            deleted: true,
            exception_start_time: Some("20260818T010000Z".to_owned()),
            ..CalendarException::default()
        },
        CalendarException {
            exception_start_time: Some("20260825T010000Z".to_owned()),
            start_time: Some("20260825T030000Z".to_owned()),
            end_time: Some("20260825T033000Z".to_owned()),
            subject: Some("Late standup".to_owned()),
            location: Some("Room 9".to_owned()),
            body_plain: Some("Moved".to_owned()),
            ..CalendarException::default()
        },
    ];
    let app_data = build_calendar_application_data(&props, "16.1");
    let exceptions = app_data
        .children
        .iter()
        .find(|c| c.page == PAGE_CALENDAR && c.token == CAL_EXCEPTIONS)
        .expect("the Exceptions container rides the ApplicationData");
    assert_eq!(exceptions.children.len(), 2);

    // Deleted marker: ExceptionStartTime + Deleted only.
    let gone = &exceptions.children[0];
    assert_eq!((gone.page, gone.token), (PAGE_CALENDAR, CAL_EXCEPTION));
    assert_eq!(gone.children.len(), 2, "a deleted marker carries no data");
    assert_eq!(
        (gone.children[0].page, gone.children[0].token),
        (PAGE_CALENDAR, CAL_EXCEPTION_START_TIME)
    );
    assert_eq!(text_value(&gone.children[0]).unwrap(), "20260818T010000Z");
    assert_eq!(
        (gone.children[1].page, gone.children[1].token),
        (PAGE_CALENDAR, CAL_DELETED)
    );
    assert_eq!(text_value(&gone.children[1]).unwrap(), "1");

    // Modified occurrence: the changed subset, Location as the 16.x
    // airsyncbase container (the note-2 gate applies inside exceptions too).
    let moved = &exceptions.children[1];
    let tokens: Vec<(u8, u8)> = moved.children.iter().map(|c| (c.page, c.token)).collect();
    assert_eq!(
        tokens,
        vec![
            (PAGE_CALENDAR, CAL_EXCEPTION_START_TIME),
            (PAGE_CALENDAR, CAL_START_TIME),
            (PAGE_CALENDAR, CAL_END_TIME),
            (PAGE_CALENDAR, CAL_SUBJECT),
            (pages::BASE, base::LOCATION),
            (pages::BASE, base::BODY),
        ],
        "ExceptionStartTime first, then the changed subset in the top-level \
         builder's order"
    );
    let location = &moved.children[4];
    assert_eq!(location.children.len(), 1);
    assert_eq!(
        (location.children[0].page, location.children[0].token),
        (pages::BASE, base::DISPLAY_NAME)
    );
    assert_eq!(text_value(&location.children[0]).unwrap(), "Room 9");
}

/// An empty exceptions list omits the container entirely — the parse side's
/// absent-element reading, not an empty container.
#[test]
fn an_empty_exceptions_list_omits_the_container() {
    let props = calendar_write_fixture("Plain");
    let app_data = build_calendar_application_data(&props, "16.1");
    assert!(
        app_data
            .children
            .iter()
            .all(|c| !(c.page == PAGE_CALENDAR && c.token == CAL_EXCEPTIONS)),
        "no Exceptions container when the list is empty"
    );
}

/// `FirstDayOfWeek` rides the Recurrence container when set ([MS-ASCAL]
/// §2.2.2.24, 14.0+) — after MonthOfYear, before the bound elements — and
/// is absent otherwise (the Monday default is never written).
#[test]
fn recurrence_first_day_of_week_rides_when_set() {
    let mut props = calendar_write_fixture("Week Start");
    props.recurrence = Some(CalendarRecurrence {
        recurrence_type: 1,
        day_of_week: Some(4),
        first_day_of_week: Some(0),
        ..CalendarRecurrence::default()
    });
    let app_data = build_calendar_application_data(&props, "16.1");
    let recurrence = app_data
        .children
        .iter()
        .find(|c| c.page == PAGE_CALENDAR && c.token == CAL_RECURRENCE)
        .expect("the Recurrence container rides");
    let tokens: Vec<(u8, u8)> = recurrence
        .children
        .iter()
        .map(|c| (c.page, c.token))
        .collect();
    assert_eq!(
        tokens,
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK),
            (PAGE_CALENDAR, CAL_RECURRENCE_FIRST_DAY_OF_WEEK),
        ],
        "Type, DayOfWeek, FirstDayOfWeek — Sunday reads as 0"
    );
    assert_eq!(
        text_value(&recurrence.children[2]).unwrap(),
        "0",
        "Sunday is the SYSTEMTIME wDayOfWeek index 0"
    );
}
