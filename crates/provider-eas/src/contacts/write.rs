// SPDX-License-Identifier: MPL-2.0
//! The contacts upsync seam (P2 Task 5): the neutral `ContactCard` /
//! `ContactPatch` → the ghost-model [`ContactsContactProps`] → the
//! MS-ASCNTC `ApplicationData` element — the `calendar_write` precedent.
//!
//! ## The ghost model
//!
//! [`ContactsContactProps`] doubles as the WRITE model with EAS's own
//! element-ghosting semantics ([MS-ASCNTC] §2.2.2: every element "can be
//! ghosted" — an element ABSENT from a Sync Change leaves the server's
//! value unchanged): a `None` slot is OMITTED (the ghost — unchanged), a
//! `Some("")` slot emits the EMPTY-VALUE element (the clear), and a
//! `Some(value)` slot sets. That maps one-to-one onto the neutral
//! `FieldPatch` three-state, which is why the wire model needs no
//! second struct.
//!
//! ## Drafts vs patches (the Graph precedent)
//!
//! A **create** maps the representable fields and silently drops the
//! neutral extras EAS has no slot for (nicknames, relations, keywords —
//! the destination's `supported_fields` tells the caller what sticks):
//! a create must succeed for the common flow. A **patch** REFUSES an
//! unrepresentable field (`Permanent`): a targeted Set the transport
//! would silently drop is a contract violation, not a degrade — the
//! Graph `patch_body` ruling verbatim. Slot CAPS refuse on both paths
//! (a fourth e-mail, a thirteenth phone): dropping part of a mapped
//! field's intent is data loss, never a degrade.
//!
//! ## Slot routing
//!
//! Phones route **by their stable property id first** (the ids the
//! downsync assigns — an edited card writes back to the slots it came
//! from), then by feature/context classification for host-created ids:
//! `pager`/`car`/`radio`/`mobile` features name their slots, `fax`
//! splits by context, `work` fills the business chain, `private`/`home`
//! the home chain, and an unclassified phone takes the home overflow
//! (the Graph else-route). Chain overflow refuses. Addresses route by
//! context (`work`→Business, `private`→Home, else→Other). The filing
//! name composes from the parts when no full name is set (never from
//! nothing). Dates re-serialize as the wire dateTime: a date-only value
//! gains the `T11:59:00.000Z` tail the server itself sends ([MS-ASDTYPE]
//! §2.3's "might be 11:59"), a full form passes verbatim, a partial form
//! refuses rather than mangling.

use std::collections::BTreeMap;

use engine_core::contact::{
    Anniversary, ContactAddress, ContactCard, ContactEmail, ContactName, ContactNote, ContactPhone,
    ContactProperty, NameComponentKind, Organization, PropertyId, Title,
};
use engine_provider::{ProviderError, ProviderResult};

use super::model::{ContactsAddress, ContactsContactProps};

/// The three e-mail slots the wire carries ([MS-ASCNTC] §2.2.2.27-.29).
const EMAIL_SLOT_COUNT: usize = 3;

/// The stable slot ids the downsync assigns — the write path's id-first
/// phone routing table (must stay in lockstep with `convert`'s tags).
const PHONE_SLOT_IDS: [&str; 12] = [
    "phone-business",
    "phone-business-2",
    "phone-home",
    "phone-home-2",
    "phone-mobile",
    "phone-assistant",
    "phone-car",
    "phone-company-main",
    "phone-business-fax",
    "phone-home-fax",
    "phone-pager",
    "phone-radio",
];

/// The classification chain of one phone class: the slots a host-created
/// phone fills, in order, before the class refuses.
fn phone_chain(phone: &ContactProperty<ContactPhone>) -> &'static [&'static str] {
    const WORK: &[&str] = &["phone-business", "phone-business-2"];
    const HOME: &[&str] = &["phone-home", "phone-home-2"];
    let features = &phone.value.features;
    let private = phone.contexts.contains("private") || phone.contexts.contains("home");
    if features.contains("pager") {
        &["phone-pager"]
    } else if features.contains("car") {
        &["phone-car"]
    } else if features.contains("radio") {
        &["phone-radio"]
    } else if features.contains("mobile") {
        &["phone-mobile"]
    } else if features.contains("fax") {
        if private {
            &["phone-home-fax"]
        } else {
            &["phone-business-fax"]
        }
    } else if phone.contexts.contains("work") {
        WORK
    } else {
        HOME
    }
}

