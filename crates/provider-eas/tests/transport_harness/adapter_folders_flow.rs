// SPDX-License-Identifier: MPL-2.0
//! Adapter `sync_mailboxes` scenarios ([MS-ASFolderSync]): the FolderSync verb
//! mapped onto the engine's `ScopeSync<Mailbox>` — the bootstrap "0" snapshot,
//! the incremental delta against the rotated key, the status-9 invalidation
//! recovery, and the cursor threading between calls. The wire side rides the
//! real transport against the mock server; the trait side is the thing under
//! test.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, MailboxRole},
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{Capabilities, Provider as _};
use provider_eas::{
    adapter::EasAdapter,
    commands::{
        FH_ADD, FH_CHANGES, FH_DELETE, FH_DISPLAY_NAME, FH_FOLDER_SYNC, FH_PARENT_ID, FH_SERVER_ID,
        FH_STATUS, FH_SYNC_KEY, FH_TYPE, FH_UPDATE, PAGE_FOLDER,
    },
    wbxml::WbxmlElement,
};

use super::{
    fixtures::{folder_sync_response, folder_sync_status},
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

/// The `<SyncKey>` text of a decoded FolderSync request tree.
fn request_sync_key(req: &CapturedRequest) -> String {
    let tree = req.wbxml_tree().expect("request body decodes");
    tree.children
        .iter()
        .find(|c| c.token == FH_SYNC_KEY)
        .and_then(|c| match &c.value {
            provider_eas::wbxml::WbxmlValue::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no SyncKey in request tree"))
}

/// One `(server_id, parent_id, display_name, type)` folder Add/Update.
type FolderEntry = (&'static str, &'static str, &'static str, &'static str);

fn change_element(token: u8, entry: &FolderEntry) -> WbxmlElement {
    let &(id, parent, name, typ) = entry;
    WbxmlElement::container(
        PAGE_FOLDER,
        token,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, id),
            WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, parent),
            WbxmlElement::text(PAGE_FOLDER, FH_DISPLAY_NAME, name),
            WbxmlElement::text(PAGE_FOLDER, FH_TYPE, typ),
        ],
    )
}

/// A FolderSync response with Add/Update/Delete changes mixed — the delta
/// vocabulary beyond `fixtures::folder_sync_response`'s adds-only shape,
/// built inline with the public tag constants (same convention).
fn folder_sync_delta(
    new_key: &str,
    adds: &[FolderEntry],
    updates: &[FolderEntry],
    deletes: &[&str],
) -> WbxmlElement {
    let mut changes: Vec<WbxmlElement> = adds.iter().map(|e| change_element(FH_ADD, e)).collect();
    changes.extend(updates.iter().map(|e| change_element(FH_UPDATE, e)));
    changes.extend(deletes.iter().map(|id| {
        WbxmlElement::container(
            PAGE_FOLDER,
            FH_DELETE,
            vec![WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, *id)],
        )
    }));
    let mut children = vec![
        WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
        WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, new_key),
    ];
    if !changes.is_empty() {
        children.push(WbxmlElement::container(PAGE_FOLDER, FH_CHANGES, changes));
    }
    WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, children)
}

/// A FolderSync success with a Status but NO SyncKey and no Changes — the
/// shape that must never advance the cursor to an empty string.
fn folder_sync_no_key() -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1")],
    )
}

