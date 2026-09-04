// SPDX-License-Identifier: MPL-2.0
//! The Contacts-class `ApplicationData` emission (P2 Task 5, split from
//! `write.rs` for the 500-line rule): the ghost model on the wire — the
//! write twin of `parse_contacts_application_data`. Canonical element
//! order; a `None` slot is OMITTED (the ghost — unchanged on the
//! server), a `Some("")` slot emits the empty-value clear element. The
//! page-12 `contacts2:CompanyMainPhone` and the page-17
//! `airsyncbase:Body` switch code pages mid-container (the email-Flag
//! precedent). Infallible by construction.

use super::{
    model::{ContactsAddress, ContactsContactProps},
    tokens::{
        CON_ANNIVERSARY, CON_ASSISTANT_PHONE, CON_BIRTHDAY, CON_BUSINESS_2_PHONE,
        CON_BUSINESS_ADDRESS_CITY, CON_BUSINESS_ADDRESS_COUNTRY, CON_BUSINESS_ADDRESS_POSTAL_CODE,
        CON_BUSINESS_ADDRESS_STATE, CON_BUSINESS_ADDRESS_STREET, CON_BUSINESS_FAX,
        CON_BUSINESS_PHONE, CON_CAR_PHONE, CON_COMPANY_NAME, CON_EMAIL_1, CON_EMAIL_2, CON_EMAIL_3,
        CON_FILE_AS, CON_FIRST_NAME, CON_HOME_2_PHONE, CON_HOME_ADDRESS_CITY,
        CON_HOME_ADDRESS_COUNTRY, CON_HOME_ADDRESS_POSTAL_CODE, CON_HOME_ADDRESS_STATE,
        CON_HOME_ADDRESS_STREET, CON_HOME_FAX, CON_HOME_PHONE, CON_JOB_TITLE, CON_LAST_NAME,
        CON_MIDDLE_NAME, CON_MOBILE_PHONE, CON_OTHER_ADDRESS_CITY, CON_OTHER_ADDRESS_COUNTRY,
        CON_OTHER_ADDRESS_POSTAL_CODE, CON_OTHER_ADDRESS_STATE, CON_OTHER_ADDRESS_STREET,
        CON_PAGER, CON_RADIO_PHONE, CON_SUFFIX, CON_TITLE, CON_WEB_PAGE, CON2_COMPANY_MAIN_PHONE,
        PAGE_CONTACTS,
    },
};
use crate::wbxml::{
    WbxmlElement,
    tags::{base, pages},
};

// ============================================================================
// ApplicationData emission
// ============================================================================

/// One ghost-model slot's element: `None` omits (the ghost), `Some`
/// emits (an empty value clears).
fn text(children: &mut Vec<WbxmlElement>, page: u8, token: u8, value: Option<&str>) {
    if let Some(value) = value {
        children.push(WbxmlElement::text(page, token, value.to_owned()));
    }
}

/// Builds the Contacts-class `ApplicationData` element (page 0, 0x1D)
/// for a Sync Add/Change — the write twin of
/// `contacts::parse_contacts_application_data`. Canonical element order
/// (the module header's table); a `None` slot is OMITTED (the ghost —
/// unchanged on the server), a `Some("")` slot emits the empty-value
/// clear element. The page-12 `contacts2:CompanyMainPhone` and the
/// page-17 `airsyncbase:Body` switch code pages mid-container (the
/// email-Flag precedent). Infallible by construction.
pub fn build_contacts_application_data(props: &ContactsContactProps) -> WbxmlElement {
    let mut children = Vec::with_capacity(32);
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_FILE_AS,
        props.file_as.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_FIRST_NAME,
        props.first_name.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_MIDDLE_NAME,
        props.middle_name.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_LAST_NAME,
        props.last_name.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_SUFFIX,
        props.name_suffix.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_TITLE,
        props.name_prefix.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_EMAIL_1,
        props.email_1.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_EMAIL_2,
        props.email_2.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_EMAIL_3,
        props.email_3.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_COMPANY_NAME,
        props.company.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_JOB_TITLE,
        props.job_title.as_deref(),
    );
    if let Some(data) = &props.body_plain {
        children.push(WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![
                WbxmlElement::text(pages::BASE, base::TYPE, "1"),
                WbxmlElement::text(pages::BASE, base::DATA, data.clone()),
            ],
        ));
    }
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_BUSINESS_PHONE,
        props.business_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_BUSINESS_2_PHONE,
        props.business_2_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_HOME_PHONE,
        props.home_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_HOME_2_PHONE,
        props.home_2_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_MOBILE_PHONE,
        props.mobile_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_ASSISTANT_PHONE,
        props.assistant_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_CAR_PHONE,
        props.car_phone.as_deref(),
    );
    text(
        &mut children,
        pages::CONTACTS2,
        CON2_COMPANY_MAIN_PHONE,
        props.company_main_phone.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_BUSINESS_FAX,
        props.business_fax.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_HOME_FAX,
        props.home_fax.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_PAGER,
        props.pager.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_RADIO_PHONE,
        props.radio_phone.as_deref(),
    );
    emit_address(
        &mut children,
        props.business_address.as_ref(),
        (
            CON_BUSINESS_ADDRESS_STREET,
            CON_BUSINESS_ADDRESS_CITY,
            CON_BUSINESS_ADDRESS_STATE,
            CON_BUSINESS_ADDRESS_POSTAL_CODE,
            CON_BUSINESS_ADDRESS_COUNTRY,
        ),
    );
    emit_address(
        &mut children,
        props.home_address.as_ref(),
        (
            CON_HOME_ADDRESS_STREET,
            CON_HOME_ADDRESS_CITY,
            CON_HOME_ADDRESS_STATE,
            CON_HOME_ADDRESS_POSTAL_CODE,
            CON_HOME_ADDRESS_COUNTRY,
        ),
    );
    emit_address(
        &mut children,
        props.other_address.as_ref(),
        (
            CON_OTHER_ADDRESS_STREET,
            CON_OTHER_ADDRESS_CITY,
            CON_OTHER_ADDRESS_STATE,
            CON_OTHER_ADDRESS_POSTAL_CODE,
            CON_OTHER_ADDRESS_COUNTRY,
        ),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_WEB_PAGE,
        props.web_page.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_ANNIVERSARY,
        props.anniversary.as_deref(),
    );
    text(
        &mut children,
        PAGE_CONTACTS,
        CON_BIRTHDAY,
        props.birthday.as_deref(),
    );
    WbxmlElement::container(
        crate::commands::PAGE_AIRSYNC,
        crate::commands::AS_APPLICATION_DATA,
        children,
    )
}

/// Emits one address set's five flat components when the set rides.
fn emit_address(
    children: &mut Vec<WbxmlElement>,
    set: Option<&ContactsAddress>,
    tokens: (u8, u8, u8, u8, u8),
) {
    let Some(set) = set else {
        return;
    };
    for (token, value) in [
        (tokens.0, set.street.as_deref()),
        (tokens.1, set.city.as_deref()),
        (tokens.2, set.state.as_deref()),
        (tokens.3, set.postal_code.as_deref()),
        (tokens.4, set.country.as_deref()),
    ] {
        text(children, PAGE_CONTACTS, token, value);
    }
}

#[cfg(test)]
#[path = "emit_tests.rs"]
mod tests;
