// SPDX-License-Identifier: MPL-2.0
//! [`ContactsContactProps`] → the engine's neutral [`ContactCard`] (P2
//! Task 5, the read-side conversion seam — pure, no IO, never panics on
//! wire data; the `calendar/convert.rs` precedent).
//!
//! ## Mapping
//!
//! Identity: `id` = the item **ServerId** (the store row key) — never an
//! invented identity, and never a panic: a server-controlled string that
//! cannot key a [`ContactId`] degrades to the fixed `eas:unkeyed`
//! placeholder the store reconciles away. Membership: the bound contact
//! folder's `AddressBookId` (the `folder` parameter — the Sync
//! `CollectionId` the adapter is bound to). `uid` stays `None`: the EAS
//! contacts class carries no UID element, and none is invented. The
//! account owner's folders are writable (`is_writable = true` — EAS
//! exposes no per-folder privileges to ask about), and every item of the
//! class is an `Individual` (distribution lists are a separate,
//! unmodeled container).
//!
//! Name: `FileAs` (how the contact is filed, [MS-ASCNTC] §2.2.2.30) →
//! `ContactName.full` — the provider-formatted full name, the Graph
//! `displayName` precedent — with the parts as ordered components
//! (`Title`→Prefix, `FirstName`→Given, `MiddleName`→Middle,
//! `LastName`→Surname, `Suffix`→Suffix). Parts alone still form a name
//! (`ContactName::display` composes them when FileAs is absent).
//!
//! Emails/phones: the wire's fixed slots become stable property ids
//! (`email-1`…, `phone-<slot>` — one per EAS slot). The slot tags are the
//! write path's routing table: a business slot carries the `work`
//! context, a home slot `private`, a mobile/car/pager/radio slot the
//! matching `features` entry, a fax slot `fax` plus its context. Slot
//! ids survive a host round-trip verbatim, so an edited card writes back
//! to the same slots.
//!
//! Addresses: the three wire sets (`Business`/`Home`/`Other`, five flat
//! components each) become three properties (`business`/`home`/`other`)
//! with `work`/`private`/`other` contexts and the vCard-meaning
//! component keys (`street`/`locality`/`region`/`postcode`/`country`).
//!
//! Organization/title/notes/url: `CompanyName` → the `organization`
//! property, `JobTitle` → `job-title`, the plain-text `Body` → `notes`,
//! `WebPage` → `web-page`. Anniversary/birthday keep only their DATE part
//! (the wire carries a full dateTime whose time part "might be 11:59 and
//! SHOULD be ignored", [MS-ASCNTC] §2.2.2.3 — the Graph birthday rule: a
//! timestamp must not leak into a date field) with kind `wedding`/`birth`.
//! The assistant/manager names land in `personal_info` (the neutral
//! model's only text-bearing org-chart slot; `ContactRelation` carries
//! no display text).
//!
//! **Degrades** (EAS has no slot, nothing is invented): no nicknames,
//! relations, languages, keywords, time zone, IM addresses, children,
//! office location, or department; `Picture` presence survives only as
//! the `eas/picture-present` extended fact — never a `media` entry,
//! because the bytes are dropped at parse time and no fetchable URI
//! exists (the photo stance — see the adapter's contacts module docs).

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactCard, ContactEmail, ContactKind, ContactName,
        ContactNote, ContactPhone, ContactProperty, ContactResource, ContactSourceClass,
        NameComponent, NameComponentKind, PersonalInfo, PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
};
use serde_json::json;

use super::model::{ContactsAddress, ContactsContactProps};

/// The adapter's extended-property namespace (every slice's convention).
const EXTENDED_NAMESPACE: &str = "eas";

