// SPDX-License-Identifier: MPL-2.0
//! Adapter contacts READ scenarios (P2 Task 5): address-book discovery
//! (FolderSync type 9 through the shared hierarchy ledger) and card sync
//! (snapshot, delta, SyncKey-invalidation resync) against the offline
//! mock server. The conversion goldens live in `src/contacts/`; the
//! upsync request shapes in `tests/commands_sync/contacts_write.rs`;
//! the write scenarios in `adapter_contacts_write_flow.rs` (the
//! 500-line split); the shared fixtures below serve both.

use std::sync::Arc;

use engine_core::{
    ids::{AddressBookId, ContactId, MailboxId},
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{Capabilities, ContactSourceSync, ContactsProvider as _, Provider as _};
use provider_eas::adapter::EasAdapter;
use provider_eas::commands::{
    AS_ADD, AS_APPLICATION_DATA, AS_COLLECTION, AS_COLLECTION_ID, AS_COLLECTIONS, AS_COMMANDS,
    AS_SERVER_ID, AS_STATUS, AS_SYNC, AS_SYNC_KEY, PAGE_AIRSYNC,
};
use provider_eas::contacts::{CON_EMAIL_1, CON_FILE_AS, PAGE_CONTACTS};
use provider_eas::wbxml::WbxmlElement;

use super::{
    adapter_calendar_flow::account,
    adapter_calendar_write_flow::text_of,
    adapter_folders_flow::folder_sync_delta,
    fixtures::folder_sync_response,
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

pub(crate) fn book() -> AddressBookId {
    AddressBookId::try_from("fid-contacts-1").unwrap()
}

/// The contacts-bound adapter under test: bound to the contact folder
/// `fid-contacts-1` for cards (one ServerId serving both bindings, the
/// `contacts_adapter` role shape).
pub(crate) fn contacts_adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::contacts_adapter(client_at(&server.eas_url()), book())
}

// ---------------------------------------------------------------------------
// sync_address_books — FolderSync type 9 through the shared ledger
// ---------------------------------------------------------------------------

/// The container snapshot: FolderSync bootstraps from "0" and the
/// Contacts class (folder type 9) — and only it — lands in the
/// `EasContactList` scope. The `contacts`/`contact_writes` bits follow
/// the binding; an unbound adapter keeps the mail family exactly.
#[tokio::test]
async fn contact_containers_bootstrap_from_zero_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        MockResponse::wbxml(&folder_sync_response(
            "hier-1",
            &[
                ("fid-inbox", "0", "Inbox", "2"),
                ("fid-cal-1", "0", "Calendar", "8"),
                ("fid-contacts-1", "0", "Contacts", "9"),
                ("fid-contacts-2", "0", "Team Book", "14"),
            ],
        ))
    }) as Handler);
    let adapter = contacts_adapter_at(&server);

    let result = adapter
        .sync_address_books(&account(), None)
        .await
        .expect("bootstrap FolderSync succeeds");
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("the contacts source is available");
    };
    assert_eq!(sync.next_cursor.as_str(), "hier-1");
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!(
            "a bootstrap round must read as a snapshot, got {:?}",
            sync.update
        );
    };
    let names: Vec<(&str, &str, bool)> = objects
        .iter()
        .map(|book| (book.id.as_str(), book.name.as_str(), book.is_default))
        .collect();
    assert_eq!(
        names,
        vec![
            ("fid-contacts-1", "Contacts", true),
            ("fid-contacts-2", "Team Book", false),
        ],
        "only the Contacts class (type 9/14) lands in the contacts container scope"
    );
    let keys: Vec<&str> = present
        .iter()
        .map(engine_core::ids::ProviderKey::as_str)
        .collect();
    assert_eq!(keys, vec!["fid-contacts-1", "fid-contacts-2"]);

    // The verb ladder: the contacts bits are live exactly with the binding.
    let capabilities = adapter.connection_info().capabilities;
    assert!(capabilities.contacts());
    assert!(capabilities.contact_writes());
    assert_eq!(
        capabilities.contact_write_guard(),
        Some(engine_provider::WriteGuard::Absent)
    );
    assert!(
        !capabilities.contact_photos(),
        "the photo verb did not land"
    );
    assert!(!capabilities.contact_groups());
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

