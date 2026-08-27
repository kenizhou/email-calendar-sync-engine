// SPDX-License-Identifier: MPL-2.0
//! The `fetch_message_source` verb: EAS `ItemOperations` Fetch mapped onto
//! the engine's single-primitive `RawMime` contract (the spike's §3.6
//! verdict — one call delivers the whole message, headers + every part).
//!
//! ## Addressing
//!
//! **The bound folder is the `CollectionId`; the `MessageId` is the
//! `ServerId`** — the same identity mapping T4 pinned on the Sync wire
//! (FolderSync ServerIds map verbatim into `MailboxId`s, Sync item
//! ServerIds verbatim into `MessageIds`), so the fetch addresses
//! `(folder.as_str(), message.id.as_str())` with no mapping table, the
//! IMAP `(mailbox, UIDVALIDITY, UID)` / Graph blob-handle precedent. A
//! message that moved or vanished answers a per-item status, which
//! classifies (below) rather than misaddressing.
//!
//! ## The request shape
//!
//! One `ItemOperations` Fetch with `airsync:MIMESupport`=2 +
//! `airsyncbase:BodyPreference` Type 4 ([MS-ASCMD] §4.10.2.1) — raw MIME for
//! every message. The multipart opt-in (`MS-ASAcceptMultiPart: T`,
//! §2.2.1.10.1) is set: a Tier-3 whole-message fetch is exactly the large
//! payload the multipart envelope exists for, and a server MAY still answer
//! plain WBXML — both shapes resolve identically.
//!
//! ## Range reassembly
//!
//! A server MAY truncate the answer (the body `Truncated` flag, or a
//! `Total` larger than the payload delivered). The loop then re-fetches the
//! remainder as `Options>Range` rounds (`"m-n"`, zero-indexed inclusive,
//! §2.2.3.143.2): each round asks from the assembled length (capped at
//! `Total-1`), and each round's bytes are placed by the **response's**
//! `Properties>Range` — authoritative, because the server's range
//! fulfillment is best-effort and may be shorter than asked. Three guards
//! keep the assembly honest: a non-contiguous authoritative start (a gap)
//! is `Permanent` — never silent misassembly; a round that returns nothing
//! while the item is still incomplete is a stall (`Permanent`, the drain
//! loop's no-progress rule); a `Total` smaller than the bytes already
//! assembled is a server inconsistency (`Permanent`).
//!
//! **Spec disclosure:** [MS-ASCMD] §2.2.3.125.3 documents byte ranges for
//! attachments and document-library items and not for PIM item fetches —
//! for mail the protocol's large-payload answer is the multipart envelope
//! this verb also opts into. The ranged continuation is nonetheless the
//! reassembly path here (the response `Range`/`Total` elements exist in the
//! Fetch schema for any fetch), and a server that refuses it answers a
//! per-item status (2/8), which surfaces as a classified `Permanent` error
//! — bounded, disclosed behavior rather than an assumption.

use engine_core::{error::FailureClass, ids::MailboxId, mail::Message, raw::RawMime};
use engine_provider::{ProviderError, ProviderResult};
use tokio::sync::Mutex;

use super::error::provider_error;
use crate::{
    client::EasClient, commands::item_operations_status_message, types::ItemOperationsFetchRequest,
};

/// The per-round byte span a ranged continuation asks for — the adapter's
/// bounded-memory policy, not a protocol limit (the server's fulfillment is
/// best-effort and may be shorter; the multipart envelope carries what does
/// arrive out of the WBXML stream).
const RANGE_CHUNK_BYTES: u64 = 512 * 1024;

