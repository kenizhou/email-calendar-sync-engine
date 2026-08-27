// SPDX-License-Identifier: MPL-2.0
//! ItemOperations fetch responses: spec shape, body payload, type-4 fallback, status.

use super::*;

/// Spec-shaped response parse: ItemOperations(20,0x05) > Status(20,0x0D)"1"
/// + Response(20,0x0E) > Fetch(20,0x06) > { Status(20,0x0D)"1", Properties(20,0x0B) > {
///   Data(20,0x0C), airsyncbase:ContentType(17,0x17) } }. Regression guard: the old constants read
///   Response as 0x08 (Options), Status as 0x0A (Total), Properties as 0x0F (Version), Data as 0x10
///   (Schema), ContentType as page-20 0x12 (EmptyFolderContents) — so every live response parsed to
///   status 0 / no data.
#[test]
fn item_operations_response_parses_spec_shaped_response() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05, // ItemOperations
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level Status
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E, // Response
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06, // Fetch
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // fetch Status
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B, // Properties
                            vec![
                                WbxmlElement::text(PAGE_ITEM_OPS, 0x0C, "aGVsbG8="), // Data
                                WbxmlElement::text(pages::BASE, 0x17, "text/plain"), /* airsyncbase:ContentType */
                            ],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some(b"aGVsbG8=" as &[u8]));
    assert_eq!(parsed.content_type.as_deref(), Some("text/plain"));
}

/// An item/body fetch returns the payload as Properties >
/// airsyncbase:Body(17,0x0A) > { Type(17,0x06), Data(17,0x0B) } rather
/// than a page-20 Data element. The parser must surface Body.Data as
/// `result.data` and derive content_type from the Body Type when the
/// server omits airsyncbase:ContentType. Live evidence 2026-08-02:
/// Exchange 2019 answers exactly this shape.
#[test]
fn item_operations_response_parses_body_fetch_payload() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "2"), // Type=HTML
                                    WbxmlElement::text(pages::BASE, 0x0B, "<p>hi</p>"), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some(b"<p>hi</p>" as &[u8]));
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
}

/// Task 3: a MIME fetch answer carries Properties > airsyncbase:Body with
/// Type=4 (MIME BLOB, [MS-ASCMD] §4.10.2.2 response example). The parser
/// must surface Body.Data as `result.data` and, when the server omits
/// airsyncbase:ContentType, fall back to `message/rfc822` — mirroring the
/// Type 1/2 → text/plain|html fallbacks.
#[test]
fn item_operations_response_type_4_body_falls_back_to_message_rfc822() {
    let raw_mime = "From: a@b\r\nSubject: opaque s + e\r\n\r\nbody";
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "4"), // Type = MIME
                                    WbxmlElement::text(pages::BASE, 0x0C, "13813"), /* EstimatedDataSize */
                                    WbxmlElement::text(pages::BASE, 0x0B, raw_mime), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some(raw_mime.as_bytes()));
    assert_eq!(
        parsed.content_type.as_deref(),
        Some("message/rfc822"),
        "Type-4 body without an explicit ContentType must read as message/rfc822"
    );
}

/// A fetch-level failure status (e.g. 3 = not found) must override the
/// top-level success Status, matching the Sync parser's "more specific
/// wins" rule.
#[test]
fn item_operations_response_fetch_status_overrides_top_level() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level OK
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "3")], // fetch failed
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 3);
    assert!(parsed.data.is_none());
}

// ---- Task 5 (eas-adapter): ranged/truncated response placement facts ----
//
// Spec anchors:
// - §2.2.3.143.2 Range (response): child of Properties; "the byte-range specified by the server in
//   the response is the authoritative value" — the server's fulfillment is best-effort and may be
//   shorter than asked.
// - §2.2.3.184.2 Total: child of Properties; the item's total size in bytes.
// - [MS-ASAIRS] airsyncbase:Truncated: child of Body; 1 = the body data was truncated — the
//   truncation signal an UNRANGED answer can carry (an unranged response may omit Total entirely).