/// The three-scope interleave: the mail bootstrap seeds the shared key,
/// the CONTACTS pass rides it (no second bootstrap, no status 9) and its
/// result carries the spare contacts row from the mail round plus its own
/// delta — the third container scope wired into the same ledger.
#[tokio::test]
async fn the_contacts_scope_rides_the_shared_hierarchy_key() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("FolderSync"));
        match ordinal {
            1 => MockResponse::wbxml(&folder_sync_response(
                "hier-1",
                &[
                    ("fid-inbox", "0", "Inbox", "2"),
                    ("fid-contacts-1", "0", "Contacts", "9"),
                ],
            )),
            2 => MockResponse::wbxml(&folder_sync_delta(
                "hier-2",
                &[("fid-contacts-2", "0", "Team Book", "14")],
                &[],
                &[],
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = contacts_adapter_at(&server);

    adapter
        .sync_mailboxes(&account(), None)
        .await
        .expect("the mail bootstrap lands");

    let result = adapter
        .sync_address_books(&account(), None)
        .await
        .expect("the contacts pass rides the shared key");
    assert_eq!(server.count(), 2, "no status-9 recovery round happened");
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("available");
    };
    assert_eq!(sync.next_cursor.as_str(), "hier-2");
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!(
            "a present-backlog round reads as a snapshot, got {:?}",
            sync.update
        );
    };
    let ids: Vec<&str> = objects.iter().map(|book| book.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["fid-contacts-1", "fid-contacts-2"],
        "the mail round's spare contacts row rides plus the delta row"
    );
}

// ---------------------------------------------------------------------------
// sync_contacts — Sync class "Contacts"
// ---------------------------------------------------------------------------

/// The card snapshot: the bootstrap enumerates everything, one malformed
/// item (no ServerId) is skipped without failing the pass, and the cards
/// convert through the neutral seam.
#[tokio::test]
async fn cards_bootstrap_from_zero_as_a_snapshot() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        assert_eq!(
            text_of(req, PAGE_AIRSYNC, AS_COLLECTION_ID),
            "fid-contacts-1",
            "the bound contact folder IS the CollectionId"
        );
        MockResponse::wbxml(&contacts_sync_response(
            "con-key-2",
            &[
                ("srv:con-1", "Zhou, Felix", "felixzhou@kylins.local"),
                ("", "Ghost Item", "ghost@example.test"),
            ],
        ))
    }) as Handler);
    let adapter = contacts_adapter_at(&server);

    let result = adapter
        .sync_contacts(&account(), None)
        .await
        .expect("the bootstrap pass succeeds");
    let ContactSourceSync::Available {
        sync,
        cursor_recovered,
    } = result
    else {
        panic!("available");
    };
    assert!(!cursor_recovered);
    assert_eq!(sync.next_cursor.as_str(), "con-key-2");
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("bootstrap must snapshot, got {:?}", sync.update);
    };
    assert_eq!(objects.len(), 1, "the ServerId-less item is skipped");
    assert_eq!(
        objects[0].display_name().as_deref(),
        Some("Zhou, Felix"),
        "FileAs is the display name"
    );
    assert_eq!(present.len(), 1);
}

/// The incremental delta: the rotated cursor key rides the request, and
/// the wire's rows map onto `changed`.
#[tokio::test]
async fn cards_incremental_delta_maps_changes() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-1", "Zhou, Felix", "felixzhou@kylins.local")],
            )),
            2 => {
                assert_eq!(
                    text_of(req, PAGE_AIRSYNC, AS_SYNC_KEY),
                    "con-key-2",
                    "the cursor key rides the delta request"
                );
                MockResponse::wbxml(&contacts_sync_response(
                    "con-key-3",
                    &[("srv:con-2", "Kerry, Anat", "anat@example.test")],
                ))
            }
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = contacts_adapter_at(&server);

    let first = adapter
        .sync_contacts(&account(), None)
        .await
        .expect("the bootstrap lands");
    let ContactSourceSync::Available {
        sync: first_sync, ..
    } = first
    else {
        panic!("available");
    };
    let second = adapter
        .sync_contacts(&account(), Some(&first_sync.next_cursor))
        .await
        .expect("the delta lands");
    let ContactSourceSync::Available {
        sync,
        cursor_recovered,
    } = second
    else {
        panic!("available");
    };
    assert!(!cursor_recovered);
    let SyncUpdate::Delta { changed, .. } = &sync.update else {
        panic!("a resumed pass must delta, got {:?}", sync.update);
    };
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].display_name().as_deref(), Some("Kerry, Anat"));
}

