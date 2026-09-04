// SPDX-License-Identifier: MPL-2.0
// Downsync conversion tests: ContactsContactProps → the engine's neutral
// ContactCard (P2 Task 5). The wire-shape goldens live in
// `contacts_testutil`; these pin the NEUTRAL mapping, field by field.

use std::collections::BTreeSet;

use engine_core::{
    contact::{ContactKind, ContactSourceClass, NameComponentKind, PropertyId},
    ids::AddressBookId,
    membership::Memberships,
};

use super::*;

fn book() -> AddressBookId {
    AddressBookId::try_from("fid-contacts-1").unwrap()
}

/// The full-fixture card: every representable field lands on the neutral
/// card with the documented ids, contexts, and features.
#[test]
fn the_full_fixture_maps_every_representable_field() {
    let props = crate::contacts_testutil::expected_full_contact_props();
    let card = contact_card_from_props(&book(), "srv:con-1", &props);

    // Identity and membership.
    assert_eq!(card.id.as_str(), "srv:con-1");
    assert_eq!(card.uid, None, "EAS contacts carry no UID element");
    assert_eq!(
        card.address_books,
        Memberships::of_one(book()),
        "the item's folder IS its address-book membership"
    );
    assert_eq!(card.source_class, ContactSourceClass::Personal);
    assert!(card.is_writable, "the account owner's folders are writable");
    assert_eq!(card.kind, ContactKind::Individual);

    // Name: FileAs is the provider-formatted full name; parts map in order.
    let name = card.name.as_ref().expect("the fixture names the contact");
    assert_eq!(name.full.as_deref(), Some("Zhou, Felix"));
    let parts: Vec<(&str, &str)> = name
        .components
        .iter()
        .map(|component| {
            let kind = match &component.kind {
                NameComponentKind::Prefix => "prefix",
                NameComponentKind::Given => "given",
                NameComponentKind::Middle => "middle",
                NameComponentKind::Surname => "surname",
                NameComponentKind::Suffix => "suffix",
                other => panic!("unexpected component kind {other:?}"),
            };
            (kind, component.value.as_str())
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            ("prefix", "Mr."),
            ("given", "Felix"),
            ("middle", "Ming"),
            ("surname", "Zhou"),
            ("suffix", "Jr."),
        ]
    );
    assert_eq!(
        card.display_name().as_deref(),
        Some("Zhou, Felix"),
        "display falls out of FileAs"
    );

    // Emails: the three wire slots become three stable properties.
    let emails: Vec<&str> = card
        .emails
        .values()
        .map(|e| e.value.address.as_str())
        .collect();
    assert_eq!(
        emails,
        vec![
            "felixzhou@kylins.local",
            "felix.zhou@example.com",
            "felix@home.example"
        ]
    );
    assert!(
        card.emails
            .contains_key(&PropertyId::new("email-1").unwrap()),
        "stable slot ids survive for the write path"
    );

    // Phones: every slot lands with its documented context/feature tag.
    let number = |id: &str| {
        let phone = card
            .phones
            .get(&PropertyId::new(id).unwrap())
            .unwrap_or_else(|| panic!("phone {id} missing"));
        phone.value.number.as_str()
    };
    assert_eq!(number("phone-business"), "(206) 555-0103");
    assert_eq!(number("phone-business-2"), "(206) 555-0104");
    assert_eq!(number("phone-home"), "(206) 555-0101");
    assert_eq!(number("phone-home-2"), "(206) 555-0107");
    assert_eq!(number("phone-mobile"), "(206) 555-0102");
    assert_eq!(number("phone-assistant"), "(206) 555-0110");
    assert_eq!(number("phone-car"), "(206) 555-0106");
    assert_eq!(number("phone-company-main"), "(206) 555-0100");
    assert_eq!(number("phone-business-fax"), "(206) 555-0105");
    assert_eq!(number("phone-home-fax"), "(206) 555-0108");
    assert_eq!(number("phone-pager"), "(206) 555-0109");
    assert_eq!(number("phone-radio"), "(206) 555-0111");
    assert_eq!(card.phones.len(), 12);
    let business = card
        .phones
        .get(&PropertyId::new("phone-business").unwrap())
        .unwrap();
    assert_eq!(business.contexts, BTreeSet::from(["work".to_owned()]));
    let mobile = card
        .phones
        .get(&PropertyId::new("phone-mobile").unwrap())
        .unwrap();
    assert!(mobile.value.features.contains("mobile"));
    assert!(
        mobile.contexts.is_empty(),
        "the mobile slot is context-free on the wire"
    );

    // Addresses: the three sets with their component keys.
    let address = |id: &str| card.addresses.get(&PropertyId::new(id).unwrap()).unwrap();
    let work = address("business");
    assert!(work.contexts.contains("work"));
    assert_eq!(
        work.value.components.get("street").unwrap(),
        &vec!["1 Microsoft Way".to_owned()]
    );
    assert_eq!(
        work.value.components.get("locality").unwrap(),
        &vec!["Redmond".to_owned()]
    );
    assert_eq!(
        work.value.components.get("region").unwrap(),
        &vec!["WA".to_owned()]
    );
    assert_eq!(
        work.value.components.get("postcode").unwrap(),
        &vec!["98052".to_owned()]
    );
    assert_eq!(
        work.value.components.get("country").unwrap(),
        &vec!["United States".to_owned()]
    );
    assert!(address("home").contexts.contains("private"));
    assert!(address("other").contexts.contains("other"));

    // Organization, title, notes, url.
    let organization = card
        .organizations
        .get(&PropertyId::new("organization").unwrap())
        .unwrap();
    assert_eq!(organization.value.name, "Kylins");
    let title = card
        .titles
        .get(&PropertyId::new("job-title").unwrap())
        .unwrap();
    assert_eq!(title.value.name, "Development Manager");
    let note = card.notes.get(&PropertyId::new("notes").unwrap()).unwrap();
    assert_eq!(note.value.note, "Prefers plain-text bodies.");
    let url = card
        .urls
        .get(&PropertyId::new("web-page").unwrap())
        .unwrap();
    assert_eq!(url.value.uri, "https://example.com/fzhou");

    // Anniversaries: the wire time part is dropped (the Graph birthday
    // rule — a full timestamp must not leak into a date field).
    let anniversary = card
        .anniversaries
        .get(&PropertyId::new("anniversary").unwrap())
        .unwrap();
    assert_eq!(anniversary.value.date, "1998-05-23");
    assert_eq!(anniversary.value.kind.as_deref(), Some("wedding"));
    let birthday = card
        .anniversaries
        .get(&PropertyId::new("birthday").unwrap())
        .unwrap();
    assert_eq!(birthday.value.date, "1975-11-30");
    assert_eq!(birthday.value.kind.as_deref(), Some("birth"));

    // The assistant/manager names: personal-info entries (the neutral
    // model's only text-bearing slot for org-chart names).
    let personal = |kind: &str| {
        let entry = card
            .personal_info
            .values()
            .find(|entry| entry.value.kind == kind)
            .unwrap_or_else(|| panic!("personal info {kind} missing"));
        entry.value.value.as_str()
    };
    assert_eq!(personal("assistant"), "Ada Lovelace");
    assert_eq!(
        personal("manager"),
        "CN=Bob Stone,OU=Engineering,DC=kylins,DC=local"
    );

    // Picture presence survives as an extended fact — never a media entry
    // (the bytes are dropped at parse and no URI exists to fetch).
    assert!(card.media.is_empty(), "no photo resource is invented");
    assert_eq!(
        card.extended.get("eas/picture-present"),
        Some(&serde_json::json!(true))
    );
}