/// Converts a create draft's card into the wire model — every
/// representable slot filled, unrepresentable extras dropped (see the
/// module docs), slot caps refused.
///
/// # Errors
///
/// Refuses `Permanent` when a mapped field overflows its slots (a fourth
/// e-mail, a thirteenth phone, a second title/URL/organization, a second
/// address routing to the same set, organization units, or a date form
/// the wire cannot carry) — never silently dropping write intent.
pub(crate) fn write_from_draft(card: &ContactCard) -> ProviderResult<ContactsContactProps> {
    let mut props = ContactsContactProps::default();
    let (file_as, first, middle, last, suffix, prefix) = name_slots(card.name.as_ref());
    props.file_as = file_as;
    props.first_name = first;
    props.middle_name = middle;
    props.last_name = last;
    props.name_suffix = suffix;
    props.name_prefix = prefix;
    fill_emails(&mut props, &card.emails, false)?;
    fill_phones(&mut props, &card.phones, false)?;
    fill_addresses(&mut props, &card.addresses, false)?;
    fill_organization(&mut props, &card.organizations, false)?;
    fill_title(&mut props, &card.titles, false)?;
    fill_notes(&mut props, &card.notes, false)?;
    fill_url(&mut props, &card.urls, false)?;
    fill_anniversaries(&mut props, &card.anniversaries, false)?;
    Ok(props)
}

/// The six name slots: (FileAs, FirstName, MiddleName, LastName, Suffix,
/// Title-prefix).
type NameSlots = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The name slots: the filing name (`FileAs`) from the full name or the
/// composed display, the parts from joined same-kind components. `None`
/// for absent parts (the draft ghost); the patch path fills the gaps
/// with empties itself.
pub(super) fn name_slots(name: Option<&ContactName>) -> NameSlots {
    let Some(name) = name else {
        return (None, None, None, None, None, None);
    };
    let part = |kind: NameComponentKind| {
        let joined = name
            .components
            .iter()
            .filter(|component| component.kind == kind)
            .map(|component| component.value.trim())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!joined.is_empty()).then_some(joined)
    };
    (
        name.display().filter(|full| !full.trim().is_empty()),
        part(NameComponentKind::Given),
        part(NameComponentKind::Middle),
        part(NameComponentKind::Surname),
        part(NameComponentKind::Suffix),
        part(NameComponentKind::Prefix),
    )
}

/// Fills the e-mail slots: the first three values in property order; a
/// fourth refuses. On the patch path every slot rides (leftovers clear).
pub(super) fn fill_emails(
    props: &mut ContactsContactProps,
    emails: &BTreeMap<PropertyId, ContactProperty<ContactEmail>>,
    family: bool,
) -> ProviderResult<()> {
    let mut slots: [Option<String>; EMAIL_SLOT_COUNT] = [None, None, None];
    for (index, (_, email)) in emails.iter().enumerate() {
        let Some(slot) = slots.get_mut(index) else {
            return Err(ProviderError::permanent(format!(
                "EAS contacts carry exactly three e-mail slots (Email1Address-Email3Address) — \
                 the {}-th address has nowhere to go",
                index + 1
            )));
        };
        *slot = Some(email.value.address.clone());
    }
    for (field, index) in [
        (&mut props.email_1, 0usize),
        (&mut props.email_2, 1),
        (&mut props.email_3, 2),
    ] {
        if family || slots[index].is_some() {
            *field = Some(slots[index].take().unwrap_or_default());
        }
    }
    Ok(())
}

