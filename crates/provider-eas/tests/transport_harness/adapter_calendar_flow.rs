// SPDX-License-Identifier: MPL-2.0
//! Adapter calendar-container downsync scenarios (P2 Task 2):
//! `sync_calendars` — FolderSync filtered to the Calendar class — mapped
//! onto the engine's `ScopeSync<Calendar>` (the bootstrap snapshot and the
//! incremental delta). The event-sync scenarios live in
//! `adapter_events_flow.rs` (the 500-line split); the shared helpers below
//! serve both.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, CalendarId, MailboxId, ProviderKey},
    sync::SyncUpdate,
};
use engine_provider::{Capabilities, Provider as _};
use provider_eas::adapter::EasAdapter;

use super::{
    adapter_folders_flow::folder_sync_delta,
    fixtures::folder_sync_response,
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

pub(crate) fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

pub(crate) fn calendar_id() -> CalendarId {
    CalendarId::try_from("fid-cal-1").unwrap()
}

/// The calendar-bound adapter under test: bound to the calendar folder
/// `fid-cal-1` for events (the Graph `placeholder` discovery pattern — one
/// ServerId, the calendar view of it).
pub(crate) fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(
        client_at(&server.eas_url()),
        MailboxId::try_from("fid-cal-1").unwrap(),
    )
    .with_calendar(calendar_id())
}
/// The container snapshot: FolderSync bootstraps from "0" and the Calendar
/// class (folder type 8) — and only it — lands in the `EasCalendarList`
/// scope. The `calendars` capability bit follows the binding: a plain
/// mail-bound adapter does not advertise it, a `with_calendar` one does.
#[tokio::test]
async fn calendar_containers_bootstrap_from_zero_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        MockResponse::wbxml(&folder_sync_response(
            "hier-1",
            &[
                ("fid-inbox", "0", "Inbox", "2"),
                ("fid-cal-1", "0", "Calendar", "8"),
                ("fid-cal-2", "0", "Work Calendar", "8"),
                ("fid-contacts", "0", "Contacts", "9"),
            ],
        ))
    }) as Handler);
    let adapter = adapter_at(&server);

    let sync = adapter
        .sync_calendars(&account(), None)
        .await
        .expect("bootstrap FolderSync succeeds");
    assert_eq!(sync.next_cursor.as_str(), "hier-1");
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!(
            "a bootstrap round must read as a snapshot, got {:?}",
            sync.update
        );
    };
    let names: Vec<(&str, &str)> = objects
        .iter()
        .map(|cal| (cal.id.as_str(), cal.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![("fid-cal-1", "Calendar"), ("fid-cal-2", "Work Calendar")],
        "only the Calendar class (type 8) lands in the calendar container scope"
    );
    let keys: Vec<&str> = present.iter().map(ProviderKey::as_str).collect();
    assert_eq!(keys, vec!["fid-cal-1", "fid-cal-2"]);

    // The verb ladder: `calendars` promises the calendar read surface, which
    // is live exactly when the adapter is calendar-bound.
    assert!(
        adapter.connection_info().capabilities.calendars(),
        "a calendar-bound adapter advertises the calendars bit"
    );
    let mail_only = EasAdapter::new(
        client_at(&server.eas_url()),
        MailboxId::try_from("fid-inbox").unwrap(),
    );
    assert_eq!(
        mail_only.connection_info().capabilities,
        Capabilities::none()
            .with_mail()
            .with_message_source()
            .with_mail_writes()
            .with_submission()
            .with_scheduling_submission(),
        "an unbound adapter keeps advertising exactly the mail family"
    );
}

/// The incremental container delta: the rotated hierarchy key goes out, and
/// the wire's Add/Update/Delete elements map to `changed`/`removed`.
#[tokio::test]
async fn calendar_containers_incremental_delta_maps_changes_and_deletes() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&folder_sync_response(
                "hier-1",
                &[("fid-cal-1", "0", "Calendar", "8")],
            )),
            2 => MockResponse::wbxml(&folder_sync_delta(
                "hier-2",
                &[("fid-cal-9", "0", "Project X", "8")],
                &[],
                &["fid-cal-1"],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_calendars(&account(), None)
        .await
        .expect("bootstrap");
    let second = adapter
        .sync_calendars(&account(), Some(&first.next_cursor))
        .await
        .expect("incremental");
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &second.update
    else {
        panic!("an incremental round must read as a delta");
    };
    assert_eq!(
        changed
            .iter()
            .map(|cal| (cal.id.as_str(), cal.name.as_str()))
            .collect::<Vec<_>>(),
        vec![("fid-cal-9", "Project X")]
    );
    assert_eq!(removed, &vec![ProviderKey::new("fid-cal-1").unwrap()]);
    assert_eq!(second.next_cursor.as_str(), "hier-2");
}
