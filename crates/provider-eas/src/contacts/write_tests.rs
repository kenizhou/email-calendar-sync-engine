// SPDX-License-Identifier: MPL-2.0
// Upsync conversion + emission tests: the neutral ContactCard/ContactPatch
// → the ghost-model ContactsContactProps → the ApplicationData element
// (P2 Task 5). The envelope goldens (Add/Change/Delete around the
// ApplicationData) live in tests/commands_sync/contacts_write.rs.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::contact::{
    ContactCard, ContactEmail, ContactField, ContactName, ContactNickname, ContactPatch,
    ContactPhone,
    ContactProperty, ContactRelation, ContactResource, FieldPatch, PropertyId, Title,
};
use engine_provider::ProviderError;

use super::*;
use crate::{
    contacts::contact_card_from_props,
    contacts::write_patch::write_from_patch, contacts_testutil,
};

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

/// A comprehensive card shaped like the full wire fixture — every
/// representable field, distinct values, the downsync's own stable ids.
fn comprehensive_card() -> ContactCard {
    let props = contacts_testutil::expected_full_contact_props();
    contact_card_from_props(
        &engine_core::ids::AddressBookId::try_from("fid-contacts-1").unwrap(),
        "srv:con-1",
        &props,
    )
}

/// The full card converts back to the wire model with every
/// representable slot intact — the representable-subset round-trip.
#[test]
fn the_full_card_round_trips_to_its_wire_slots() {
    let card = comprehensive_card();
    let written = write_from_draft(&card).expect("the card converts");

    assert_eq!(written.file_as.as_deref(), Some("Zhou, Felix"));
    assert_eq!(written.first_name.as_deref(), Some("Felix"));
    assert_eq!(written.middle_name.as_deref(), Some("Ming"));
    assert_eq!(written.last_name.as_deref(), Some("Zhou"));
    assert_eq!(written.name_suffix.as_deref(), Some("Jr."));
    assert_eq!(written.name_prefix.as_deref(), Some("Mr."));
    assert_eq!(written.email_1.as_deref(), Some("felixzhou@kylins.local"));
    assert_eq!(written.email_2.as_deref(), Some("felix.zhou@example.com"));
    assert_eq!(written.email_3.as_deref(), Some("felix@home.example"));
    assert_eq!(written.company.as_deref(), Some("Kylins"));
    assert_eq!(written.job_title.as_deref(), Some("Development Manager"));
    assert_eq!(
        written.body_plain.as_deref(),
        Some("Prefers plain-text bodies.")
    );
    assert_eq!(written.business_phone.as_deref(), Some("(206) 555-0103"));
    assert_eq!(written.business_2_phone.as_deref(), Some("(206) 555-0104"));
    assert_eq!(written.home_phone.as_deref(), Some("(206) 555-0101"));
    assert_eq!(written.home_2_phone.as_deref(), Some("(206) 555-0107"));
    assert_eq!(written.mobile_phone.as_deref(), Some("(206) 555-0102"));
    assert_eq!(written.assistant_phone.as_deref(), Some("(206) 555-0110"));
    assert_eq!(written.car_phone.as_deref(), Some("(206) 555-0106"));
    assert_eq!(
        written.company_main_phone.as_deref(),
        Some("(206) 555-0100")
    );
    assert_eq!(written.business_fax.as_deref(), Some("(206) 555-0105"));
    assert_eq!(written.home_fax.as_deref(), Some("(206) 555-0108"));
    assert_eq!(written.pager.as_deref(), Some("(206) 555-0109"));
    assert_eq!(written.radio_phone.as_deref(), Some("(206) 555-0111"));
    assert_eq!(
        written.business_address,
        Some(ContactsAddress {
            street: Some("1 Microsoft Way".into()),
            city: Some("Redmond".into()),
            state: Some("WA".into()),
            postal_code: Some("98052".into()),
            country: Some("United States".into()),
        })
    );
    assert_eq!(
        written.home_address,
        Some(ContactsAddress {
            street: Some("42 Pine Street".into()),
            city: Some("Seattle".into()),
            state: Some("Washington".into()),
            postal_code: Some("98101".into()),
            country: Some("USA".into()),
        })
    );
    assert_eq!(
        written.other_address,
        Some(ContactsAddress {
            street: Some("999 Robson Street".into()),
            city: Some("Vancouver".into()),
            state: Some("BC".into()),
            postal_code: Some("V5K 0A1".into()),
            country: Some("Canada".into()),
        })
    );
    assert_eq!(
        written.web_page.as_deref(),
        Some("https://example.com/fzhou")
    );
    assert_eq!(
        written.anniversary.as_deref(),
        Some("1998-05-23T11:59:00.000Z"),
        "the date-only neutral form re-serializes as the wire dateTime"
    );
    assert_eq!(
        written.birthday.as_deref(),
        Some("1975-11-30T11:59:00.000Z")
    );
    // Org-chart names and picture presence have no write slot — the
    // downsync degrades, so the upsync leaves them unset.
    assert_eq!(written.assistant_name, None);
    assert_eq!(written.manager_name, None);
    assert!(!written.picture_present);
}

