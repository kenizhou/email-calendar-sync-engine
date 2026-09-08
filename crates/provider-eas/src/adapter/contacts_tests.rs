// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the contacts slice (`contacts.rs`) — the `#[path]`
//! split the repo uses to hold the 500-line cap. The wire-level
//! scenarios live in `tests/transport_harness/adapter_contacts_flow.rs`.

use engine_core::contact::ContactField;

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

/// Only the Contacts class (folder Type 9/14) lands in the contacts
/// container scope: mail folders, calendars, tasks, and the classless
/// shape are all excluded.
#[test]
fn only_contacts_class_folders_map_to_address_books() {
    let folders = vec![
        wire_folder("fid-contacts-1", "Contacts", Some(9)),
        wire_folder("fid-contacts-2", "Contacts", Some(14)),
        wire_folder("fid-inbox", "Email", Some(2)),
        wire_folder("fid-cal-1", "Calendar", Some(8)),
        wire_folder("fid-typeless", "", None),
    ];
    let mapped = address_books(&folders);
    let ids: Vec<&str> = mapped.iter().map(|book| book.id.as_str()).collect();
    assert_eq!(ids, vec!["fid-contacts-1", "fid-contacts-2"]);
}

/// A contact folder maps with its ServerId as the stable id, the display
/// name verbatim, writability (EAS exposes no privileges to deny), the
/// default-folder fact (Type 9 is the default contacts folder, 14 the
/// user-created one), and the EAS-native class/type facts under the
/// adapter's extended namespace.
#[test]
fn a_contact_folder_maps_with_native_facts() {
    let default_folder = wire_folder("fid-contacts-1", "Contacts", Some(9));
    let book = &address_books(&[default_folder])[0];
    assert_eq!(book.id.as_str(), "fid-contacts-1");
    assert_eq!(book.name, "Name of fid-contacts-1");
    assert!(book.is_writable);
    assert!(book.is_default, "folder Type 9 is the default book");
    assert_eq!(
        book.extended.get("eas/class"),
        Some(&serde_json::json!("Contacts"))
    );
    assert_eq!(
        book.extended.get("eas/folder-type"),
        Some(&serde_json::json!(9u8))
    );

    let user_folder = wire_folder("fid-contacts-2", "Contacts", Some(14));
    let book = &address_books(&[user_folder])[0];
    assert!(!book.is_default, "folder Type 14 is a user-created book");
}

/// A folder whose ServerId cannot key an AddressBookId (empty) is skipped
/// with a warning, never failing the round.
#[test]
fn an_unkeyable_contact_folder_is_skipped() {
    let folders = vec![
        wire_folder("", "Contacts", Some(9)),
        wire_folder("fid-contacts-2", "Contacts", Some(9)),
    ];
    let mapped = address_books(&folders);
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].id.as_str(), "fid-contacts-2");
}

/// An adapter built without a contacts binding refuses card sync with
/// `InvalidState` — and its capabilities never advertise the contacts
/// family, so a capability-checking caller never reaches the refusal.
#[test]
fn an_unbound_adapter_refuses_card_sync_as_invalid_state() {
    assert_eq!(
        unbound_contacts().class(),
        engine_core::error::FailureClass::InvalidState
    );
}

/// The destination advertises exactly the fields the write seam
/// represents — Kind is absent by design (the class is individual-only
/// with no kind element).
#[test]
fn supported_fields_name_exactly_the_representable_set() {
    let fields = supported_fields();
    for field in [
        ContactField::Name,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Addresses,
        ContactField::Organizations,
        ContactField::Titles,
        ContactField::Notes,
        ContactField::Urls,
        ContactField::Anniversaries,
    ] {
        assert!(fields.contains(field), "{field:?} must be supported");
    }
    assert!(!fields.contains(ContactField::Kind), "EAS has no kind slot");
    assert!(!fields.contains(ContactField::Nicknames));
    assert!(!fields.contains(ContactField::Keywords));
}