/// The bootstrap shape: `cursor: None` sends the "0" key, and the full
/// hierarchy the bootstrap round returns is a **snapshot** — every mail-class
/// folder an object, their keys the `present` set. Non-mail folders (the
/// Calendar/Contacts/Tasks classes) belong to the calendar/contacts scopes,
/// not the mail container list, so they are filtered out of both.
#[tokio::test]
async fn fresh_sync_bootstraps_from_zero_and_returns_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        MockResponse::wbxml(&folder_sync_response(
            "hier-1",
            &[
                ("fid-inbox", "0", "Inbox", "2"),
                ("fid-drafts", "0", "Drafts", "3"),
                ("fid-trash", "0", "Deleted Items", "4"),
                ("fid-sent", "0", "Sent Items", "5"),
                ("fid-outbox", "0", "Outbox", "6"),
                ("fid-arch", "fid-inbox", "Archive", "1"),
                ("fid-cal", "0", "Calendar", "8"),
                ("fid-contacts", "0", "Contacts", "9"),
            ],
        ))
    }) as Handler);
    let adapter = adapter_at(&server);

    let sync = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("bootstrap FolderSync succeeds");
    assert_eq!(
        request_sync_key(&server.request(1)),
        "0",
        "cursor None bootstraps from the \"0\" hierarchy key"
    );
    assert_eq!(server.count(), 1, "one round for a fresh sync");
    assert_eq!(
        sync.next_cursor.as_str(),
        "hier-1",
        "the rotated hierarchy key is the cursor to persist"
    );

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!(
            "a bootstrap round must read as a snapshot, got {:?}",
            sync.update
        );
    };
    let by_id: Vec<(&str, &str, Option<MailboxId>, Option<MailboxRole>)> = objects
        .iter()
        .map(|m: &Mailbox| {
            (
                m.id.as_str(),
                m.name.as_str(),
                m.parent.clone(),
                m.role.clone(),
            )
        })
        .collect();
    assert_eq!(
        by_id,
        vec![
            ("fid-inbox", "Inbox", None, Some(MailboxRole::Inbox)),
            ("fid-drafts", "Drafts", None, Some(MailboxRole::Drafts)),
            ("fid-trash", "Deleted Items", None, Some(MailboxRole::Trash)),
            ("fid-sent", "Sent Items", None, Some(MailboxRole::Sent)),
            ("fid-outbox", "Outbox", None, None),
            (
                "fid-arch",
                "Archive",
                Some(MailboxId::try_from("fid-inbox").unwrap()),
                None
            ),
        ],
        "mail folders map with type-derived roles and parent ids; non-mail classes are filtered"
    );
    let keys: Vec<&str> = present.iter().map(ProviderKey::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "fid-arch",
            "fid-drafts",
            "fid-inbox",
            "fid-outbox",
            "fid-sent",
            "fid-trash"
        ],
        "present is exactly the mail folders' keys (BTreeSet order)"
    );
    // The verb ladder: the mail bit promises the whole mail read surface
    // (containers + messages), so it stays off until stream_email lands.
    assert_eq!(
        adapter.connection_info().capabilities,
        Capabilities::none(),
        "mail flips with the message verbs, not with FolderSync alone"
    );
}

/// The incremental shape: the second call sends the key the first call's
/// `next_cursor` returned, and the delta's Add/Update/Delete elements map to
/// `changed`/`removed` — no snapshot claim, so no tombstoning beyond the
/// explicit deletions.
#[tokio::test]
async fn incremental_round_sends_the_advanced_key_and_maps_the_delta() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&folder_sync_response(
                "hier-1",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-old", "0", "Old", "1"),
                ],
            )),
            2 => MockResponse::wbxml(&folder_sync_delta(
                "hier-2",
                &[("fid-arch", "fid-inbox", "Archive", "1")],
                &[("fid-inbox", "0", "Inbox Renamed", "2")],
                &["fid-old"],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("bootstrap round");
    assert_eq!(first.next_cursor.as_str(), "hier-1");

    let second = adapter
        .sync_mailboxes(&account(), Some(&first.next_cursor))
        .await
        .expect("incremental round");
    assert_eq!(
        request_sync_key(&server.request(2)),
        "hier-1",
        "the second round sends the key the first round's cursor advanced to"
    );
    assert_eq!(second.next_cursor.as_str(), "hier-2");

    let SyncUpdate::Delta {
        changed, removed, ..
    } = &second.update
    else {
        panic!(
            "an incremental round must read as a delta, got {:?}",
            second.update
        );
    };
    assert_eq!(
        changed
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str()))
            .collect::<Vec<_>>(),
        vec![("fid-arch", "Archive"), ("fid-inbox", "Inbox Renamed")],
        "Add and Update both land as whole changed objects (response order)"
    );
    assert_eq!(
        removed,
        &vec![ProviderKey::new("fid-old").unwrap()],
        "a Delete element becomes an explicit removed key"
    );
}