/// A ranged answer surfaces its placement facts: Properties > Range "m-n" and
/// Properties > Total parse into `range`/`total`, alongside the Body payload.
#[test]
fn ranged_response_parses_authoritative_range_and_total() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B, // Properties
                            vec![
                                WbxmlElement::text(PAGE_ITEM_OPS, 0x09, "120-219"), // Range
                                WbxmlElement::text(PAGE_ITEM_OPS, 0x0A, "1200"),    // Total
                                WbxmlElement::container(
                                    pages::BASE,
                                    0x0A, // airsyncbase:Body
                                    vec![
                                        WbxmlElement::text(pages::BASE, 0x06, "4"),
                                        WbxmlElement::text(pages::BASE, 0x0B, "From: a@b"),
                                    ],
                                ),
                            ],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.range, Some((120, 219)), "the authoritative span");
    assert_eq!(parsed.total, Some(1200), "the whole-item byte size");
    assert_eq!(parsed.data.as_deref(), Some(b"From: a@b" as &[u8]));
    assert_eq!(parsed.content_type.as_deref(), Some("message/rfc822"));
}

/// airsyncbase:Body > Truncated parses: "1" → Some(true), "0" → Some(false).
#[test]
fn truncated_body_flag_is_parsed() {
    let response = |truncated: &str| {
        WbxmlElement::container(
            PAGE_ITEM_OPS,
            0x05,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![WbxmlElement::container(
                        PAGE_ITEM_OPS,
                        0x0B,
                        vec![WbxmlElement::container(
                            pages::BASE,
                            0x0A,
                            vec![
                                WbxmlElement::text(pages::BASE, 0x06, "4"),
                                WbxmlElement::text(pages::BASE, 0x0B, "partial"),
                                WbxmlElement::text(pages::BASE, 0x0D, truncated),
                            ],
                        )],
                    )],
                )],
            )],
        )
    };
    let truncated = parse_item_operations_response(&response("1")).expect("parse");
    assert_eq!(truncated.truncated, Some(true));
    let whole = parse_item_operations_response(&response("0")).expect("parse");
    assert_eq!(whole.truncated, Some(false));
}

/// OPAQUE payload bytes surface byte-exact — never base64-re-encoded — since
/// the reassembly loop places them against byte offsets.
#[test]
fn opaque_data_is_byte_exact() {
    let non_utf8: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0xff, 0x00, 0xd8];
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            0x0E,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x06,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x0B,
                    vec![WbxmlElement::opaque(PAGE_ITEM_OPS, 0x0C, non_utf8.to_vec())],
                )],
            )],
        )],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.data.as_deref(), Some(non_utf8));
}

/// A malformed authoritative Range never flows into a reassembly buffer as
/// positioned data: non-"m-n" text and m > n are `InvalidContent` errors.
#[test]
fn malformed_range_errors() {
    for bad in ["abc", "9-2", "0-", "-5"] {
        let response = WbxmlElement::container(
            PAGE_ITEM_OPS,
            0x05,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![WbxmlElement::container(
                        PAGE_ITEM_OPS,
                        0x0B,
                        vec![WbxmlElement::text(PAGE_ITEM_OPS, 0x09, bad)],
                    )],
                )],
            )],
        );
        let err = parse_item_operations_response(&response).expect_err("malformed Range errors");
        assert!(
            matches!(err, WbxmlError::InvalidContent(_)),
            "Range '{bad}' must be InvalidContent, got {err:?}"
        );
    }
}

/// A non-numeric Total is an `InvalidContent` error — an unreadable total
/// must not be mistaken for an unranged (whole-item) answer.
#[test]
fn non_numeric_total_errors() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            0x0E,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x06,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x0B,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, 0x0A, "lots")],
                )],
            )],
        )],
    );
    let err = parse_item_operations_response(&response).expect_err("bad Total errors");
    assert!(matches!(err, WbxmlError::InvalidContent(_)));
}
