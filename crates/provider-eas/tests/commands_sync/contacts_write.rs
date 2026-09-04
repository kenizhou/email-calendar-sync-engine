// SPDX-License-Identifier: MPL-2.0
//! Contacts upsync: build_contacts_change_request golden structure (P2
//! Task 5) — the twin of `calendar_write.rs`'s goldens for the Contacts
//! class.

use super::*;
use provider_eas::{
    commands::{AS_CLASS, AS_GET_CHANGES, ContactsChange},
    contacts::{CON_EMAIL_1, CON_FILE_AS, PAGE_CONTACTS},
};

/// A minimal-but-populated contacts payload for the goldens — one
/// DISTINCT value per element so a crossed wire cannot hide behind
/// equal props.
fn contact_props() -> provider_eas::contacts::ContactsContactProps {
    provider_eas::contacts::ContactsContactProps {
        file_as: Some("Kerry, Anat".to_owned()),
        first_name: Some("Anat".to_owned()),
        last_name: Some("Kerry".to_owned()),
        email_1: Some("anat@example.test".to_owned()),
        ..Default::default()
    }
}

/// Walk to the single Collection's `Commands` element and assert the
/// shared envelope: Sync > Collections > Collection with EXACTLY
/// [SyncKey, CollectionId, Commands] as direct children — in particular
/// NO `airsync:Class` (14.0+ rejects it; CollectionId identifies the
/// collection) and NO `GetChanges` (invalid in 16.1), the same gates as
/// the email and calendar builders. Returns the Commands element.
fn assert_contacts_envelope<'a>(
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
    assert_eq!(collection.children.len(), 3);
    let key = &collection.children[0];
    assert_eq!((key.page, key.token), (PAGE_AIRSYNC, AS_SYNC_KEY));
    assert_eq!(text_value(key).unwrap(), sync_key);
    let cid = &collection.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).unwrap(), collection_id);
    let commands = &collection.children[2];
    assert_eq!((commands.page, commands.token), (PAGE_AIRSYNC, AS_COMMANDS));
    assert!(
        collection.children.iter().all(
            |c| !(c.page == PAGE_AIRSYNC && (c.token == AS_CLASS || c.token == AS_GET_CHANGES))
        ),
        "contacts upsync Collection must not carry Class or GetChanges"
    );
    commands
}

/// Golden (Add): `ContactsChange::Add` emits the wire `airsync:Add`
/// container { ClientId, ApplicationData }, the ApplicationData being
/// the contacts serializer's output unmodified.
#[test]
fn contacts_change_add_matches_golden_structure() {
    let props = contact_props();
    let client_id = new_contacts_client_id();
    let tree = build_contacts_change_request(
        "con5",
        "{sk9}",
        &[ContactsChange::Add {
            client_id: client_id.clone(),
            props: props.clone(),
        }],
    );

    let commands = assert_contacts_envelope(&tree, "{sk9}", "con5");
    assert_eq!(commands.children.len(), 1);
    let add = &commands.children[0];
    assert_eq!((add.page, add.token), (PAGE_AIRSYNC, AS_ADD));
    assert_eq!(add.children.len(), 2, "ClientId + ApplicationData only");

    let cid_el = &add.children[0];
    assert_eq!((cid_el.page, cid_el.token), (PAGE_AIRSYNC, AS_CLIENT_ID));
    assert_eq!(text_value(cid_el).unwrap(), client_id);

    let app_data = &add.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(
        *app_data,
        build_contacts_application_data(&props),
        "ApplicationData must be the contacts serializer's output, unmodified"
    );
}

/// Golden (Replace → wire Change): ServerId + ApplicationData; the
/// ghost contract — a `None` slot is omitted, a `Some("")` slot emits
/// the empty-value clear element.
#[test]
fn contacts_change_replace_carries_the_ghost_clear() {
    let clearing = provider_eas::contacts::ContactsContactProps {
        email_1: Some(String::new()),
        ..Default::default()
    };
    let tree = build_contacts_change_request(
        "con5",
        "k2",
        &[ContactsChange::Replace {
            server_id: "srv:con-9".to_owned(),
            props: clearing,
        }],
    );
    let commands = assert_contacts_envelope(&tree, "k2", "con5");
    let change = &commands.children[0];
    assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
    assert_eq!(change.children.len(), 2, "ServerId + ApplicationData");
    assert_eq!(
        (change.children[0].page, change.children[0].token),
        (PAGE_AIRSYNC, AS_SERVER_ID)
    );
    assert_eq!(text_value(&change.children[0]).unwrap(), "srv:con-9");

    let app_data = &change.children[1];
    assert_eq!(
        app_data.children.len(),
        1,
        "only the clearing element rides"
    );
    let email = &app_data.children[0];
    assert_eq!((email.page, email.token), (PAGE_CONTACTS, CON_EMAIL_1));
    assert_eq!(
        text_value(email).unwrap(),
        "",
        "the clear is an empty-VALUE element — present, not omitted"
    );
}

/// Golden (Remove → wire Delete): a container whose only child is the
/// ServerId — no ApplicationData on a delete.
#[test]
fn contacts_change_remove_is_a_bare_delete() {
    let tree = build_contacts_change_request(
        "con5",
        "k3",
        &[ContactsChange::Remove {
            server_id: "srv:con-9".to_owned(),
        }],
    );
    let commands = assert_contacts_envelope(&tree, "k3", "con5");
    let delete = &commands.children[0];
    assert_eq!((delete.page, delete.token), (PAGE_AIRSYNC, AS_DELETE));
    assert_eq!(delete.children.len(), 1);
    assert_eq!(
        (delete.children[0].page, delete.children[0].token),
        (PAGE_AIRSYNC, AS_SERVER_ID)
    );
    assert_eq!(text_value(&delete.children[0]).unwrap(), "srv:con-9");
}

/// The client id stays under the [MS-ASCMD] 40-char cap, carries the
/// contacts prefix, and is unique per call.
#[test]
fn new_contacts_client_id_fits_cap_carries_prefix_and_is_unique() {
    let a = new_contacts_client_id();
    let b = new_contacts_client_id();
    assert!(a.starts_with("ConAdd-"));
    assert!(a.len() <= 40);
    assert_ne!(a, b);
}

/// The ApplicationData carries the page-1 contacts tokens (spot-check
/// FileAs) — the serializer, not a calendar/email page mix.
#[test]
fn contacts_application_data_uses_the_contacts_page() {
    let tree = build_contacts_change_request(
        "con5",
        "k4",
        &[ContactsChange::Add {
            client_id: new_contacts_client_id(),
            props: contact_props(),
        }],
    );
    let texts = texts_of(&tree, PAGE_CONTACTS, CON_FILE_AS);
    assert_eq!(texts, vec!["Kerry, Anat".to_owned()]);
}

/// Depth-first text values of `(page, token)` elements in a tree.
fn texts_of(tree: &WbxmlElement, page: u8, token: u8) -> Vec<String> {
    fn walk(el: &WbxmlElement, page: u8, token: u8, out: &mut Vec<String>) {
        if el.page == page
            && el.token == token
            && let provider_eas::wbxml::WbxmlValue::Text(text) = &el.value
        {
            out.push(text.clone());
        }
        for child in &el.children {
            walk(child, page, token, out);
        }
    }
    let mut out = Vec::new();
    walk(tree, page, token, &mut out);
    out
}
