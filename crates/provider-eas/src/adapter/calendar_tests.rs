// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the calendar read slice (`calendar.rs`) — the `#[path]`
//! split the repo uses to hold the 500-line cap (the `email_tests.rs`
//! precedent). The wire-level scenarios live in
//! `tests/transport_harness/adapter_calendar_flow.rs`.

use super::*;

fn wire_folder(server_id: &str, class: &str, typ: Option<u8>) -> EasFolder {
    EasFolder {
        server_id: server_id.to_owned(),
        parent_id: "0".to_owned(),
        display_name: format!("Name of {server_id}"),
        class: class.to_owned(),
        folder_type: typ,
    }
}

/// Only the Calendar class lands in the calendar container scope: mail
/// folders, contacts, tasks, and the classless shape (a missing Type element
/// parses classless — mail by the mail slice's default) are all excluded.
#[test]
fn only_calendar_class_folders_map_to_calendars() {
    let folders = vec![
        wire_folder("fid-cal-1", "Calendar", Some(8)),
        wire_folder("fid-inbox", "Email", Some(2)),
        wire_folder("fid-contacts", "Contacts", Some(9)),
        wire_folder("fid-tasks", "Tasks", Some(7)),
        wire_folder("fid-typeless", "", None),
    ];
    let mapped = calendars(&folders);
    let ids: Vec<&str> = mapped.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["fid-cal-1"]);
}

/// A calendar folder maps with its ServerId as the stable id, the display
/// name verbatim, and the EAS-native class/type facts under the adapter's
/// extended namespace.
#[test]
fn a_calendar_folder_maps_with_native_facts() {
    let folder = wire_folder("fid-cal-1", "Calendar", Some(8));
    let mapped = &calendars(&[folder])[0];
    assert_eq!(mapped.id.as_str(), "fid-cal-1");
    assert_eq!(mapped.name, "Name of fid-cal-1");
    assert_eq!(
        mapped.extended.get("eas/class"),
        Some(&serde_json::json!("Calendar"))
    );
    assert_eq!(
        mapped.extended.get("eas/folder-type"),
        Some(&serde_json::json!(8u8))
    );
}

/// A folder whose ServerId cannot key a CalendarId (empty) is skipped with a
/// warning, never failing the round.
#[test]
fn an_unkeyable_calendar_folder_is_skipped() {
    let folders = vec![
        wire_folder("", "Calendar", Some(8)),
        wire_folder("fid-cal-2", "Calendar", Some(8)),
    ];
    let mapped = calendars(&folders);
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].id.as_str(), "fid-cal-2");
}

/// An adapter built without a calendar binding refuses event sync with
/// `InvalidState` — and its capabilities never advertise the calendar
/// family, so a capability-checking caller never reaches the refusal.
#[test]
fn an_unbound_adapter_refuses_event_sync_as_invalid_state() {
    assert_eq!(
        unbound_calendar().class(),
        engine_core::error::FailureClass::InvalidState
    );
}
