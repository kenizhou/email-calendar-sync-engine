// SPDX-License-Identifier: MPL-2.0
//! ItemOperations fetch requests WITH `Options > Range` — the ranged-fetch
//! shapes Task 5 (eas-adapter) added for the message-source reassembly loop.
//! Split from `fetch_requests.rs` to hold the 500-line cap (the
//! `adapter_source_wire.rs` convention); the unranged shapes stay there.
//!
//! Spec anchors:
//! - §2.2.3.143.2 Range (ItemOperations): request child of Options; value "m-n", zero-indexed,
//!   inclusive; omitting it fetches the whole item; the server's fulfillment is best-effort (the
//!   RESPONSE's Properties>Range is the authoritative placement).
//! - §2.2.3.125.3 Options (ItemOperations): with FileReference present, Range is the ONLY valid
//!   Options child. The fetch-operation child list runs Schema, Range, UserName, Password,
//!   MIMESupport, BodyPreference — Range goes FIRST in the item branch, ahead of MIMESupport.

use super::*;

/// Ranged MIME fetch: Options must carry [Range(20,0x09)="m-n",
/// MIMESupport(0,0x22)="2", BodyPreference(17,0x05)>Type 4] in that order —
/// Range first per the §2.2.3.125.3 child list, MIMESupport before
/// BodyPreference per the §4.10.2.1 example.
#[test]
fn item_operations_request_mime_fetch_with_range_wire_shape() {
    let req = ItemOperationsFetchRequest {
        server_id: "17:11".to_string(),
        collection_id: "17".to_string(),
        file_reference: None,
        long_id: None,
        mime: true,
        accept_multipart: false,
        range: Some((0, 511)),
    };
    let tree = build_item_operations_request(&req);
    let fetch = &tree.children[0];
    let options = &fetch.children[3];
    assert_eq!(
        (options.page, options.token),
        (PAGE_ITEM_OPS, 0x08),
        "Options must stay the fourth Fetch child"
    );
    assert_eq!(
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>(),
        vec![
            (PAGE_ITEM_OPS, tags::item_operations::RANGE),
            (PAGE_AIRSYNC, 0x22),
            (pages::BASE, tags::base::BODY_PREFERENCE),
        ],
        "Range BEFORE MIMESupport BEFORE BodyPreference"
    );
    let range = &options.children[0];
    assert_eq!(range.tag_name(), "Range");
    assert_eq!(
        text_value(range).unwrap(),
        "0-511",
        "the wire form is the zero-indexed inclusive \"m-n\" string"
    );
}

/// A mid-stream continuation range (the reassembly loop's second+ rounds)
/// formats verbatim: the assembled-so-far length is the new start.
#[test]
fn item_operations_request_continuation_range_wire_text() {
    let req = ItemOperationsFetchRequest {
        server_id: "17:11".to_string(),
        collection_id: "17".to_string(),
        file_reference: None,
        long_id: None,
        mime: true,
        accept_multipart: false,
        range: Some((512, 1023)),
    };
    let tree = build_item_operations_request(&req);
    let options = &tree.children[0].children[3];
    assert_eq!(text_value(&options.children[0]).unwrap(), "512-1023");
}

/// Attachment fetch with a range ([MS-ASCMD] §2.2.3.125.3: with FileReference
/// present, Range is the ONLY valid Options child): Fetch carries
/// Store + FileReference + Options>[Range] — and nothing else in Options.
#[test]
fn item_operations_request_attachment_with_range_emits_range_only_options() {
    let req = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: Some("fileref-1".to_string()),
        long_id: None,
        mime: false,
        accept_multipart: false,
        range: Some((0, 255)),
    };
    let tree = build_item_operations_request(&req);
    let fetch = &tree.children[0];
    assert_eq!(fetch.children.len(), 3, "Store + FileReference + Options");
    let options = &fetch.children[2];
    assert_eq!(
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>(),
        vec![(PAGE_ITEM_OPS, tags::item_operations::RANGE)],
        "Range is the only valid Options child when FileReference is present"
    );
    assert_eq!(text_value(&options.children[0]).unwrap(), "0-255");
}

/// Ranged MIME fetch round-trips through the WBXML codec losslessly.
#[test]
fn item_operations_request_ranged_mime_round_trips() {
    let req = ItemOperationsFetchRequest {
        server_id: "17:11".to_string(),
        collection_id: "17".to_string(),
        file_reference: None,
        long_id: None,
        mime: true,
        accept_multipart: false,
        range: Some((120, 299)),
    };
    let tree = build_item_operations_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Regression guard: `range: None` (every pre-Task-5 caller) keeps the exact
/// prior shapes — no Range element appears anywhere in the tree, and the
/// attachment branch emits NO Options at all.
#[test]
fn item_operations_request_without_range_keeps_prior_shapes() {
    let req = ItemOperationsFetchRequest {
        server_id: "7:3".to_string(),
        collection_id: "7".to_string(),
        file_reference: None,
        long_id: None,
        mime: false,
        accept_multipart: false,
        range: None,
    };
    let tree = build_item_operations_request(&req);
    assert!(
        !contains_range(&tree),
        "no Range element when range is None"
    );
    let attachment = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: Some("fileref-1".to_string()),
        long_id: None,
        mime: false,
        accept_multipart: false,
        range: None,
    };
    let attachment_tree = build_item_operations_request(&attachment);
    assert_eq!(
        attachment_tree.children[0].children.len(),
        2,
        "attachment without a range keeps Store + FileReference only"
    );
}

/// Whether any element in the tree is the page-20 Range token.
fn contains_range(el: &WbxmlElement) -> bool {
    if el.page == PAGE_ITEM_OPS && el.token == tags::item_operations::RANGE {
        return true;
    }
    el.children.iter().any(contains_range)
}
