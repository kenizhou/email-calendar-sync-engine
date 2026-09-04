// SPDX-License-Identifier: MPL-2.0
//! The patch half of the contacts upsync conversion (P2 Task 5, split
//! from `write.rs` for the 500-line rule): `ContactPatch` → the ghost
//! model. Each patched field's whole slot family rides (a Set replaces
//! the family — leftover slots clear as empty values; a Clear empties
//! it); everything else stays ghosted (`None` = omit = unchanged). See
//! `write.rs` module docs for the family rules and the Graph-precedent
//! asymmetry (create drops unrepresentable extras; a patch REFUSES
//! them).

use std::collections::BTreeMap;

use engine_core::contact::{
    Anniversary, ContactAddress, ContactEmail, ContactField, ContactKind, ContactName, ContactNote,
    ContactPatch, ContactPhone, ContactProperty, FieldPatch, Organization, PropertyId, Title,
};
use engine_provider::{ProviderError, ProviderResult};
use serde::de::DeserializeOwned;

use super::write::{
    fill_addresses, fill_anniversaries, fill_emails, fill_notes, fill_organization, fill_phones,
    fill_title, fill_url, name_slots,
};
/// Converts a targeted patch into the wire model: each patched field's
use crate::contacts::ContactsContactProps;

/// Converts a targeted patch into the wire model: each patched field's
/// whole slot family rides (leftover slots clearing as empty values, the
/// Set-replaces-field rule), everything else stays ghosted (`None`).
///
/// # Errors
///
/// Refuses `Permanent` for a non-individual kind Set/Clear (the Graph
/// individual-only ruling), an unrepresentable field, a value the field
/// cannot decode, or any of the draft-path slot caps.
pub(crate) fn write_from_patch(patch: &ContactPatch) -> ProviderResult<ContactsContactProps> {
    match &patch.kind {
        None | Some(FieldPatch::Set(ContactKind::Individual)) => {}
        Some(FieldPatch::Set(kind)) => {
            return Err(ProviderError::permanent(format!(
                "EAS contacts hold only individual cards — kind {kind:?} has no slot"
            )));
        }
        Some(FieldPatch::Clear) => {
            return Err(ProviderError::permanent(
                "EAS contacts hold only individual cards — the kind cannot be cleared",
            ));
        }
    }
    let mut props = ContactsContactProps::default();
    for (field, patch) in &patch.fields {
        // The family-replace rule: a Set (like a Clear) emits the whole
        // slot family, so a leftover slot clears instead of surviving.
        let cleared = matches!(patch, FieldPatch::Clear);
        match field {
            ContactField::Name => {
                let name: Option<ContactName> = if cleared {
                    None
                } else {
                    let FieldPatch::Set(value) = patch else {
                        unreachable!("the cleared arm handled Clear");
                    };
                    Some(decode(value)?)
                };
                let (file_as, first, middle, last, suffix, prefix) = name_slots(name.as_ref());
                props.file_as = Some(file_as.unwrap_or_default());
                props.first_name = first.map_or_else(|| Some(String::new()), Some);
                props.middle_name = middle.map_or_else(|| Some(String::new()), Some);
                props.last_name = last.map_or_else(|| Some(String::new()), Some);
                props.name_suffix = suffix.map_or_else(|| Some(String::new()), Some);
                props.name_prefix = prefix.map_or_else(|| Some(String::new()), Some);
            }
            ContactField::Emails => {
                let values = decoded_map::<ContactEmail>(patch)?;
                fill_emails(&mut props, &values, true)?;
            }
            ContactField::Phones => {
                let values = decoded_map::<ContactPhone>(patch)?;
                fill_phones(&mut props, &values, true)?;
            }
            ContactField::Addresses => {
                let values = decoded_map::<ContactAddress>(patch)?;
                fill_addresses(&mut props, &values, true)?;
            }
            ContactField::Organizations => {
                let values = decoded_map::<Organization>(patch)?;
                fill_organization(&mut props, &values, true)?;
            }
            ContactField::Titles => {
                let values = decoded_map::<Title>(patch)?;
                fill_title(&mut props, &values, true)?;
            }
            ContactField::Notes => {
                let values = decoded_map::<ContactNote>(patch)?;
                fill_notes(&mut props, &values, true)?;
            }
            ContactField::Urls => {
                let values = decoded_map::<engine_core::contact::ContactResource>(patch)?;
                fill_url(&mut props, &values, true)?;
            }
            ContactField::Anniversaries => {
                let values = decoded_map::<Anniversary>(patch)?;
                fill_anniversaries(&mut props, &values, true)?;
            }
            other => {
                return Err(ProviderError::permanent(format!(
                    "EAS contacts have no slot for the {other:?} field — refusing the \
                     patch rather than silently dropping its intent"
                )));
            }
        }
    }
    Ok(props)
}

/// Decodes a Set's property map (the `ContactPatch` carries JSON
/// values); a Clear decodes to the empty map, which the family-replace
/// rule then emits as all-empty slots.
fn decoded_map<T: DeserializeOwned + Default>(
    patch: &FieldPatch<serde_json::Value>,
) -> ProviderResult<BTreeMap<PropertyId, ContactProperty<T>>> {
    match patch {
        FieldPatch::Set(value) => decode(value),
        FieldPatch::Clear => Ok(BTreeMap::new()),
    }
}

fn decode<T: DeserializeOwned>(value: &serde_json::Value) -> ProviderResult<T> {
    serde_json::from_value(value.clone()).map_err(|e| {
        ProviderError::permanent(format!("the contact patch value cannot decode: {e}"))
    })
}

#[cfg(test)]
#[path = "write_patch_tests.rs"]
mod tests;
