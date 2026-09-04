// SPDX-License-Identifier: MPL-2.0
// Emission tests: the ghost model on the wire (split from write_tests.rs
// with the emit module for the 500-line rule).

use engine_core::ids::AddressBookId;

use super::*;
use crate::contacts::{
    ContactsContactProps, contact_card_from_props, parse_contacts_application_data,
    write::write_from_draft,
};
use crate::contacts_testutil;
use crate::wbxml::{WbxmlValue, tags::base, tags::pages};

/// The full-fixture card — the same construction the conversion tests
/// use (the downsync of the shared golden fixture).
fn comprehensive_card() -> engine_core::contact::ContactCard {
    let props = contacts_testutil::expected_full_contact_props();
    contact_card_from_props(
        &AddressBookId::try_from("fid-contacts-1").unwrap(),
        "srv:con-1",
        &props,
    )
}

/// The emission: every Some slot lands under its token on its page, None
/// slots are omitted (the ghost), and Some("") emits the empty-value
/// clear element. The page-12 CompanyMainPhone switches code pages
/// mid-container.
#[test]
fn application_data_emission_matches_the_token_layout() {
    let written = write_from_draft(&comprehensive_card()).expect("converts");
    let app_data = build_contacts_application_data(&written);

    let text_of = |page: u8, token: u8| -> Option<String> {
        app_data.children.iter().find_map(|child| {
            (child.page == page && child.token == token).then(|| match &child.value {
                WbxmlValue::Text(text) => text.clone(),
                other => panic!("expected text, got {other:?}"),
            })
        })
    };
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_FILE_AS).as_deref(),
        Some("Zhou, Felix")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_FIRST_NAME).as_deref(),
        Some("Felix")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_EMAIL_1).as_deref(),
        Some("felixzhou@kylins.local")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_EMAIL_3).as_deref(),
        Some("felix@home.example")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_COMPANY_NAME).as_deref(),
        Some("Kylins")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_MOBILE_PHONE).as_deref(),
        Some("(206) 555-0102")
    );
    assert_eq!(
        text_of(pages::CONTACTS2, CON2_COMPANY_MAIN_PHONE).as_deref(),
        Some("(206) 555-0100"),
        "the page-12 element switches code pages mid-container"
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_STREET).as_deref(),
        Some("1 Microsoft Way")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_WEB_PAGE).as_deref(),
        Some("https://example.com/fzhou")
    );
    assert_eq!(
        text_of(PAGE_CONTACTS, CON_ANNIVERSARY).as_deref(),
        Some("1998-05-23T11:59:00.000Z")
    );

    // The Body container: Type 1 plain text with the notes as Data.
    let body = app_data
        .children
        .iter()
        .find(|child| child.page == pages::BASE && child.token == base::BODY)
        .expect("the Body container rides");
    assert_eq!(body.children.len(), 2);
    assert_eq!(
        (body.children[0].page, body.children[0].token),
        (pages::BASE, base::TYPE)
    );
    assert_eq!(
        match &body.children[0].value {
            WbxmlValue::Text(text) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        "1"
    );

    // The ghost: a props slot that is None emits no element.
    let sparse = ContactsContactProps {
        file_as: Some("Solo".into()),
        ..ContactsContactProps::default()
    };
    let sparse_data = build_contacts_application_data(&sparse);
    assert_eq!(sparse_data.children.len(), 1, "only FileAs rides");

    // The clear: Some("") emits the empty-value element.
    let clearing = ContactsContactProps {
        email_1: Some(String::new()),
        ..ContactsContactProps::default()
    };
    let clearing_data = build_contacts_application_data(&clearing);
    assert_eq!(clearing_data.children.len(), 1);
    assert_eq!(
        match &clearing_data.children[0].value {
            WbxmlValue::Text(text) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        "",
        "the clear is an empty-VALUE element, not an omitted one"
    );
}

/// The emitted ApplicationData parses back through the downsync parser —
/// the emission and the parser agree on the wire shape.
#[test]
fn the_emission_round_trips_through_the_downsync_parser() {
    let written = write_from_draft(&comprehensive_card()).expect("converts");
    let app_data = build_contacts_application_data(&written);
    let reparsed = parse_contacts_application_data(&app_data).expect("the emission parses");
    let expected = contacts_testutil::expected_full_contact_props();
    // Compare the representable subset: everything except the fields the
    // write path deliberately leaves (assistant/manager names, picture).
    let mut round = reparsed;
    round.assistant_name = None;
    round.manager_name = None;
    round.picture_present = false;
    let mut want = expected;
    want.assistant_name = None;
    want.manager_name = None;
    want.picture_present = false;
    assert_eq!(round, want);
}