/// Status 9 ("folder hierarchy out of date") is the EAS
/// `cannotCalculateChanges`: the stored key can never produce a delta again.
/// The adapter recovers inside the call — re-issuing FolderSync from the
/// bootstrap "0" and returning the full hierarchy as a snapshot — the JMAP
/// needs-resync-falls-back-to-snapshot precedent, so the caller only ever
/// sees one healthy `ScopeSync`.
#[tokio::test]
async fn an_invalidated_hierarchy_key_reboots_from_zero_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&folder_sync_response(
                "hier-1",
                &[("fid-inbox", "0", "Inbox", "2")],
            )),
            2 => MockResponse::wbxml(&folder_sync_status("9")),
            3 => MockResponse::wbxml(&folder_sync_response(
                "hier-2",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-new", "0", "New Folder", "1"),
                ],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = adapter_at(&server);

    let first = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("bootstrap round");
    assert_eq!(first.next_cursor.as_str(), "hier-1");

    let recovered = adapter
        .sync_mailboxes(&account(), Some(&first.next_cursor))
        .await
        .expect("status 9 recovers inside the call");
    assert_eq!(
        request_sync_key(&server.request(2)),
        "hier-1",
        "the stale key goes out first, exactly as the cursor said"
    );
    assert_eq!(
        request_sync_key(&server.request(3)),
        "0",
        "the recovery round re-bootstraps from \"0\""
    );
    assert_eq!(server.count(), 3, "exactly one recovery round — no loop");
    assert_eq!(recovered.next_cursor.as_str(), "hier-2");
    assert!(
        recovered.is_snapshot(),
        "the recovery result is the full hierarchy, so the store can tombstone anything absent"
    );
    assert_eq!(recovered.update.changed().len(), 2);
}

/// The cursor-contract guard ([MS-ASCMD] Sync's empty-body precedent): a
/// success response that carries NO SyncKey must not advance the persisted
/// cursor to the empty string — the request's key is echoed back, exactly like
/// the client's no-changes result.
#[tokio::test]
async fn a_success_without_a_sync_key_keeps_the_request_key_as_cursor() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&folder_sync_no_key())
    }) as Handler);
    let adapter = adapter_at(&server);

    let boot = adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("status 1 without a key still succeeds");
    assert_eq!(
        boot.next_cursor.as_str(),
        "0",
        "no key in the response → the bootstrap request key stays the cursor"
    );

    let again = adapter
        .sync_mailboxes(&account(), Some(&SyncState::new("hier-5")))
        .await
        .expect("second round succeeds");
    assert_eq!(
        again.next_cursor.as_str(),
        "hier-5",
        "no key in the response → the request key is echoed"
    );
}

/// FolderSync failures surface in the engine's classification, never as raw
/// protocol errors: an in-body status the classifier marks permanent (108,
/// invalid device id) is `Permanent`; an HTTP 5xx that escaped the transport
/// is `Retryable` — the poll loop owns the retry.
#[tokio::test]
async fn folder_sync_failures_surface_in_engine_classes() {
    super::harness::init_logger();
    let status_server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&folder_sync_status("108"))
    }) as Handler);
    let err = adapter_at(&status_server)
        .sync_mailboxes(&account(), None)
        .await
        .expect_err("status 108 must surface");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(
        err.to_string().contains('1') && err.to_string().contains("08"),
        "the surfaced detail names the protocol failure: {err}"
    );

    let http_server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let err = adapter_at(&http_server)
        .sync_mailboxes(&account(), None)
        .await
        .expect_err("HTTP 500 must surface");
    assert_eq!(err.class(), engine_core::error::FailureClass::Retryable);
}
