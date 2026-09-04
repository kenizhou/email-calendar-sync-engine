// SPDX-License-Identifier: MPL-2.0
//! Adapter contacts WRITE scenarios (P2 Task 5): the Add ack's
//! ServerId backfill, the ghost-model Change, the already-gone Delete,
//! the empty-patch no-op, the cold-ledger and unbound refusals, and the
//! destination — against the offline mock server. The read scenarios
//! (discovery + card sync) and the shared fixtures live in
//! `adapter_contacts_flow.rs`; the conversion goldens in `src/contacts/`;
//! the upsync request shapes in `tests/commands_sync/contacts_write.rs`.

use std::sync::Arc;

use engine_core::contact::{ContactField, ContactPatch, FieldPatch};
use engine_core::ids::{AddressBookId, ContactId, MailboxId};
use engine_provider::{ContactsProvider as _, Provider as _};
use provider_eas::adapter::EasAdapter;
use provider_eas::commands::{
    AS_CHANGE, AS_CLIENT_ID, AS_COLLECTION_ID, AS_DELETE, AS_SERVER_ID, AS_SYNC_KEY, PAGE_AIRSYNC,
};
use provider_eas::contacts::{CON_EMAIL_1, CON_EMAIL_2, CON_FILE_AS, PAGE_CONTACTS};

use super::{
    adapter_calendar_flow::account,
    adapter_calendar_write_flow::{
        add_ack, count_of, item_status, text_of, texts, upsync_response,
    },
    adapter_contacts_flow::{
        base_card, book, contacts_adapter_at, contacts_sync_response, minimal_draft, seed,
    },
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// A cold ledger (no pass observed yet) refuses `NeedsResync` rather than
/// guessing a key — and NO request goes out.
#[tokio::test]
async fn a_cold_ledger_refuses_the_create_without_a_round_trip() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = contacts_adapter_at(&server);
    let err = adapter
        .create_contact(&account(), &minimal_draft())
        .await
        .expect_err("cold ledger");
    assert_eq!(err.class(), engine_core::error::FailureClass::NeedsResync);
    assert_eq!(server.count(), 0, "no request went out");
}

/// The create happy path: Sync `Add` riding the ledger's key, the
/// server's ack assigns the ServerId, and the receipt keys it — the only
/// id-reveal point.
#[tokio::test]
async fn create_adds_and_resolves_the_server_id_through_the_ack() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-0", "Seed Item", "seed@example.test")],
            )),
            2 => {
                let client_id = text_of(req, PAGE_AIRSYNC, AS_CLIENT_ID);
                assert!(
                    client_id.starts_with("ConAdd-") && client_id.len() <= 40,
                    "the ClientId is synthesized under the 40-char cap: {client_id}"
                );
                MockResponse::wbxml(&upsync_response(
                    "con-key-3",
                    vec![add_ack(&client_id, "srv:con-new-1", "1")],
                ))
            }
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let adapter = contacts_adapter_at(&server);
    seed(&adapter).await;

    let receipt = adapter
        .create_contact(&account(), &minimal_draft())
        .await
        .expect("the Add lands");
    assert_eq!(receipt.contact.as_str(), "srv:con-new-1");

    let add_request = server.request(2);
    assert_eq!(
        text_of(&add_request, PAGE_AIRSYNC, AS_SYNC_KEY),
        "con-key-2",
        "the Add rides the ledger's key"
    );
    assert_eq!(
        text_of(&add_request, PAGE_AIRSYNC, AS_COLLECTION_ID),
        "fid-contacts-1"
    );
    assert_eq!(
        texts(&add_request, PAGE_CONTACTS, CON_FILE_AS),
        vec!["Kerry, Anat".to_owned()],
        "the draft's filing name rides the ApplicationData"
    );
}

/// A draft targeting a different address book refuses `InvalidState` —
/// the bound folder is the only destination this adapter can name.
#[tokio::test]
async fn a_draft_for_another_book_refuses() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = contacts_adapter_at(&server);
    let mut draft = minimal_draft();
    draft.address_book = AddressBookId::try_from("fid-contacts-other").unwrap();
    let err = adapter
        .create_contact(&account(), &draft)
        .await
        .expect_err("foreign book");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
}

/// An empty patch is a no-op receipt — no wire round (the outbox driver
/// does not pre-filter emptiness, so the adapter honors it).
#[tokio::test]
async fn an_empty_patch_sends_nothing() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-0", "Seed Item", "seed@example.test")],
            )),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = contacts_adapter_at(&server);
    seed(&adapter).await;
    let count_before = server.count();

    let receipt = adapter
        .patch_contact(&account(), &base_card(), &ContactPatch::default())
        .await
        .expect("the no-op receipt");
    assert_eq!(receipt.contact.as_str(), "srv:con-9");
    assert_eq!(server.count(), count_before, "no wire round for a no-op");
}