/// Fills the phone slots: id-first routing (the downsync's stable ids),
/// then classification chains; every collision that cannot overflow
/// refuses. On the patch path all twelve slots ride.
pub(super) fn fill_phones(
    props: &mut ContactsContactProps,
    phones: &BTreeMap<PropertyId, ContactProperty<ContactPhone>>,
    family: bool,
) -> ProviderResult<()> {
    let mut taken: BTreeMap<&str, String> = BTreeMap::new();
    // Pass 1 — the stable ids place directly (unique keys, no overflow:
    // the id IS the intent).
    for (pid, phone) in phones {
        if let Some(slot) = PHONE_SLOT_IDS.iter().find(|slot| **slot == pid.as_str()) {
            taken.insert(slot, phone.value.number.clone());
        }
    }
    // Pass 2 — classification fills the first free slot of its chain.
    for (pid, phone) in phones {
        if PHONE_SLOT_IDS.contains(&pid.as_str()) {
            continue;
        }
        let chain = phone_chain(phone);
        let Some(slot) = chain.iter().find(|slot| !taken.contains_key(*slot)) else {
            return Err(ProviderError::permanent(format!(
                "the phone {} ({}) has no free EAS slot left — the {} chain is full",
                pid.as_str(),
                phone.value.number,
                chain[0]
            )));
        };
        taken.insert(slot, phone.value.number.clone());
    }
    let mut assignments: [(&mut Option<String>, &str); 12] = [
        (&mut props.business_phone, "phone-business"),
        (&mut props.business_2_phone, "phone-business-2"),
        (&mut props.home_phone, "phone-home"),
        (&mut props.home_2_phone, "phone-home-2"),
        (&mut props.mobile_phone, "phone-mobile"),
        (&mut props.assistant_phone, "phone-assistant"),
        (&mut props.car_phone, "phone-car"),
        (&mut props.company_main_phone, "phone-company-main"),
        (&mut props.business_fax, "phone-business-fax"),
        (&mut props.home_fax, "phone-home-fax"),
        (&mut props.pager, "phone-pager"),
        (&mut props.radio_phone, "phone-radio"),
    ];
    for (field, name) in &mut assignments {
        if let Some(value) = taken.get(name)
            && (family || !value.is_empty())
        {
            **field = Some(value.clone());
        }
    }
    Ok(())
}

/// Fills the address sets: routing by context (`work`→Business,
/// `private`/`home`→Home, else→Other), one per set, components by their
/// vCard-meaning keys. On the patch path all three sets ride.
pub(super) fn fill_addresses(
    props: &mut ContactsContactProps,
    addresses: &BTreeMap<PropertyId, ContactProperty<ContactAddress>>,
    family: bool,
) -> ProviderResult<()> {
    let mut business: Option<&ContactAddress> = None;
    let mut home: Option<&ContactAddress> = None;
    let mut other: Option<&ContactAddress> = None;
    for (pid, address) in addresses {
        let (slot, label) = if address.contexts.contains("work") {
            (&mut business, "Business")
        } else if address.contexts.contains("private") || address.contexts.contains("home") {
            (&mut home, "Home")
        } else {
            (&mut other, "Other")
        };
        if slot.is_some() {
            return Err(ProviderError::permanent(format!(
                "two addresses ({}) route to the same EAS {label} set — the wire carries one",
                pid.as_str()
            )));
        }
        *slot = Some(&address.value);
    }
    let fill = |target: &mut Option<ContactsAddress>, source: Option<&ContactAddress>| {
        if family || source.is_some() {
            let component = |key: &str| -> Option<String> {
                source
                    .and_then(|address| address.components.get(key))
                    .and_then(|values| values.first())
                    .map(|value| value.trim())
                    .filter(|value| family || !value.is_empty())
                    .map(str::to_owned)
            };
            *target = Some(ContactsAddress {
                street: component("street"),
                city: component("locality"),
                state: component("region"),
                postal_code: component("postcode"),
                country: component("country"),
            });
        }
    };
    fill(&mut props.business_address, business);
    fill(&mut props.home_address, home);
    fill(&mut props.other_address, other);
    Ok(())
}

/// The organization slot: the first organization's name; a second
/// refuses, and units refuse (no modeled Department element).
pub(super) fn fill_organization(
    props: &mut ContactsContactProps,
    organizations: &BTreeMap<PropertyId, ContactProperty<Organization>>,
    family: bool,
) -> ProviderResult<()> {
    if organizations.len() > 1 {
        return Err(ProviderError::permanent(
            "EAS contacts carry one CompanyName slot — a second organization has nowhere to go",
        ));
    }
    if let Some((_, organization)) = organizations.iter().next() {
        if !organization.value.units.is_empty() {
            return Err(ProviderError::permanent(format!(
                "EAS write model has no Department slot — the organization {:?} carries \
                 {} unit(s) that would be lost",
                organization.value.name,
                organization.value.units.len()
            )));
        }
        props.company = Some(organization.value.name.clone());
    } else if family {
        props.company = Some(String::new());
    }
    Ok(())
}

