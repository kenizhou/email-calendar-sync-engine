// SPDX-License-Identifier: MPL-2.0
// Patch-conversion tests (split from write_tests.rs with the write_patch
// module for the 500-line rule): the family-replace contract — every
// slot of a patched field's family rides, so a removed value clears on
// the wire instead of ghosting and resurrecting on the next downsync.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::contact::{
    ContactAddress, ContactEmail, ContactField, ContactName, ContactPatch, ContactPhone,
    ContactProperty, FieldPatch, PropertyId,
};

use super::*;
use crate::contacts::{build_contacts_application_data, parse_contacts_application_data};

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

fn one_email_map() -> BTreeMap<PropertyId, ContactProperty<ContactEmail>> {
    let mut map = BTreeMap::new();
    map.insert(
        id("email-1"),
        ContactProperty::new(ContactEmail::new("solo@example.test")),
    );
    map
}

/// One work phone — the replacement a phones Set carries when the server
/// holds more.
fn one_work_phone() -> BTreeMap<PropertyId, ContactProperty<ContactPhone>> {
    let mut phones = BTreeMap::new();
    let mut property = ContactProperty::new(ContactPhone {
        number: "+1-only".into(),
        ..ContactPhone::default()
    });
    property.contexts = BTreeSet::from(["work".into()]);
    phones.insert(id("phone-business"), property);
    phones
}

/// One work address carrying only a street — the partial-address shape
/// whose unset components must clear on the family path.
fn one_partial_work_address() -> BTreeMap<PropertyId, ContactProperty<ContactAddress>> {
    let mut addresses = BTreeMap::new();
    let mut property = ContactProperty::new(ContactAddress::default());
    property.contexts = BTreeSet::from(["work".into()]);
    property
        .value
        .components
        .insert("street".into(), vec!["2 Main St".into()]);
    addresses.insert(id("work"), property);
    addresses
}

/// The family-replace contract, per family: a Set emits the field's
/// whole slot family — filled slots carry their values, every other slot
/// clears as an explicit empty value — and a Clear empties the family.
/// A slot that ghosts instead would leave the server's stale value
/// standing, resurrecting it over the host's removal on the next
/// downsync.
#[test]
fn a_patch_set_replaces_the_whole_slot_family() {
    // E-mails (the original coverage, kept as the reference shape).
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Emails,
        FieldPatch::Set(serde_json::to_value(one_email_map()).unwrap()),
    );
    let written = write_from_patch(&patch).expect("converts");
    assert_eq!(written.email_1.as_deref(), Some("solo@example.test"));
    assert_eq!(
        written.email_2,
        Some(String::new()),
        "the leftover slot clears"
    );
    assert_eq!(
        written.email_3,
        Some(String::new()),
        "the leftover slot clears"
    );
    // Everything outside the patched family stays ghosted (None = omit).
    assert_eq!(written.file_as, None);

    // Phones: the replacement fills one slot; the other eleven clear.
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Phones,
        FieldPatch::Set(serde_json::to_value(one_work_phone()).unwrap()),
    );
    let written = write_from_patch(&patch).expect("converts");
    assert_eq!(written.business_phone.as_deref(), Some("+1-only"));
    for slot in [
        &written.business_2_phone,
        &written.home_phone,
        &written.home_2_phone,
        &written.mobile_phone,
        &written.assistant_phone,
        &written.car_phone,
        &written.company_main_phone,
        &written.business_fax,
        &written.home_fax,
        &written.pager,
        &written.radio_phone,
    ] {
        assert_eq!(
            slot,
            &Some(String::new()),
            "an unfilled slot clears, never ghosts — a ghosted slot leaves the server's \
             stale number standing"
        );
    }

    // Addresses: a one-set replacement clears the two dropped sets AND
    // the unset components of the surviving set.
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Addresses,
        FieldPatch::Set(serde_json::to_value(one_partial_work_address()).unwrap()),
    );
    let written = write_from_patch(&patch).expect("converts");
    let business = written
        .business_address
        .as_ref()
        .expect("the routed set rides");
    assert_eq!(business.street.as_deref(), Some("2 Main St"));
    for component in [
        &business.city,
        &business.state,
        &business.postal_code,
        &business.country,
    ] {
        assert_eq!(
            component,
            &Some(String::new()),
            "an unset component of a family set clears, never ghosts"
        );
    }
    for (set, label) in [
        (&written.home_address, "home"),
        (&written.other_address, "other"),
    ] {
        let set = set.as_ref().expect("the family forces every set to ride");
        for component in [
            &set.street,
            &set.city,
            &set.state,
            &set.postal_code,
            &set.country,
        ] {
            assert_eq!(
                component,
                &Some(String::new()),
                "a dropped {label} set clears outright, never ghosts"
            );
        }
    }

    // A Clear empties the e-mail family.
    let mut clear = ContactPatch::default();
    clear.fields.insert(ContactField::Emails, FieldPatch::Clear);
    let written = write_from_patch(&clear).expect("converts");
    assert_eq!(written.email_1, Some(String::new()));
    assert_eq!(written.email_2, Some(String::new()));
    assert_eq!(written.email_3, Some(String::new()));
}