/// Converts one downsynced Contacts item into the engine's neutral
/// `ContactCard`.
///
/// `book` is the contact folder's ServerId (the Sync collection the item
/// arrived under — the card's address-book membership); `server_id` the
/// item's ServerId (the store row key). Pure and total: empty wire values
/// (the ghosted-clear shape) and malformed shapes degrade per-field, never
/// panic, never drop the item.
pub(crate) fn contact_card_from_props(
    book: &AddressBookId,
    server_id: &str,
    props: &ContactsContactProps,
) -> ContactCard {
    let id = ContactId::try_from(server_id).unwrap_or_else(|e| {
        log::warn!(
            "contacts conversion: ServerId {server_id:?} cannot key a contact ({e}); the \
             item is kept under a placeholder key the store will reconcile away"
        );
        ContactId::try_from("eas:unkeyed").unwrap_or_else(|_| unreachable!("a fixed valid key"))
    });
    let mut card = ContactCard::new(id, Memberships::of_one(book.clone()));
    card.source_class = ContactSourceClass::Personal;
    // The class holds the account owner's own contacts: writable (EAS
    // exposes no per-folder privilege to ask about), individually typed.
    card.is_writable = true;
    card.kind = ContactKind::Individual;
    card.name = name_of(props);
    insert_emails(&mut card, props);
    insert_phones(&mut card, props);
    insert_addresses(&mut card, props);
    if let Some(company) = non_empty(props.company.as_deref()) {
        card.organizations.insert(
            slot_id("organization"),
            ContactProperty::new(engine_core::contact::Organization {
                name: company.to_owned(),
                ..engine_core::contact::Organization::default()
            }),
        );
    }
    if let Some(title) = non_empty(props.job_title.as_deref()) {
        card.titles.insert(
            slot_id("job-title"),
            ContactProperty::new(Title {
                name: title.to_owned(),
                ..Title::default()
            }),
        );
    }
    if let Some(note) = non_empty(props.body_plain.as_deref()) {
        card.notes.insert(
            slot_id("notes"),
            ContactProperty::new(ContactNote::new(note)),
        );
    }
    if let Some(date) = date_part(props.anniversary.as_deref()) {
        card.anniversaries.insert(
            slot_id("anniversary"),
            ContactProperty::new(Anniversary {
                date,
                kind: Some("wedding".into()),
                place: None,
            }),
        );
    }
    if let Some(date) = date_part(props.birthday.as_deref()) {
        card.anniversaries.insert(
            slot_id("birthday"),
            ContactProperty::new(Anniversary {
                date,
                kind: Some("birth".into()),
                place: None,
            }),
        );
    }
    if let Some(url) = non_empty(props.web_page.as_deref()) {
        card.urls.insert(
            slot_id("web-page"),
            ContactProperty::new(ContactResource {
                uri: url.to_owned(),
                ..ContactResource::default()
            }),
        );
    }
    // Org-chart names: the neutral model's only text-bearing slot for
    // them is personal info (a relation names its KIND, never a person).
    for (text, kind) in [
        (non_empty(props.assistant_name.as_deref()), "assistant"),
        (non_empty(props.manager_name.as_deref()), "manager"),
    ] {
        if let Some(value) = text {
            card.personal_info.insert(
                slot_id(kind),
                ContactProperty::new(PersonalInfo {
                    kind: kind.to_owned(),
                    value: value.to_owned(),
                }),
            );
        }
    }
    if props.picture_present {
        card.extended
            .set(format!("{EXTENDED_NAMESPACE}/picture-present"), json!(true));
    }
    card
}

/// The trimmed value, or `None` when absent/blank — the empty-value
/// degrade every string field shares (a ghosted clear reads as absent).
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The DATE part of a wire dateTime (`YYYY-MM-DDTHH:MM:SS.MSSZ` — the
/// time part "might be 11:59 and SHOULD be ignored"), or the verbatim
/// text when it carries no `T` (a partial date form). Blank → absent.
fn date_part(raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|raw| !raw.is_empty())?;
    Some(
        raw.split_once('T')
            .map_or(raw.to_owned(), |(date, _)| date.to_owned()),
    )
}

/// A stable slot property id — the write path's routing key.
fn slot_id(slot: &str) -> PropertyId {
    PropertyId::new(slot).unwrap_or_else(|e| unreachable!("a fixed non-empty slot id: {e}"))
}

/// The structured name: FileAs as the provider-formatted full name, the
/// five wire parts as ordered components. Absent when neither exists.
fn name_of(props: &ContactsContactProps) -> Option<ContactName> {
    let full = non_empty(props.file_as.as_deref()).map(str::to_owned);
    let mut components = Vec::new();
    for (value, kind) in [
        (
            non_empty(props.name_prefix.as_deref()),
            NameComponentKind::Prefix,
        ),
        (
            non_empty(props.first_name.as_deref()),
            NameComponentKind::Given,
        ),
        (
            non_empty(props.middle_name.as_deref()),
            NameComponentKind::Middle,
        ),
        (
            non_empty(props.last_name.as_deref()),
            NameComponentKind::Surname,
        ),
        (
            non_empty(props.name_suffix.as_deref()),
            NameComponentKind::Suffix,
        ),
    ] {
        if let Some(value) = value {
            components.push(NameComponent::new(kind, value.to_owned()));
        }
    }
    (full.is_some() || !components.is_empty()).then_some(ContactName {
        full,
        components,
        ..ContactName::default()
    })
}

/// The three e-mail slots, in wire order, under their stable ids.
fn insert_emails(card: &mut ContactCard, props: &ContactsContactProps) {
    for (value, id) in [
        (non_empty(props.email_1.as_deref()), "email-1"),
        (non_empty(props.email_2.as_deref()), "email-2"),
        (non_empty(props.email_3.as_deref()), "email-3"),
    ] {
        if let Some(address) = value {
            card.emails.insert(
                slot_id(id),
                ContactProperty::new(ContactEmail::new(address)),
            );
        }
    }
}