/// The title slot: the first title's name; a second refuses.
pub(super) fn fill_title(
    props: &mut ContactsContactProps,
    titles: &BTreeMap<PropertyId, ContactProperty<Title>>,
    family: bool,
) -> ProviderResult<()> {
    if titles.len() > 1 {
        return Err(ProviderError::permanent(
            "EAS contacts carry one JobTitle slot — a second title has nowhere to go",
        ));
    }
    if let Some((_, title)) = titles.iter().next() {
        props.job_title = Some(title.value.name.clone());
    } else if family {
        props.job_title = Some(String::new());
    }
    Ok(())
}

/// The notes slot: the notes joined with newlines (the Graph precedent).
#[allow(
    clippy::unnecessary_wraps,
    reason = "the fill_* family shares one Result-returning shape"
)]
pub(super) fn fill_notes(
    props: &mut ContactsContactProps,
    notes: &BTreeMap<PropertyId, ContactProperty<ContactNote>>,
    family: bool,
) -> ProviderResult<()> {
    if !notes.is_empty() {
        props.body_plain = Some(
            notes
                .values()
                .map(|note| note.value.note.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    } else if family {
        props.body_plain = Some(String::new());
    }
    Ok(())
}

/// The URL slot: the first resource's URI; a second refuses.
pub(super) fn fill_url(
    props: &mut ContactsContactProps,
    urls: &BTreeMap<PropertyId, ContactProperty<engine_core::contact::ContactResource>>,
    family: bool,
) -> ProviderResult<()> {
    if urls.len() > 1 {
        return Err(ProviderError::permanent(
            "EAS contacts carry one WebPage slot — a second URL has nowhere to go",
        ));
    }
    if let Some((_, url)) = urls.iter().next() {
        props.web_page = Some(url.value.uri.clone());
    } else if family {
        props.web_page = Some(String::new());
    }
    Ok(())
}

/// The date slots: kind `birth` → Birthday, `wedding`/unset →
/// Anniversary; one each, place-less, wire-serialized.
pub(super) fn fill_anniversaries(
    props: &mut ContactsContactProps,
    anniversaries: &BTreeMap<PropertyId, ContactProperty<Anniversary>>,
    family: bool,
) -> ProviderResult<()> {
    let mut anniversary: Option<String> = None;
    let mut birthday: Option<String> = None;
    for (pid, entry) in anniversaries {
        let slot = match entry.value.kind.as_deref() {
            Some("birth") => &mut birthday,
            Some("wedding") | None => &mut anniversary,
            Some(kind) => {
                return Err(ProviderError::permanent(format!(
                    "EAS contacts carry only wedding-anniversary and birthday slots — the \
                     {kind} date ({}) has no slot",
                    pid.as_str()
                )));
            }
        };
        if slot.is_some() {
            return Err(ProviderError::permanent(
                "two anniversaries route to the same EAS date slot — the wire carries one",
            ));
        }
        if entry.value.place.is_some() {
            return Err(ProviderError::permanent(format!(
                "EAS date slots carry no place — the place of {} would be lost",
                pid.as_str()
            )));
        }
        *slot = Some(wire_date(&entry.value.date)?);
    }
    if family || anniversary.is_some() {
        props.anniversary = Some(anniversary.unwrap_or_default());
    }
    if family || birthday.is_some() {
        props.birthday = Some(birthday.unwrap_or_default());
    }
    Ok(())
}

/// Re-serializes a neutral date as the wire dateTime ([MS-ASDTYPE]
/// §2.3): a date-only value gains the `T11:59:00.000Z` tail the server
/// itself sends; a full `…Z` form passes verbatim; anything else refuses
/// rather than mangling.
fn wire_date(date: &str) -> ProviderResult<String> {
    let bytes = date.as_bytes();
    let date_only = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if date_only {
        return Ok(format!("{date}T11:59:00.000Z"));
    }
    if date.contains('T') && date.ends_with('Z') {
        return Ok(date.to_owned());
    }
    Err(ProviderError::permanent(format!(
        "the anniversary date {date:?} is neither a date-only value nor a wire dateTime — \
         refusing rather than mangling it"
    )))
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
