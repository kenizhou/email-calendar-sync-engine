// SPDX-License-Identifier: MPL-2.0
//! Wire-shape helpers for the adapter `fetch_message_source` scenarios
//! (`adapter_source_flow.rs`): the ItemOperations MIME-fetch response
//! builder (with the Range/Total/Truncated placement facts) and the request
//! decoders the scenarios assert against — split out of the flow file to
//! hold the 500-line cap (the `adapter_email_wire.rs` convention).

use provider_eas::{
    commands::{PAGE_AIRSYNC, PAGE_ITEM_OPS, pages},
    wbxml::{
        WbxmlElement,
        tags::{base, item_operations},
    },
};

use super::server::CapturedRequest;

/// An ItemOperations MIME-fetch response ([MS-ASCMD] §4.10.2.2 shape): Status
/// 1 at both levels, `Properties > airsyncbase:Body` with Type 4 + the chunk
/// text, and — when the server ranged or truncated the answer — the
/// placement facts `Properties > Range` ("m-n", authoritative),
/// `Properties > Total`, and the Body `Truncated` flag. The payload is
/// inline text (the live Exchange 2019 form; OPAQUE is covered by the
/// commands-layer goldens), so fixture chunks must be valid UTF-8.
pub(crate) fn mime_fetch_response(
    data: &[u8],
    range: Option<(u64, u64)>,
    total: Option<u64>,
    truncated: bool,
) -> WbxmlElement {
    let text = std::str::from_utf8(data).expect("fixture chunk is UTF-8 text");
    let mut body_children = vec![
        WbxmlElement::text(pages::BASE, base::TYPE, "4"),
        WbxmlElement::text(
            pages::BASE,
            base::ESTIMATED_DATA_SIZE,
            data.len().to_string(),
        ),
    ];
    if truncated {
        body_children.push(WbxmlElement::text(pages::BASE, base::TRUNCATED, "1"));
    }
    body_children.push(WbxmlElement::text(pages::BASE, base::DATA, text));
    let mut properties = vec![WbxmlElement::container(
        pages::BASE,
        base::BODY,
        body_children,
    )];
    if let Some((m, n)) = range {
        properties.push(WbxmlElement::text(
            PAGE_ITEM_OPS,
            item_operations::RANGE,
            format!("{m}-{n}"),
        ));
    }
    if let Some(total) = total {
        properties.push(WbxmlElement::text(
            PAGE_ITEM_OPS,
            item_operations::TOTAL,
            total.to_string(),
        ));
    }
    WbxmlElement::container(
        PAGE_ITEM_OPS,
        item_operations::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                item_operations::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    item_operations::FETCH,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            item_operations::PROPERTIES,
                            properties,
                        ),
                    ],
                )],
            ),
        ],
    )
}

/// An ItemOperations Fetch response whose Fetch-level Status is `status`
/// (the per-op code that overrides the top-level Status — the "absent item"
/// shape: [MS-ASCMD] §2.2.3.177.8, e.g. 6 "object was not found or access
/// denied").
pub(crate) fn fetch_status_response(status: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_ITEM_OPS,
        item_operations::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                item_operations::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    item_operations::FETCH,
                    vec![WbxmlElement::text(
                        PAGE_ITEM_OPS,
                        item_operations::STATUS,
                        status,
                    )],
                )],
            ),
        ],
    )
}

/// Decodes a captured request into its `ItemOperations > Fetch` children.
fn fetch_children(req: &CapturedRequest) -> Vec<WbxmlElement> {
    let tree = req.wbxml_tree().expect("request body decodes");
    let fetch = tree
        .children
        .iter()
        .find(|c| c.page == PAGE_ITEM_OPS && c.token == item_operations::FETCH)
        .unwrap_or_else(|| panic!("no Fetch in request tree"));
    fetch.children.clone()
}

/// The text of a direct `Fetch` child (`Store`, airsync:CollectionId,
/// airsync:ServerId), or `None` when the element is absent.
pub(crate) fn fetch_request_field(req: &CapturedRequest, page: u8, token: u8) -> Option<String> {
    fetch_children(req)
        .into_iter()
        .find(|c| c.page == page && c.token == token)
        .and_then(|c| match c.value {
            provider_eas::wbxml::WbxmlValue::Text(t) => Some(t),
            _ => None,
        })
}

/// The text of an `Options` child (`Range`, airsync:MIMESupport,
/// BodyPreference…), or `None` when the Options element or the child is
/// absent.
pub(crate) fn options_child_field(req: &CapturedRequest, page: u8, token: u8) -> Option<String> {
    let options = fetch_children(req)
        .into_iter()
        .find(|c| c.page == PAGE_ITEM_OPS && c.token == item_operations::OPTIONS)?;
    options
        .children
        .iter()
        .find(|c| c.page == page && c.token == token)
        .and_then(|c| match &c.value {
            provider_eas::wbxml::WbxmlValue::Text(t) => Some(t.clone()),
            _ => None,
        })
}

/// The `Options > airsyncbase:BodyPreference > Type` text — which body form
/// the fetch asked for (2 = HTML, 4 = MIME BLOB).
pub(crate) fn body_preference_type(req: &CapturedRequest) -> Option<String> {
    let options = fetch_children(req)
        .into_iter()
        .find(|c| c.page == PAGE_ITEM_OPS && c.token == item_operations::OPTIONS)?;
    let body_pref = options
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::BODY_PREFERENCE)?;
    body_pref
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::TYPE)
        .and_then(|c| match &c.value {
            provider_eas::wbxml::WbxmlValue::Text(t) => Some(t.clone()),
            _ => None,
        })
}

/// True when a page-20 `Range` element appears anywhere in the request.
pub(crate) fn request_has_range(req: &CapturedRequest) -> bool {
    fn walk(el: &WbxmlElement) -> bool {
        (el.page == PAGE_ITEM_OPS && el.token == item_operations::RANGE)
            || el.children.iter().any(walk)
    }
    walk(&req.wbxml_tree().expect("request body decodes"))
}

/// The airsync page-0 CollectionId token (re-exported so the flow file needs
/// one import site for the addressing constants).
pub(crate) const AS_COLLECTION_ID: u8 = 0x12;

/// The airsync page-0 ServerId token.
pub(crate) const AS_SERVER_ID: u8 = 0x0D;

/// The airsync page-0 MIMESupport token.
pub(crate) const AS_MIME_SUPPORT: u8 = 0x22;

/// The page (0) of the airsync addressing tokens above.
pub(crate) const AIRSYNC: u8 = PAGE_AIRSYNC;