/// Without FileAs the filing name composes from the parts — the display
/// composition, never invented from nothing.
#[test]
fn file_as_composes_from_parts_when_full_is_absent() {
    let mut card = comprehensive_card();
    let mut name = card.name.take().unwrap();
    name.full = None;
    card.name = Some(name);
    let written = write_from_draft(&card).expect("converts");
    assert_eq!(written.file_as.as_deref(), Some("Mr. Felix Ming Zhou Jr."));
}

/// A draft drops the neutral extras EAS has no slot for (the Graph
/// create-precedent) and never refuses them.
#[test]
fn a_draft_drops_unrepresentable_neutral_extras() {
    let mut card = comprehensive_card();
    card.nicknames
        .insert(id("nick"), ContactProperty::new(ContactNickname::new("F")));
    card.keywords.insert("friend".into());
    card.relations.insert(
        id("manager"),
        ContactProperty::new(ContactRelation {
            relation: BTreeSet::from(["manager".into()]),
            ..ContactRelation::default()
        }),
    );
    card.kind = engine_core::contact::ContactKind::Organization;
    assert!(write_from_draft(&card).is_ok());
}

/// The EAS slot caps refuse rather than silently drop write intent: a
/// fourth e-mail, a thirteenth phone, a second title, a second URL, a
/// second organization, organization units, and a second address routing
/// to the same set.
#[test]
fn slot_caps_refuse_instead_of_dropping_intent() {
    let mut card = comprehensive_card();
    card.emails.insert(
        id("email-4"),
        ContactProperty::new(ContactEmail::new("4@example.test")),
    );
    let err = write_from_draft(&card).expect_err("fourth email");
    assert_permanent_naming(&err, "Email3Address");

    let mut card = comprehensive_card();
    card.titles
        .insert(id("title-2"), ContactProperty::new(Title::default()));
    assert_permanent_naming(
        &write_from_draft(&card).expect_err("second title"),
        "JobTitle",
    );

    let mut card = comprehensive_card();
    card.urls.insert(
        id("url-2"),
        ContactProperty::new(ContactResource::default()),
    );
    assert_permanent_naming(&write_from_draft(&card).expect_err("second url"), "WebPage");

    let mut card = comprehensive_card();
    card.organizations.insert(
        id("org-2"),
        ContactProperty::new(engine_core::contact::Organization::default()),
    );
    assert_permanent_naming(
        &write_from_draft(&card).expect_err("second organization"),
        "CompanyName",
    );

    let mut card = comprehensive_card();
    let org = card.organizations.get_mut(&id("organization")).unwrap();
    org.value.units = vec![engine_core::contact::OrganizationUnit {
        name: "Research".into(),
        ..engine_core::contact::OrganizationUnit::default()
    }];
    assert_permanent_naming(
        &write_from_draft(&card).expect_err("organization units"),
        "Department",
    );

    let mut card = comprehensive_card();
    let mut second = ContactProperty::new(ContactAddress::default());
    second.contexts.insert("work".into());
    second
        .value
        .components
        .insert("street".into(), vec!["2 Main St".into()]);
    card.addresses.insert(id("work-2"), second);
    assert_permanent_naming(
        &write_from_draft(&card).expect_err("second work address"),
        "Business",
    );

    // A thirteenth phone: the neutral card with every slot taken plus an
    // unclassifiable extra.
    let mut card = comprehensive_card();
    card.phones.insert(
        id("extra"),
        ContactProperty::new(ContactPhone {
            number: "+1-extra".into(),
            ..ContactPhone::default()
        }),
    );
    assert_permanent_naming(&write_from_draft(&card).expect_err("13th phone"), "phone");
}

