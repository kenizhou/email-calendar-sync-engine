// SPDX-License-Identifier: MPL-2.0
// ItemOperations Fetch ([MS-ASCMD] §4.10): attachments, search results, bodies.

use crate::commands::{
    AS_COLLECTION_ID, AS_MIME_SUPPORT, AS_SERVER_ID, ItemOperationsFetchRequest,
    ItemOperationsFetchResult, PAGE_AIRSYNC, PAGE_ITEM_OPS, WbxmlElement, WbxmlError, WbxmlValue,
    pages, tags, text_value, text_value_opt,
};

// ============================================================================
// ItemOperations (fetch attachments / items)
// ============================================================================

/// Formats a byte range as the wire `"m-n"` string ([MS-ASCMD] §2.2.3.143.2:
/// zero-indexed, inclusive).
pub(crate) fn range_wire_text(range: (u64, u64)) -> String {
    format!("{}-{}", range.0, range.1)
}

/// Parses the response's `Properties>Range` text (`"m-n"`, zero-indexed
/// inclusive). Strict — a malformed span errors rather than being silently
/// dropped, because a ranged answer whose placement cannot be read must never
/// flow into a reassembly buffer as if it were positioned.
fn parse_range_text(text: &str) -> Result<Option<(u64, u64)>, WbxmlError> {
    let Some((m, n)) = text.split_once('-') else {
        return Err(WbxmlError::InvalidContent(format!(
            "ItemOperations Range '{text}' is not m-n"
        )));
    };
    let (Ok(m), Ok(n)) = (m.trim().parse::<u64>(), n.trim().parse::<u64>()) else {
        return Err(WbxmlError::InvalidContent(format!(
            "ItemOperations Range '{text}' has non-numeric bounds"
        )));
    };
    if m > n {
        return Err(WbxmlError::InvalidContent(format!(
            "ItemOperations Range '{text}' has m > n"
        )));
    }
    Ok(Some((m, n)))
}

/// Build an ItemOperations Fetch request.
///
/// Wire shape per [MS-ASCMD] §6.23 (request schema) and [MS-ASWBXML]
/// §2.1.2.1.21 (page 20):
/// ```xml
/// <ItemOperations>                                   <!-- page 20, 0x05 -->
///   <Fetch>                                          <!-- page 20, 0x06 -->
///     <Store>Mailbox</Store>                         <!-- page 20, 0x07 -->
///     <airsyncbase:FileReference>{ref}</>            <!-- page 17, 0x11 (attachment) -->
///     — OR —
///     <airsync:CollectionId>{collection_id}</>       <!-- page 0, 0x12 (item fetch) -->
///     <airsync:ServerId>{server_id}</>               <!-- page 0, 0x0D -->
///     <Options>                                      <!-- page 20, 0x08 -->
///       <airsyncbase:BodyPreference>                 <!-- page 17, 0x05 -->
///         <airsyncbase:Type>2</>                     <!-- page 17, 0x06 (HTML) -->
///       </>
///     </Options>
///   </Fetch>
/// </ItemOperations>
/// ```
/// CollectionId/ServerId/FileReference are NOT page-20 tokens — the request
/// schema (§6.23) imports them from the AirSync / AirSyncBase namespaces.
/// The BodyPreference on item fetches is required in practice: without it
/// Exchange 2019 answers with the airsyncbase:Body *metadata* only (Type +
/// EstimatedDataSize + Truncated=1, no Data child) — live evidence:
/// eas_io_debug raw dump, 2026-08-02.
///
/// Three fetch forms, in precedence order (first populated wins):
/// 1. **Attachment fetch** (`file_reference`): Store + airsyncbase:FileReference [+ Options>Range
///    when `range` is set — §2.2.3.125.3: with FileReference present, Range is the ONLY valid
///    Options child].
/// 2. **Search-result fetch** (`long_id`, [MS-ASCMD] §4.10.3.3): Store + search:LongId (page 15,
///    0x18) + Options>BodyPreference(Type 2). The search:LongId replaces CollectionId/ServerId
///    (§2.2.3.98.1: they MUST NOT accompany it — that rule is stated for MeetingResponse/Source but
///    the Fetch schema is a `xs:choice` with the same exclusivity).
/// 3. **Body/item fetch** (`collection_id` + `server_id`): Store + airsync:CollectionId +
///    airsync:ServerId + Options>BodyPreference(Type 2). With `mime: true` the Options carry
///    `airsync:MIMESupport`=2 ahead of the BodyPreference and its Type switches to 4 — the
///    MIME-fetch shape of [MS-ASCMD] §4.10.2.1. With `range` set the Options also carry `Range`
///    ("m-n") FIRST — the §2.2.3.125.3 child order (Schema, Range, …, MIMESupport, BodyPreference).
pub fn build_item_operations_request(req: &ItemOperationsFetchRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut fetch_children = vec![WbxmlElement::text(
        PAGE_ITEM_OPS,
        io::STORE,
        "Mailbox".to_string(),
    )];

    if let Some(file_ref) = &req.file_reference {
        fetch_children.push(WbxmlElement::text(
            pages::BASE,
            tags::base::FILE_REFERENCE,
            file_ref.clone(),
        ));
        // §2.2.3.125.3: with FileReference present, Range is the only valid
        // Options child — emit Options only when a range is requested.
        if let Some(range) = req.range {
            fetch_children.push(WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::OPTIONS,
                vec![WbxmlElement::text(
                    PAGE_ITEM_OPS,
                    io::RANGE,
                    range_wire_text(range),
                )],
            ));
        }
    } else if let Some(long_id) = &req.long_id {
        fetch_children.push(WbxmlElement::text(
            tags::search::PAGE,
            tags::search::LONG_ID,
            long_id.clone(),
        ));
        // Same HTML-body requirement as the collection/server-id form below.
        let body_preference = WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, "2")],
        );
        fetch_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![body_preference],
        ));
    } else {
        fetch_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_COLLECTION_ID,
            req.collection_id.clone(),
        ));
        fetch_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_SERVER_ID,
            req.server_id.clone(),
        ));
        // Item fetch: ask for the body explicitly. Without a BodyPreference
        // the server returns Body metadata (Type / EstimatedDataSize /
        // Truncated) but no Data child.
        //
        // Options children per [MS-ASCMD] §4.10.2.1 (MIME fetch example):
        // `airsync:MIMESupport` (page 0, 0x22) BEFORE
        // `airsyncbase:BodyPreference`. MIME fetches switch the BodyPreference
        // Type to 4 (MIME BLOB); the default stays Type 2 (HTML). A requested
        // `Range` goes first (§2.2.3.125.3 child order: Schema, Range, …,
        // MIMESupport, BodyPreference).
        let mut options_children: Vec<WbxmlElement> = Vec::new();
        if let Some(range) = req.range {
            options_children.push(WbxmlElement::text(
                PAGE_ITEM_OPS,
                io::RANGE,
                range_wire_text(range),
            ));
        }
        let body_type = if req.mime {
            // Level 2 = "send MIME data for all messages" (§2.2.3.110.3).
            // The spec example uses 1 (S/MIME messages only), but a MIME
            // fetch here means the caller wants the raw message for THIS
            // item regardless of its S/MIME status — level 2 is the
            // conservative superset that never withholds the MIME BLOB.
            options_children.push(WbxmlElement::text(
                PAGE_AIRSYNC,
                AS_MIME_SUPPORT,
                "2".to_string(),
            ));
            "4"
        } else {
            "2"
        };
        options_children.push(WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, body_type)],
        ));
        fetch_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            options_children,
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::FETCH,
            fetch_children,
        )],
    )
}