/// A patch rides a Sync `Change` naming the card's ServerId, and the
/// Set-replaces-family ghosting shows on the wire: the set slot carries
/// its value, the leftover slots carry EMPTY values.
#[tokio::test]
async fn a_patch_rides_a_change_with_the_ghost_clear() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-0", "Seed Item", "seed@example.test")],
            )),
            2 => {
                assert_eq!(
                    count_of(req, PAGE_AIRSYNC, AS_CHANGE),
                    1,
                    "exactly one Change"
                );
                assert_eq!(
                    text_of(req, PAGE_AIRSYNC, AS_SERVER_ID),
                    "srv:con-9",
                    "the Change names the card"
                );
                MockResponse::wbxml(&upsync_response("con-key-3", vec![]))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = contacts_adapter_at(&server);
    seed(&adapter).await;

    let mut patch = ContactPatch::default();
    let mut emails = std::collections::BTreeMap::new();
    emails.insert(
        engine_core::contact::PropertyId::new("email-1").unwrap(),
        engine_core::contact::ContactProperty::new(engine_core::contact::ContactEmail::new(
            "solo@example.test",
        )),
    );
    patch.fields.insert(
        ContactField::Emails,
        FieldPatch::Set(serde_json::to_value(&emails).unwrap()),
    );
    let receipt = adapter
        .patch_contact(&account(), &base_card(), &patch)
        .await
        .expect("the Change lands");
    assert_eq!(receipt.contact.as_str(), "srv:con-9");

    let change_request = server.request(2);
    assert_eq!(
        texts(&change_request, PAGE_CONTACTS, CON_EMAIL_1),
        vec!["solo@example.test".to_owned()]
    );
    assert_eq!(
        texts(&change_request, PAGE_CONTACTS, CON_EMAIL_2),
        vec![String::new()],
        "the leftover slot clears as an empty-value element"
    );
}

/// The delete: a Sync `Delete` of the ServerId, with the already-gone
/// item status 8 resolving as the idempotent success.
#[tokio::test]
async fn delete_removes_and_already_gone_is_success() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&contacts_sync_response(
                "con-key-2",
                &[("srv:con-0", "Seed Item", "seed@example.test")],
            )),
            2 => {
                assert_eq!(
                    count_of(req, PAGE_AIRSYNC, AS_DELETE),
                    1,
                    "exactly one Delete"
                );
                MockResponse::wbxml(&upsync_response(
                    "con-key-3",
                    vec![item_status("srv:con-9", "8", false)],
                ))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = contacts_adapter_at(&server);
    seed(&adapter).await;

    adapter
        .delete_contact(&account(), &base_card())
        .await
        .expect("already-gone (status 8) is the idempotent success");
    assert_eq!(
        text_of(&server.request(2), PAGE_AIRSYNC, AS_SERVER_ID),
        "srv:con-9"
    );
}

/// The unbound refusals: an adapter without the contacts binding refuses
/// card sync and every write verb `InvalidState` — and its capabilities
/// never advertise the family, so a capability-checking caller never
/// reaches the refusal.
#[tokio::test]
async fn an_unbound_adapter_refuses_every_contacts_verb() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = EasAdapter::new(
        client_at(&server.eas_url()),
        MailboxId::try_from("fid-inbox").unwrap(),
    );
    let err = adapter
        .sync_contacts(&account(), None)
        .await
        .expect_err("unbound");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert!(
        adapter
            .create_contact(&account(), &minimal_draft())
            .await
            .is_err()
    );
    assert!(
        adapter
            .patch_contact(&account(), &base_card(), &ContactPatch::default())
            .await
            .is_err()
    );
    assert!(
        adapter
            .delete_contact(&account(), &base_card())
            .await
            .is_err()
    );
    assert_eq!(server.count(), 0, "no request went out");

    // The unsupported verbs keep their honest defaults: no canonical
    // fetch, no photo fetch.
    assert!(
        adapter
            .fetch_contact(&account(), &ContactId::try_from("srv:con-1").unwrap())
            .await
            .is_err()
    );
    assert!(!adapter.connection_info().capabilities.contact_photos());
}

/// The bound adapter keeps the same honest defaults for the verbs that
/// did not land: `fetch_contact` and `fetch_contact_photo` reject (the
/// photo bit stays off — the documented stance).
#[tokio::test]
async fn the_bound_adapter_keeps_the_unimplemented_verbs_refusing() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = contacts_adapter_at(&server);
    let card = base_card();
    assert!(adapter.fetch_contact(&account(), &card.id).await.is_err());
    assert!(
        adapter
            .fetch_contact_photo(
                &account(),
                &card,
                &engine_core::contact::ContactResource::default()
            )
            .await
            .is_err()
    );
    assert!(!adapter.connection_info().capabilities.contact_photos());
}

/// The destination names the bound book with the honest absent guard and
/// exactly the representable fields.
#[tokio::test]
async fn the_destination_names_the_bound_book() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = contacts_adapter_at(&server);
    let destination = adapter.contact_destination().expect("the binding is live");
    assert_eq!(destination.address_book, book());
    assert!(destination.writable);
    assert_eq!(
        destination.write_guard,
        Some(engine_provider::WriteGuard::Absent)
    );
    assert!(!destination.supported_fields.contains(ContactField::Kind));
    assert!(
        destination
            .supported_fields
            .contains(ContactField::Anniversaries)
    );
}

// ---------------------------------------------------------------------------