/// Phones route by their stable slot id first, then by feature/context
/// classification — a host-created phone (fresh id) lands by its tags.
#[test]
fn fresh_phone_ids_route_by_classification() {
    let mut card = comprehensive_card();
    card.phones.clear();
    for (pid, number, contexts, features) in [
        ("p-work", "+1-work", vec!["work"], vec![""]),
        ("p-home", "+1-home", vec!["private"], vec![""]),
        ("p-mobile", "+1-mobile", vec![], vec!["mobile"]),
        ("p-fax-work", "+1-fax-w", vec!["work"], vec!["fax"]),
        ("p-fax-home", "+1-fax-h", vec!["private"], vec!["fax"]),
        ("p-pager", "+1-pager", vec![], vec!["pager"]),
        ("p-plain", "+1-plain", vec![], vec![]),
    ] {
        let mut property = ContactProperty::new(ContactPhone {
            number: number.to_owned(),
            features: features
                .iter()
                .filter(|feature| !feature.is_empty())
                .map(|feature| (*feature).to_owned())
                .collect(),
        });
        property.contexts = contexts
            .iter()
            .map(|context| (*context).to_owned())
            .collect();
        card.phones.insert(id(pid), property);
    }
    let written = write_from_draft(&card).expect("converts");
    assert_eq!(written.business_phone.as_deref(), Some("+1-work"));
    assert_eq!(written.home_phone.as_deref(), Some("+1-home"));
    assert_eq!(written.mobile_phone.as_deref(), Some("+1-mobile"));
    assert_eq!(written.business_fax.as_deref(), Some("+1-fax-w"));
    assert_eq!(written.home_fax.as_deref(), Some("+1-fax-h"));
    assert_eq!(written.pager.as_deref(), Some("+1-pager"));
    assert_eq!(
        written.home_2_phone.as_deref(),
        Some("+1-plain"),
        "an unclassified phone takes the home overflow (the Graph else-route)"
    );
}

/// A patch Set replaces its field's whole slot family — leftover slots
/// clear (empty wire values), so a stale value cannot survive the replace.
#[test]
fn a_patch_set_replaces_the_whole_slot_family() {
    let card = comprehensive_card();
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
    assert_eq!(written.business_phone, None);

    // A Clear empties the family.
    let mut clear = ContactPatch::default();
    clear.fields.insert(ContactField::Emails, FieldPatch::Clear);
    let written = write_from_patch(&clear).expect("converts");
    assert_eq!(written.email_1, Some(String::new()));
    assert_eq!(written.email_2, Some(String::new()));
    assert_eq!(written.email_3, Some(String::new()));
    let _ = card;
}

/// A name Set emits the filing name plus every part; a name Clear empties
/// them all.
#[test]
fn a_name_patch_emits_file_as_and_every_part() {
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(
            serde_json::to_value(&ContactName {
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
        assert_permanent_naming(&err, "no slot");
        assert!(
            err.detail().contains("field"),
            "the refusal names the field: {}",
            err.detail()
        );
    }
}

/// A partial date form the wire cannot carry refuses rather than being
/// mangled into a wrong date.
#[test]
fn a_partial_anniversary_date_refuses() {
    let mut card = comprehensive_card();
    card.anniversaries
        .get_mut(&id("birthday"))
        .unwrap()
        .value
        .date = "1975-11".into();
    assert_permanent_naming(
        &write_from_draft(&card).expect_err("partial date"),
        "1975-11",
    );
}

fn one_email_map() -> BTreeMap<PropertyId, ContactProperty<ContactEmail>> {
    let mut map = BTreeMap::new();
    map.insert(
        id("email-1"),
        ContactProperty::new(ContactEmail::new("solo@example.test")),
    );
    map
}

fn assert_permanent_naming(err: &ProviderError, needle: &str) {
    assert!(
        matches!(err.class(), engine_core::error::FailureClass::Permanent),
        "the refusal is permanent: {err:?}"
    );
    assert!(
        err.detail().contains(needle),
        "the refusal names {needle:?}: {}",
        err.detail()
    );
}