/// The family clears reach the WIRE, not just the model: the emission of
/// a family patch parses back with every cleared slot an explicit empty
/// element — the server-visible set equals the replacement, so nothing
/// resurrects.
#[test]
fn family_clears_reach_the_wire() {
    // Phones: one value plus eleven explicit empties.
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Phones,
        FieldPatch::Set(serde_json::to_value(one_work_phone()).unwrap()),
    );
    let written = write_from_patch(&patch).expect("converts");
    let parsed_back = parse_contacts_application_data(&build_contacts_application_data(&written))
        .expect("the emission parses");
    assert_eq!(parsed_back.business_phone.as_deref(), Some("+1-only"));
    for slot in [
        &parsed_back.business_2_phone,
        &parsed_back.home_phone,
        &parsed_back.home_2_phone,
        &parsed_back.mobile_phone,
        &parsed_back.assistant_phone,
        &parsed_back.car_phone,
        &parsed_back.company_main_phone,
        &parsed_back.business_fax,
        &parsed_back.home_fax,
        &parsed_back.pager,
        &parsed_back.radio_phone,
    ] {
        assert_eq!(
            slot,
            &Some(String::new()),
            "the cleared slot rides the wire as an explicit empty element"
        );
    }

    // Addresses: a Clear puts fifteen explicit empty components on the
    // wire — nothing ghosts.
    let mut clear = ContactPatch::default();
    clear
        .fields
        .insert(ContactField::Addresses, FieldPatch::Clear);
    let written = write_from_patch(&clear).expect("converts");
    let parsed_back = parse_contacts_application_data(&build_contacts_application_data(&written))
        .expect("the emission parses");
    for (set, label) in [
        (&parsed_back.business_address, "business"),
        (&parsed_back.home_address, "home"),
        (&parsed_back.other_address, "other"),
    ] {
        let set = set.as_ref().expect("the {label} set rides the wire");
        for component in [
            &set.street,
            &set.city,
            &set.state,
            &set.postal_code,
            &set.country,
        ] {
            assert_eq!(
                component,
                &Some(String::new()),
                "the {label} clear is an explicit empty element, never a ghost"
            );
        }
    }

    // A dropped set under a one-set replacement clears the same way.
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Addresses,
        FieldPatch::Set(serde_json::to_value(one_partial_work_address()).unwrap()),
    );
    let written = write_from_patch(&patch).expect("converts");
    let parsed_back = parse_contacts_application_data(&build_contacts_application_data(&written))
        .expect("the emission parses");
    assert_eq!(
        parsed_back
            .business_address
            .as_ref()
            .unwrap()
            .street
            .as_deref(),
        Some("2 Main St")
    );
    assert_eq!(
        parsed_back
            .business_address
            .as_ref()
            .unwrap()
            .city
            .as_deref(),
        Some(""),
        "the unset component clears on the wire"
    );
    let dropped = parsed_back
        .home_address
        .as_ref()
        .expect("the dropped set rides");
    assert_eq!(dropped.street.as_deref(), Some(""));
    assert_eq!(dropped.country.as_deref(), Some(""));
}

/// A name Set emits the filing name plus every part; a name Clear empties
/// them all.
#[test]
fn a_name_patch_emits_file_as_and_every_part() {
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(
            serde_json::to_value(ContactName {
                full: None,
                components: vec![
                    engine_core::contact::NameComponent::new(
                        engine_core::contact::NameComponentKind::Given,
                        "Anat",
                    ),
                    engine_core::contact::NameComponent::new(
                        engine_core::contact::NameComponentKind::Surname,
                        "Kerry",
                    ),
                ],
                ..ContactName::default()
            })
            .unwrap(),
        ),
    );
    let written = write_from_patch(&patch).expect("converts");
    assert_eq!(written.file_as.as_deref(), Some("Anat Kerry"));
    assert_eq!(written.first_name.as_deref(), Some("Anat"));
    assert_eq!(written.last_name.as_deref(), Some("Kerry"));
    assert_eq!(
        written.middle_name,
        Some(String::new()),
        "unset parts clear"
    );

    let mut clear = ContactPatch::default();
    clear.fields.insert(ContactField::Name, FieldPatch::Clear);
    let written = write_from_patch(&clear).expect("converts");
    for slot in [
        &written.file_as,
        &written.first_name,
        &written.middle_name,
        &written.last_name,
        &written.name_prefix,
        &written.name_suffix,
    ] {
        assert_eq!(slot, &Some(String::new()));
    }
}

/// Kind: setting Individual is the no-op it already is; anything else —
/// or a clear — refuses, exactly like Graph's individual-only ruling.
#[test]
fn only_individual_kind_patches_are_representable() {
    let patch = ContactPatch {
        kind: Some(FieldPatch::Set(
            engine_core::contact::ContactKind::Individual,
        )),
        ..ContactPatch::default()
    };
    assert!(write_from_patch(&patch).is_ok());

    let patch = ContactPatch {
        kind: Some(FieldPatch::Set(
            engine_core::contact::ContactKind::Organization,
        )),
        ..ContactPatch::default()
    };
    assert!(write_from_patch(&patch).is_err());

    let patch = ContactPatch {
        kind: Some(FieldPatch::Clear),
        ..ContactPatch::default()
    };
    assert!(write_from_patch(&patch).is_err());
}

/// A patch field with no EAS slot refuses (the Graph patch-precedent) —
/// a targeted Set the transport would silently drop is a contract
/// violation, not a degrade.
#[test]
fn unrepresentable_patch_fields_refuse() {
    for field in [
        ContactField::Nicknames,
        ContactField::Relations,
        ContactField::Languages,
        ContactField::Keywords,
        ContactField::TimeZone,
        ContactField::PersonalInfo,
        ContactField::OnlineServices,
    ] {
        let mut patch = ContactPatch::default();
        patch.fields.insert(field, FieldPatch::Clear);
        let err = write_from_patch(&patch).expect_err("no EAS slot");
        assert!(
            matches!(err.class(), engine_core::error::FailureClass::Permanent),
            "the refusal is permanent: {err:?}"
        );
        assert!(
            err.detail().contains("no slot"),
            "the refusal names the missing slot: {}",
            err.detail()
        );
    }
}
