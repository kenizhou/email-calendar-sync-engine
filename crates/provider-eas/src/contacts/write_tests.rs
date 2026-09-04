// SPDX-License-Identifier: MPL-2.0
// Upsync draft-conversion tests: the neutral ContactCard → the
// ghost-model ContactsContactProps (P2 Task 5). The patch-conversion
// tests live in `write_patch_tests.rs` (the module split); the emission
// tests in `emit_tests.rs`; the envelope goldens (Add/Change/Delete
// around the ApplicationData) in tests/commands_sync/contacts_write.rs.

use std::collections::BTreeSet;

use engine_core::contact::{
    ContactCard, ContactEmail, ContactNickname, ContactPhone, ContactProperty, ContactRelation,
    ContactResource, PropertyId, Title,
};
use engine_provider::ProviderError;

use super::*;
use crate::contacts_testutil;

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

/// A comprehensive card shaped like the full wire fixture — every
/// representable field, distinct values, the downsync's own stable ids
/// (the shared fixture the patch/emission tests also derive from).
fn comprehensive_card() -> ContactCard {
    contacts_testutil::full_card()
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
