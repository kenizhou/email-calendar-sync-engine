// SPDX-License-Identifier: MPL-2.0
//! Wire-shape helpers for the adapter `stream_email` scenarios
//! (`adapter_email_flow.rs`): the Sync round/response builders and the
//! request-field decoder the scenarios assert against — split out of the
//! flow file to hold the 500-line cap (the `fixtures.rs` convention: canned
//! WBXML built with the crate's own serializer and public tag constants).

use provider_eas::{
    commands::{
        AS_ADD, AS_CHANGE, AS_COLLECTION, AS_COLLECTIONS, AS_COMMANDS, AS_DELETE, AS_SERVER_ID,
        AS_SYNC_KEY, PAGE_AIRSYNC,
    },
    wbxml::{WbxmlElement, tags::email},
};

use super::server::CapturedRequest;

/// One wire Add/Change email item: `(ServerId, Subject, From, Read,
/// DateReceived?)` — the fields the mapping projects onto the engine
/// `Message`.
pub(crate) type ItemSpec = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);

fn item_element(token: u8, spec: &ItemSpec) -> WbxmlElement {
    let &(id, subject, from, read, date) = spec;
    let mut app_data = vec![
        WbxmlElement::text(email::PAGE, email::SUBJECT, subject),
        WbxmlElement::text(email::PAGE, email::FROM, from),
        WbxmlElement::text(email::PAGE, email::READ, read),
    ];
    if let Some(date) = date {
        app_data.push(WbxmlElement::text(email::PAGE, email::DATE_RECEIVED, date));
    }
    WbxmlElement::container(
        PAGE_AIRSYNC,
        token,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, id),
            WbxmlElement::container(
                PAGE_AIRSYNC,
                provider_eas::commands::AS_APPLICATION_DATA,
                app_data,
            ),
        ],
    )
}

/// A Sync response round: collection status, rotated key, MoreAvailable, and
/// the Add/Update/Delete delta vocabulary (`fixtures::sync_response`'s
/// adds-only shape, extended for the adapter scenarios).
pub(crate) fn sync_round(
    status: &str,
    key: &str,
    more_available: bool,
    adds: &[ItemSpec],
    updates: &[ItemSpec],
    deletes: &[&str],
) -> WbxmlElement {
    let mut commands: Vec<WbxmlElement> = adds.iter().map(|s| item_element(AS_ADD, s)).collect();
    commands.extend(updates.iter().map(|s| item_element(AS_CHANGE, s)));
    commands.extend(deletes.iter().map(|id| {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_DELETE,
            vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, *id)],
        )
    }));
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, key),
        WbxmlElement::text(PAGE_AIRSYNC, provider_eas::commands::AS_STATUS, status),
    ];
    if more_available {
        collection.push(WbxmlElement::empty(
            PAGE_AIRSYNC,
            provider_eas::commands::AS_MORE_AVAILABLE,
        ));
    }
    if !commands.is_empty() {
        collection.push(WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, commands));
    }
    WbxmlElement::container(
        PAGE_AIRSYNC,
        provider_eas::commands::AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                collection,
            )],
        )],
    )
}

/// The text of a `Sync > Collections > Collection` child (`SyncKey`,
/// `CollectionId`, `WindowSize`, …) of a decoded Sync request.
pub(crate) fn request_field(req: &CapturedRequest, token: u8) -> String {
    let tree = req.wbxml_tree().expect("request body decodes");
    let collection = tree
        .children
        .iter()
        .find_map(|c| {
            if c.token == AS_COLLECTIONS {
                c.children
                    .iter()
                    .find(|cc| cc.token == AS_COLLECTION)
                    .cloned()
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("no Collection in request tree"));
    collection
        .children
        .iter()
        .find(|c| c.token == token)
        .and_then(|c| match &c.value {
            provider_eas::wbxml::WbxmlValue::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no field {token:#x} in Collection"))
}