/// Fetches the raw RFC 5322 source of `message` from the bound folder —
/// see the module docs for the addressing and reassembly contract.
pub(super) async fn fetch_source(
    client: &Mutex<EasClient>,
    folder: &MailboxId,
    message: &Message,
) -> ProviderResult<RawMime> {
    let mut client = client.lock().await;
    let mut assembled: Vec<u8> = Vec::new();
    let mut total: Option<u64> = None;
    let mut ranged = false;
    loop {
        let result = client
            .item_operations(&ItemOperationsFetchRequest {
                collection_id: folder.as_str().to_owned(),
                server_id: message.id.as_str().to_owned(),
                file_reference: None,
                long_id: None,
                mime: true,
                accept_multipart: true,
                range: ranged.then(|| next_range(assembled.len() as u64, total)),
            })
            .await
            .map_err(provider_error)?;
        if result.status != 1 {
            return Err(fetch_status_error(result.status));
        }
        let chunk = result.data.ok_or_else(|| {
            ProviderError::permanent("ItemOperations fetch succeeded with no payload")
        })?;
        // Placement: the response's authoritative Range when present, else
        // the assembled prefix (an unranged answer covers a prefix).
        let start = result.range.map_or_else(|| assembled.len() as u64, |r| r.0);
        if start != assembled.len() as u64 {
            return Err(ProviderError::permanent(format!(
                "non-contiguous fetch: {} bytes assembled but the server's authoritative range starts at {start}",
                assembled.len()
            )));
        }
        if ranged && chunk.is_empty() {
            return Err(ProviderError::permanent(format!(
                "ItemOperations fetch stalled: no bytes for the range starting at {} while the item is incomplete",
                assembled.len()
            )));
        }
        // The most recent Total wins; once known it must never fall below
        // what has arrived.
        total = result.total.or(total);
        assembled.extend_from_slice(&chunk);
        if let Some(t) = total
            && t < assembled.len() as u64
        {
            return Err(ProviderError::permanent(format!(
                "ItemOperations fetch inconsistency: Total {t} is smaller than the {} bytes delivered",
                assembled.len()
            )));
        }
        if !is_truncated(result.truncated, total, assembled.len()) {
            return Ok(RawMime::new(assembled));
        }
        ranged = true;
    }
}

/// The truncation signal for a completed round: the server's `Truncated`
/// flag (the signal an unranged answer can carry — `Total` is optional
/// there), or a `Total` larger than the bytes received.
fn is_truncated(truncated: Option<bool>, total: Option<u64>, received: usize) -> bool {
    truncated == Some(true) || total.is_some_and(|t| t > received as u64)
}

/// The `Options>Range` span for the next continuation round: from the
/// assembled length, spanning a chunk, capped at `Total-1` when the whole
/// size is known. The `m ≤ n` invariant holds by construction (`.max`)
/// even for a degenerate `assembled == total` call the loop never makes.
fn next_range(assembled: u64, total: Option<u64>) -> (u64, u64) {
    let end = assembled
        .saturating_add(RANGE_CHUNK_BYTES - 1)
        .min(total.map_or(u64::MAX, |t| t.saturating_sub(1)))
        .max(assembled);
    (assembled, end)
}

/// The surfaced error for a non-success per-item fetch status — through the
/// ItemOperations status table ([MS-ASCMD] §2.2.3.177.8), classified for
/// the fetch verb's semantics:
///
/// * **3** (server error) and **17** (partial success on a single-op request — an ambiguous
///   outcome) retry: the fetch is idempotent.
/// * **6** ("the object was not found or access was denied") is the stale-target class the trait
///   names for this verb — EAS moves rotate ServerIds, so a pre-move id IS stale; `Conflict` routes
///   the caller to re-sync then retry.
/// * **18** (credentials required) re-authenticates.
/// * **142/143** (provisioning) escaped the transport's own one retry — still retry-shaped (the
///   `http_class` 449 precedent).
/// * Everything else (2 protocol error, 8 invalid range, 11 too large, 14 conversion, 15
///   attachment, 16 access denied, unknown) is permanent: a resend unchanged repeats it.
fn fetch_status_error(status: u8) -> ProviderError {
    let detail = format!(
        "ItemOperations fetch failed: {} (status {status})",
        item_operations_status_message(u32::from(status)),
    );
    match status {
        3 | 17 | 142 | 143 => ProviderError::retryable(detail),
        6 => ProviderError::new(FailureClass::Conflict, detail),
        18 => ProviderError::new(FailureClass::Authentication, detail),
        _ => ProviderError::permanent(detail),
    }
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