/// Parse an ItemOperations Fetch response.
///
/// Wire shape per [MS-ASCMD] §6.24 (response schema):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <Fetch>                           <!-- page 20, 0x06 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — per-op level -->
///       <Properties>                    <!-- page 20, 0x0B -->
///         <Data>{base64}</Data>         <!-- page 20, 0x0C -->
///         <Range>m-n</Range>            <!-- page 20, 0x09 — authoritative span -->
///         <Total>1200</Total>           <!-- page 20, 0x0A — whole-item size -->
///         <airsyncbase:ContentType>../> <!-- page 17, 0x17 (§2.2.3.139.2) -->
///       </Properties>
///     </Fetch>
///   </Response>
/// </ItemOperations>
/// ```
/// The fetch-level Status overrides the top-level one (more specific wins),
/// mirroring the Sync parser's collection-status rule.
///
/// `Properties>Range`/`Properties>Total` ([MS-ASCMD] §2.2.3.143.2 /
/// §2.2.3.184.2) are the ranged-fetch placement facts: the response Range is
/// the span the Data ACTUALLY covers (the server's range fulfillment is
/// best-effort), Total the whole item's byte size. `Range` must parse as
/// `"m-n"` with m ≤ n and `Total` as an integer — a malformed span is an
/// `InvalidContent` error, never a silent drop (unknowingly-positioned bytes
/// must not reach a reassembly buffer). `airsyncbase:Body>Truncated`
/// ([MS-ASAIRS]) surfaces as `truncated`.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, a non-numeric `Total`, or a
/// `Range` that is not a parseable `"m-n"` span.
pub fn parse_item_operations_response(
    root: &WbxmlElement,
) -> Result<ItemOperationsFetchResult, WbxmlError> {
    use tags::item_operations as io;

    let mut result = ItemOperationsFetchResult::default();
    // Top-level Status first (command-level rejection, e.g. 143 device not
    // provisioned); a fetch-level Status below overrides it.
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS
            && child.token == io::STATUS
            && let Ok(s) = text_value(child)
            && let Ok(n) = s.parse::<u8>()
        {
            result.status = n;
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::FETCH {
                    for fetch_child in &resp_child.children {
                        match (fetch_child.page, fetch_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                if let Ok(s) = text_value(fetch_child)
                                    && let Ok(n) = s.parse::<u8>()
                                {
                                    result.status = n;
                                }
                            }
                            (PAGE_ITEM_OPS, io::PROPERTIES) => {
                                for prop in &fetch_child.children {
                                    match (prop.page, prop.token) {
                                        (PAGE_ITEM_OPS, io::DATA) => {
                                            result.data = match &prop.value {
                                                WbxmlValue::Text(t) => Some(t.clone().into_bytes()),
                                                WbxmlValue::Opaque(b) => Some(b.clone()),
                                                WbxmlValue::Empty => None,
                                            };
                                        }
                                        // Properties>Range (§2.2.3.143.2): the
                                        // authoritative span of the returned
                                        // Data — strict, see the module docs.
                                        (PAGE_ITEM_OPS, io::RANGE) => {
                                            if let Ok(text) = text_value(prop) {
                                                result.range = parse_range_text(&text)?;
                                            }
                                        }
                                        // Properties>Total (§2.2.3.184.2):
                                        // the whole item's byte size. Strict —
                                        // an unreadable total must not be
                                        // mistaken for an unranged answer.
                                        (PAGE_ITEM_OPS, io::TOTAL) => {
                                            if let Ok(text) = text_value(prop) {
                                                result.total = Some(
                                                    text.trim().parse::<u64>().map_err(|_| {
                                                        WbxmlError::InvalidContent(format!(
                                                            "ItemOperations Total '{text}' is not an integer"
                                                        ))
                                                    })?,
                                                );
                                            }
                                        }
                                        // airsyncbase:ContentType (page 17, 0x17)
                                        (pages::BASE, tags::base::CONTENT_TYPE) => {
                                            result.content_type = match &prop.value {
                                                WbxmlValue::Text(t) => Some(t.clone()),
                                                _ => None,
                                            };
                                        }
                                        // airsyncbase:Body (page 17, 0x0A) —
                                        // the payload of an item/body fetch.
                                        // Its Data child (page 17, 0x0B)
                                        // carries the body bytes; Type tells us
                                        // whether it is HTML (2) or plain (1),
                                        // which we surface as content_type
                                        // when the server didn't send one.
                                        // Truncated (page 17, 0x0D) is the
                                        // body-truncation flag.
                                        (pages::BASE, tags::base::BODY) => {
                                            let mut body_type: Option<u8> = None;
                                            for b in &prop.children {
                                                match (b.page, b.token) {
                                                    (pages::BASE, tags::base::TYPE) => {
                                                        body_type = text_value_opt(b)
                                                            .and_then(|s| s.parse().ok());
                                                    }
                                                    (pages::BASE, tags::base::DATA) => {
                                                        result.data = match &b.value {
                                                            WbxmlValue::Text(t) => {
                                                                Some(t.clone().into_bytes())
                                                            }
                                                            WbxmlValue::Opaque(bytes) => {
                                                                Some(bytes.clone())
                                                            }
                                                            WbxmlValue::Empty => None,
                                                        };
                                                    }
                                                    (pages::BASE, tags::base::TRUNCATED) => {
                                                        result.truncated = match &b.value {
                                                            WbxmlValue::Text(t) => match t.trim() {
                                                                "1" | "true" => Some(true),
                                                                _ => Some(false),
                                                            },
                                                            _ => None,
                                                        };
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            if result.content_type.is_none() {
                                                result.content_type = match body_type {
                                                    Some(2) => Some("text/html".to_string()),
                                                    Some(1) => Some("text/plain".to_string()),
                                                    // Type 4 = MIME BLOB
                                                    // ([MS-ASCMD] §2.2.3.110.3):
                                                    // the payload is a raw
                                                    // RFC 5322 message.
                                                    Some(4) => Some("message/rfc822".to_string()),
                                                    _ => None,
                                                };
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Decodes the ItemOperations Fetch status table ([MS-ASCMD] §2.2.3.177.8)
/// into a human message — the family's `*_status_message` helper (the
/// `move_items_status_message` precedent).
#[must_use]
pub fn item_operations_status_message(status: u32) -> &'static str {
    match status {
        1 => "Success",
        2 => "Protocol error — protocol violation or XML validation error",
        3 => "Server error",
        6 => "The object was not found or access was denied",
        8 => "The byte range is invalid or too large",
        9 => "The store is unknown or unsupported",
        10 => "The file is empty",
        11 => "The requested data size is too large",
        12 => "Failed to download the file because of an I/O failure",
        14 => "The item failed conversion",
        15 => "The attachment or attachment ID is invalid",
        16 => "Access to the resource is denied",
        17 => "Partial success — the operation completed partially",
        18 => "Credentials are required",
        _ => "unknown ItemOperations status",
    }
}
