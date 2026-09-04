// SPDX-License-Identifier: MPL-2.0
//! The shared hierarchy-SyncKey ledger scenarios (P2 Task 3): interleaved
//! mail + calendar container passes over ONE adapter share the account's
//! hierarchy cursor instead of invalidating each other into status-9 full
//! re-enumerations — and the rows a riding scope missed (its class's
//! folders, the class-less deletions, another scope's snapshot present-set)
//! ride the ledger's backlog so nothing is skipped.

use std::sync::Arc;

use engine_core::{ids::ProviderKey, sync::SyncUpdate};
use engine_provider::Provider as _;
use provider_eas::commands::{FH_SYNC_KEY, PAGE_FOLDER};

use super::{
    adapter_calendar_flow::{account, adapter_at},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The `<SyncKey>` text inside a FolderSync request.
fn folder_sync_key(req: &CapturedRequest) -> String {
    fn walk(el: &provider_eas::wbxml::WbxmlElement) -> Option<String> {
        if (el.page, el.token) == (PAGE_FOLDER, FH_SYNC_KEY)
            && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(walk)
    }
    walk(&req.wbxml_tree().expect("the request is WBXML")).expect("a SyncKey rides")
}

/// The interleaved fan-out on one calendar-bound adapter: the mail
/// bootstrap seeds the shared key, the calendar pass RIDES it (no second
/// bootstrap, no status 9) and its result carries both the spare calendar
/// row from the mail round and its own delta row — snapshot-shaped, since
/// its authority set is the mail bootstrap's present-set.
#[tokio::test]
async fn interleaved_container_passes_share_one_hierarchy_key() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            // Mail bootstrap: the full hierarchy — one mail folder, one
            // calendar folder.
            1 => MockResponse::wbxml(&super::fixtures::folder_sync_response(
                "hier-1",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-cal-1", "0", "Calendar", "8"),
                ],
            )),
            // Calendar rides hier-1: a delta naming another calendar folder.
            2 => MockResponse::wbxml(&super::adapter_folders_flow::folder_sync_delta(
                "hier-2",
                &[("fid-cal-2", "0", "Work Calendar", "8")],
                &[],
                &[],
            )),
            // Mail rides hier-2: an empty delta closes the loop.
            3 => MockResponse::wbxml(&super::adapter_folders_flow::folder_sync_delta(
                "hier-3",
                &[],
                &[],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let mail = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("the mail bootstrap lands");
    assert_eq!(mail.next_cursor.as_str(), "hier-1");
    assert!(mail.is_snapshot());

    let calendars = adapter
        .sync_calendars(&account(), None)
        .await
        .expect("the calendar pass rides the shared key");
    // THE ledger claim: request 2 carried hier-1 — not "0" — so the server
    // never saw a stale key and never answered status 9.
    assert_eq!(
        folder_sync_key(&server.request(2)),
        "hier-1",
        "the calendar pass rides the mail round's rotated key"
    );
    assert_eq!(server.count(), 2, "no status-9 recovery round happened");
    assert_eq!(calendars.next_cursor.as_str(), "hier-2");
    // Snapshot-shaped (the authority set is the mail bootstrap's
    // present-set), carrying BOTH calendar folders: the spare from the mail
    // round and the delta row.
    let SyncUpdate::Snapshot { objects, present } = &calendars.update else {
        panic!(
            "a present-backlog round reads as a snapshot, got {:?}",
            calendars.update
        )
    };
    let ids: Vec<&str> = objects.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["fid-cal-1", "fid-cal-2"]);
    let keys: Vec<&str> = present.iter().map(ProviderKey::as_str).collect();
    assert_eq!(keys, vec!["fid-cal-1", "fid-cal-2"]);

    // And back: the mail pass rides hier-2 without a bootstrap.
    let mail_again = adapter
        .sync_mailboxes(&account(), Some(&mail.next_cursor))
        .await
        .expect("the mail pass rides the shared key");
    assert_eq!(folder_sync_key(&server.request(3)), "hier-2");
    assert_eq!(server.count(), 3);
    assert!(
        !mail_again.is_snapshot(),
        "a plain delta round stays a delta (no present-backlog involved)"
    );
}

/// The reverse interleave: the calendar pass bootstraps first, and the mail
/// pass's delta result carries the spare mail row the calendar round saw.
#[tokio::test]
async fn the_reverse_interleave_carries_the_mail_spare() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&super::fixtures::folder_sync_response(
                "hier-1",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-cal-1", "0", "Calendar", "8"),
                ],
            )),
            2 => MockResponse::wbxml(&super::adapter_folders_flow::folder_sync_delta(
                "hier-2",
                &[("fid-archive", "fid-inbox", "Archive", "1")],
                &[],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_calendars(&account(), None)
        .await
        .expect("the calendar bootstrap lands");
    assert_eq!(first.next_cursor.as_str(), "hier-1");

    let mail = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("the mail pass rides the shared key");
    assert_eq!(
        folder_sync_key(&server.request(2)),
        "hier-1",
        "no second bootstrap"
    );
    let SyncUpdate::Snapshot { objects, .. } = &mail.update else {
        panic!("the mail round consumed the calendar bootstrap's present-set")
    };
    let ids: Vec<&str> = objects.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["fid-inbox", "fid-archive"],
        "the spare mail row (Inbox, from the calendar bootstrap) rides ahead of the delta row"
    );
}

/// A deletion another scope's round carried rides the backlog too: the
/// class-less Delete element pends for the riding scope, whose next delta
/// applies it even though the server's cursor moved past it.
#[tokio::test]
async fn cross_scope_deletions_ride_the_backlog() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&super::fixtures::folder_sync_response(
                "hier-1",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-cal-1", "0", "Calendar", "8"),
                    ("fid-cal-9", "0", "Old Calendar", "8"),
                ],
            )),
            // A mail delta that deletes a CALENDAR folder (the wire's Delete
            // carries no class — any folder).
            2 => MockResponse::wbxml(&super::adapter_folders_flow::folder_sync_delta(
                "hier-2",
                &[],
                &[],
                &["fid-cal-9"],
            )),
            // The calendar pass's own delta: no rows.
            3 => MockResponse::wbxml(&super::adapter_folders_flow::folder_sync_delta(
                "hier-3",
                &[],
                &[],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let mail = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("bootstrap");
    assert_eq!(mail.next_cursor.as_str(), "hier-1");
    let mail = adapter
        .sync_mailboxes(&account(), Some(&mail.next_cursor))
        .await
        .expect("the mail delta lands");
    assert_eq!(
        mail.update.removed(),
        &vec![ProviderKey::new("fid-cal-9").unwrap()],
        "the mail delta applies the class-less deletion itself"
    );

    let calendars = adapter
        .sync_calendars(&account(), None)
        .await
        .expect("the calendar pass rides the shared key");
    assert_eq!(folder_sync_key(&server.request(3)), "hier-2");
    let SyncUpdate::Snapshot { present, .. } = &calendars.update else {
        panic!("the present-backlog round reads as a snapshot")
    };
    let keys: Vec<&str> = present.iter().map(ProviderKey::as_str).collect();
    assert_eq!(
        keys,
        vec!["fid-cal-1"],
        "fid-cal-9 is gone from the authority set — the deletion rode the backlog"
    );
}