/// A SyncKey invalidation (collection status 3) recovers INSIDE the call:
/// the pass discards nothing-yet-delivered, restarts from "0" once as a
/// snapshot, and reports `cursor_recovered`.
#[tokio::test]
async fn a_dead_sync_key_resyncs_inside_the_call() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-1", "Zhou, Felix", "felixzhou@kylins.local")],
            )),
            // The stored key is dead: status 3.
            2 => MockResponse::wbxml(&super::fixtures::sync_response("3", "", false, &[])),
            // The recovery round: bootstrap from "0" as a snapshot.
            3 => {
                assert_eq!(
                    text_of(req, PAGE_AIRSYNC, AS_SYNC_KEY),
                    "0",
                    "the recovery restarts from the bootstrap key"
                );
                MockResponse::wbxml(&contacts_sync_response(
                    "con-key-9",
                    &[
                        ("srv:con-1", "Zhou, Felix", "felixzhou@kylins.local"),
                        ("srv:con-2", "Kerry, Anat", "anat@example.test"),
                    ],
                ))
            }
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = contacts_adapter_at(&server);

    let first = adapter
        .sync_contacts(&account(), None)
        .await
        .expect("the bootstrap lands");
    let ContactSourceSync::Available {
        sync: first_sync, ..
    } = first
    else {
        panic!("available");
    };
    let second = adapter
        .sync_contacts(&account(), Some(&first_sync.next_cursor))
        .await
        .expect("the recovery lands");
    let ContactSourceSync::Available {
        sync,
        cursor_recovered,
    } = second
    else {
        panic!("available");
    };
    assert!(
        cursor_recovered,
        "the invalidation is reported as a recovery"
    );
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("the recovery must snapshot, got {:?}", sync.update);
    };
    assert_eq!(objects.len(), 2);
    assert_eq!(sync.next_cursor.as_str(), "con-key-9");
}

// ---------------------------------------------------------------------------
// The write verbs
// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Seeds the contacts ledger with one clean `sync_contacts` pass.
pub(super) async fn seed(adapter: &EasAdapter) {
    adapter
        .sync_contacts(&account(), None)
        .await
        .expect("the seeding pass succeeds");
}

/// A contacts-shaped Sync response: one collection, key rotation, one
/// Add per `(server_id, file_as, email)` triple (a ServerId-less row
/// exercises the skip path). Built with the crate's own serializer, the
/// fixtures convention.
pub(super) fn contacts_sync_response(new_key: &str, adds: &[(&str, &str, &str)]) -> WbxmlElement {
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
    ];
    if !adds.is_empty() {
        collection.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COMMANDS,
            adds.iter()
                .map(|&(id, file_as, email)| {
                    WbxmlElement::container(
                        PAGE_AIRSYNC,
                        AS_ADD,
                        vec![
                            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, id),
                            WbxmlElement::container(
                                PAGE_AIRSYNC,
                                AS_APPLICATION_DATA,
                                vec![
                                    WbxmlElement::text(PAGE_CONTACTS, CON_FILE_AS, file_as),
                                    WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_1, email),
                                ],
                            ),
                        ],
                    )
                })
                .collect(),
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

/// A minimal draft: one named card with one e-mail, targeting the bound
/// book.
pub(super) fn minimal_draft() -> engine_core::contact::ContactDraft {
    let mut card = engine_core::contact::ContactCard::new(
        ContactId::try_from("local-new-1").unwrap(),
        engine_core::membership::Memberships::of_one(book()),
    );
    card.name = Some(engine_core::contact::ContactName {
        full: Some("Kerry, Anat".into()),
        components: vec![],
        ..engine_core::contact::ContactName::default()
    });
    card.emails.insert(
        engine_core::contact::PropertyId::new("email-1").unwrap(),
        engine_core::contact::ContactProperty::new(engine_core::contact::ContactEmail::new(
            "anat@example.test",
        )),
    );
    engine_core::contact::ContactDraft {
        address_book: book(),
        card,
    }
}

/// A stored base card as the read side would have keyed it.
pub(super) fn base_card() -> engine_core::contact::ContactCard {
    engine_core::contact::ContactCard::new(
        ContactId::try_from("srv:con-9").unwrap(),
        engine_core::membership::Memberships::of_one(book()),
    )
}

/// A resumed pass's cursor (kept for the scenario that spells it out).
#[allow(dead_code)]
pub(super) fn cursor(value: &str) -> SyncState {
    SyncState::new(value)
}