/// One phone property's shape for the routing table.
struct PhoneTag {
    /// Contexts (empty for the context-free slots).
    contexts: &'static [&'static str],
    /// Features (empty for the plain voice slots).
    features: &'static [&'static str],
}

/// The slot routing table: wire slot → (stable id, neutral tag). The ids
/// and tags are the write path's return route — an edited card's phones
/// land back in the slots they came from.
const PHONE_SLOTS: &[(&str, PhoneTag)] = &[
    (
        "phone-business",
        PhoneTag {
            contexts: &["work"],
            features: &[],
        },
    ),
    (
        "phone-business-2",
        PhoneTag {
            contexts: &["work"],
            features: &[],
        },
    ),
    (
        "phone-home",
        PhoneTag {
            contexts: &["private"],
            features: &[],
        },
    ),
    (
        "phone-home-2",
        PhoneTag {
            contexts: &["private"],
            features: &[],
        },
    ),
    (
        "phone-mobile",
        PhoneTag {
            contexts: &[],
            features: &["mobile"],
        },
    ),
    (
        "phone-assistant",
        PhoneTag {
            contexts: &[],
            features: &[],
        },
    ),
    (
        "phone-car",
        PhoneTag {
            contexts: &[],
            features: &["car"],
        },
    ),
    (
        "phone-company-main",
        PhoneTag {
            contexts: &["work"],
            features: &[],
        },
    ),
    (
        "phone-business-fax",
        PhoneTag {
            contexts: &["work"],
            features: &["fax"],
        },
    ),
    (
        "phone-home-fax",
        PhoneTag {
            contexts: &["private"],
            features: &["fax"],
        },
    ),
    (
        "phone-pager",
        PhoneTag {
            contexts: &[],
            features: &["pager"],
        },
    ),
    (
        "phone-radio",
        PhoneTag {
            contexts: &[],
            features: &["radio"],
        },
    ),
];

/// Every populated wire phone slot under its stable id and tag.
fn insert_phones(card: &mut ContactCard, props: &ContactsContactProps) {
    let numbers = [
        (non_empty(props.business_phone.as_deref()), "phone-business"),
        (
            non_empty(props.business_2_phone.as_deref()),
            "phone-business-2",
        ),
        (non_empty(props.home_phone.as_deref()), "phone-home"),
        (non_empty(props.home_2_phone.as_deref()), "phone-home-2"),
        (non_empty(props.mobile_phone.as_deref()), "phone-mobile"),
        (
            non_empty(props.assistant_phone.as_deref()),
            "phone-assistant",
        ),
        (non_empty(props.car_phone.as_deref()), "phone-car"),
        (
            non_empty(props.company_main_phone.as_deref()),
            "phone-company-main",
        ),
        (
            non_empty(props.business_fax.as_deref()),
            "phone-business-fax",
        ),
        (non_empty(props.home_fax.as_deref()), "phone-home-fax"),
        (non_empty(props.pager.as_deref()), "phone-pager"),
        (non_empty(props.radio_phone.as_deref()), "phone-radio"),
    ];
    for (number, id) in numbers {
        let Some(number) = number else {
            continue;
        };
        let tag = &PHONE_SLOTS
            .iter()
            .find(|(slot, _)| *slot == id)
            .expect("every slot id has a tag")
            .1;
        let phone = ContactPhone {
            number: number.to_owned(),
            features: tag
                .features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
        };
        let mut property = ContactProperty::new(phone);
        property.contexts = tag
            .contexts
            .iter()
            .map(|context| (*context).to_owned())
            .collect();
        card.phones.insert(slot_id(id), property);
    }
}

/// The three address sets under their stable ids and contexts.
fn insert_addresses(card: &mut ContactCard, props: &ContactsContactProps) {
    for (set, id, context) in [
        (&props.business_address, "business", "work"),
        (&props.home_address, "home", "private"),
        (&props.other_address, "other", "other"),
    ] {
        let Some(address) = set.as_ref().and_then(address_of) else {
            continue;
        };
        let mut property = ContactProperty::new(address);
        property.contexts = BTreeSet::from([context.to_owned()]);
        card.addresses.insert(slot_id(id), property);
    }
}

/// One wire address set → neutral components; an all-empty set maps to
/// `None` (the caller skips it — no phantom address).
fn address_of(set: &ContactsAddress) -> Option<ContactAddress> {
    let mut components: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (value, key) in [
        (non_empty(set.street.as_deref()), "street"),
        (non_empty(set.city.as_deref()), "locality"),
        (non_empty(set.state.as_deref()), "region"),
        (non_empty(set.postal_code.as_deref()), "postcode"),
        (non_empty(set.country.as_deref()), "country"),
    ] {
        if let Some(value) = value {
            components.insert(key.to_owned(), vec![value.to_owned()]);
        }
    }
    (!components.is_empty()).then_some(ContactAddress {
        components,
        ..ContactAddress::default()
    })
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