/// A minimal FileAs-only item degrades to a bare named card — no phantom
/// emails, phones, or components.
#[test]
fn a_file_as_only_item_maps_to_a_bare_named_card() {
    let props = ContactsContactProps {
        file_as: Some("Kerry, Anat".to_owned()),
        ..ContactsContactProps::default()
    };
    let card = contact_card_from_props(&book(), "srv:con-2", &props);
    assert_eq!(card.display_name().as_deref(), Some("Kerry, Anat"));
    assert_eq!(card.name.as_ref().unwrap().components, vec![]);
    assert!(card.emails.is_empty());
    assert!(card.phones.is_empty());
    assert!(card.addresses.is_empty());
    assert!(card.organizations.is_empty());
    assert!(card.titles.is_empty());
    assert!(card.notes.is_empty());
    assert!(card.anniversaries.is_empty());
    assert!(card.urls.is_empty());
    assert!(card.personal_info.is_empty());
    assert!(card.extended.get("eas/picture-present").is_none());
}

/// Without FileAs the name still exists when parts do — `display()`
/// composes from the components.
#[test]
fn parts_alone_still_form_a_name() {
    let props = ContactsContactProps {
        first_name: Some("Anat".to_owned()),
        last_name: Some("Kerry".to_owned()),
        ..ContactsContactProps::default()
    };
    let card = contact_card_from_props(&book(), "srv:con-3", &props);
    assert_eq!(card.display_name().as_deref(), Some("Anat Kerry"));
}

/// Empty-string values (the cleared shape a ghosted clear produces) read
/// as absent everywhere — an empty phone is no phone.
#[test]
fn empty_wire_values_read_as_absent() {
    let props = ContactsContactProps {
        file_as: Some(String::new()),
        first_name: Some("   ".to_owned()),
        email_1: Some(String::new()),
        mobile_phone: Some(String::new()),
        body_plain: Some(String::new()),
        company: Some(String::new()),
        business_address: Some(ContactsAddress {
            street: Some(String::new()),
            city: Some(String::new()),
            state: Some(String::new()),
            postal_code: Some(String::new()),
            country: Some(String::new()),
        }),
        ..ContactsContactProps::default()
    };
    let card = contact_card_from_props(&book(), "srv:con-4", &props);
    assert_eq!(card.name, None, "blank FileAs and parts are no name");
    assert!(card.emails.is_empty());
    assert!(card.phones.is_empty());
    assert!(card.notes.is_empty());
    assert!(card.organizations.is_empty());
    assert!(
        card.addresses.is_empty(),
        "an all-empty address set is no address"
    );
}

/// An unkeyable ServerId keeps the item under the placeholder key the
/// store reconciles away — never a panic, never a dropped item.
#[test]
fn an_unkeyable_server_id_degrades_to_the_placeholder() {
    let card = contact_card_from_props(&book(), "", &ContactsContactProps::default());
    assert_eq!(card.id.as_str(), "eas:unkeyed");
}
