// SPDX-License-Identifier: MPL-2.0
//! The submission verbs: `submit_email` (a `Draft` assembled to RFC 5322
//! through `engine-rfc5322` — the Graph adapter's seam) and
//! `submit_email_source` (caller-rendered bytes sent verbatim). Both ride
//! the one wire form, EAS `SendMail` ([MS-ASCMD] §2.2.1.13): the raw message
// ! bytes as an OPAQUE `<Mime>`, `<SaveInSentItems/>` asking the server to
//! file the Sent copy, and a `<ClientId>` derived deterministically from the
//! `Message-ID`.
//!
//! ## Mapping
//!
//! **The filed assembly variant.** `SendMail` routes recipients from the
//! bytes' own headers — there is no separate envelope — so the `Bcc` header
//! must stay IN the bytes for the blind copies to be delivered
//! (`assemble_filed_message`, the same reasoning as Graph's `sendMail`; no
//! recipient but the server ever sees it).
//!
//! **Empty body is the only success.** SendMail answers HTTP 200 with no
//! body; a body can only carry an in-body `<Status>` failure, which
//! `EasClient::send_mail` surfaces as `Ok(status)` — the adapter converts
//! every non-1 through the SendMail family classifier
//! ([`compose_status_error`](super::error::compose_status_error)).
//!
//! **No sent id comes back.** The receipt's key is the Graph/IMAP
//! `sent:<Message-ID>` placeholder — the sent copy reconciles by
//! `Message-ID` when Sent Items next syncs (the Write Contract).
//!
//! **The ClientId is the idempotency story.** `[MS-ASCMD]` caps it at 40
//! chars (Exchange 15.2 enforces it with in-body Status 103 — task-11 live
//! evidence) and dedups a lost-response retry by it (§2.2.3.28.1), so the
//! adapter derives it from the send's `Message-ID` (`SM` + 16 hex of an
//! FNV-1a-64 — the Kylins client's live-proven derivation): the same send
//! retried derives the same id and cannot double-deliver.
//!
//! ## The source seam's envelope rule
//!
//! `submit_email_source`'s `recipients` is an SMTP-shaped envelope, but
//! SendMail delivers exactly the bytes' header set. An empty list derives
//! the envelope from the bytes (`To`/`Cc`, plus a `Bcc` header left in — it
//! travels visibly, per the trait contract). A non-empty list is honored
//! only when it names EXACTLY the bytes' own recipient addr-specs — a list
//! the bytes cannot deliver (an extra recipient, or a stripped `Bcc` the
//! caller listed) is refused permanently BEFORE the wire rather than
//! silently mis-delivered. The comparison is deliberately conservative: a
//! header shape the adapter cannot split confidently (a quoted display name
//! may hide a comma) counts as a mismatch — a false refusal is safe, a
//! false delivery is not.

use std::collections::BTreeSet;

use engine_core::ids::{MessageIdHeader, ProviderKey};
use engine_provider::{Draft, ProviderError, ProviderResult, SubmissionReceipt};
use engine_rfc5322::{assemble_filed_message, header_values, parse_message_id};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::error::{compose_status_error, provider_error};
use crate::{
    client::EasClient,
    types::{SendMailRequest, new_send_client_id},
};

/// Assembles the draft (the filed variant — see the module docs) and sends
/// it. The receipt echoes the draft's pre-generated `Message-ID`, the
/// reconcile key for the sent copy.
///
/// # Errors
///
/// An assembly failure (a header value carrying CR/LF/NUL) is permanent;
/// delivery failures classify per [`compose_status_error`].
pub(super) async fn submit(
    client: &Mutex<EasClient>,
    draft: &Draft,
) -> ProviderResult<SubmissionReceipt> {
    let mime = assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    send(
        client,
        mime,
        send_client_id(draft.message_id.as_str()),
        draft.message_id.clone(),
    )
    .await
}

/// Sends caller-rendered bytes verbatim — never re-rendered (the bytes may
/// already be signed or encrypted). Validates the seam's shape contract
/// first: a `Message-ID` (nothing to reconcile the sent copy by), a `From`
/// header, a terminated body, and — when `recipients` is non-empty — the
/// exact-envelope rule of the module docs.
///
/// # Errors
///
/// [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent)
/// for bytes the seam cannot send; delivery failures classify per
/// [`compose_status_error`].
pub(super) async fn submit_source(
    client: &Mutex<EasClient>,
    source: &[u8],
    recipients: &[String],
) -> ProviderResult<SubmissionReceipt> {
    let Some(message_id) = parse_message_id(source) else {
        return Err(ProviderError::permanent(
            "the submitted bytes carry no Message-ID; the caller must stamp one \
             before submitting (the sent copy reconciles by it)",
        ));
    };
    if header_values(source, "From").iter().all(String::is_empty) {
        return Err(ProviderError::permanent(
            "the submitted bytes carry no From header",
        ));
    }
    if !source.ends_with(b"\n") {
        return Err(ProviderError::permanent(
            "the submitted bytes lack a trailing line terminator",
        ));
    }
    if !recipients.is_empty() && header_addr_specs(source) != Some(addr_spec_set(recipients)) {
        return Err(ProviderError::permanent(
            "SendMail delivers exactly the bytes' own To/Cc/Bcc headers; the \
             recipients list names a different envelope — send the recipients in \
             the bytes or submit with an empty list to derive from them",
        ));
    }
    send(
        client,
        source.to_vec(),
        send_client_id(message_id.as_str()),
        message_id,
    )
    .await
}

/// The one send path both verbs share: one SendMail with
/// `SaveInSentItems`, the in-body status gate, and the filed placeholder
/// receipt.
async fn send(
    client: &Mutex<EasClient>,
    mime: Vec<u8>,
    client_id: String,
    message_id: MessageIdHeader,
) -> ProviderResult<SubmissionReceipt> {
    let mut client = client.lock().await;
    let status = client
        .send_mail(&SendMailRequest {
            mime,
            save_to_sent: true,
            client_id: Some(client_id),
        })
        .await
        .map_err(provider_error)?;
    if status != 1 {
        return Err(compose_status_error(status));
    }
    Ok(SubmissionReceipt::filed(
        placeholder_key(&message_id),
        message_id,
    ))
}

/// The sent copy's placeholder key — `sent:<Message-ID>`, mirroring Graph's
/// no-id sendMail and IMAP's no-UIDPLUS APPEND: SendMail returns no server
/// id, so this stands in until the sent message syncs back from Sent Items
/// and reconciles by `Message-ID`.
fn placeholder_key(message_id: &MessageIdHeader) -> ProviderKey {
    ProviderKey::new(format!("sent:{}", message_id.as_str()))
        .expect("a Message-ID-derived placement key is never empty")
}

/// FNV-1a 64 ([the reference
/// vector](https://datatracker.ietf.org/doc/html/draft-eastlake-fnv)): the
/// 10-line dependency-free digest the Kylins ClientId derivation is
/// live-proven on.
fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The SendMail `<ClientId>`: `SM` + 16 hex of the FNV-1a-64 of the
/// normalized `Message-ID` (lowercased, brackets and whitespace stripped) —
/// 18 chars, comfortably under the enforced 40-char cap, and DETERMINISTIC
/// per message so a lost-response retry derives the same id and Exchange's
/// dedup absorbs it. The normalize step mirrors the Kylins helper a long
/// folded id must survive (folds inject CRLF+SP into the value).
fn send_client_id(message_id: &str) -> String {
    let normalized: String = message_id
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '<' && *c != '>')
        .flat_map(char::to_lowercase)
        .collect();
    if normalized.is_empty() {
        return new_send_client_id("SM");
    }
    format!("SM{:016x}", fnv1a64(normalized.as_bytes()))
}

/// The addr-spec set a recipients list names.
fn addr_spec_set(recipients: &[String]) -> BTreeSet<String> {
    recipients.iter().map(|r| addr_spec_of(r.trim())).collect()
}

/// One recipient token → its addr-spec: a bracketed form keeps only the
/// `local@domain` inside the angle brackets; a bare token is its own
/// addr-spec.
fn addr_spec_of(token: &str) -> String {
    match (token.find('<'), token.rfind('>')) {
        (Some(open), Some(close)) if open < close => token[open + 1..close].trim().to_owned(),
        _ => token.trim().to_owned(),
    }
}

/// The addr-spec set the bytes' own recipient headers (`To`/`Cc`/`Bcc`)
/// deliver to — `None` when a header cannot be split conservatively, which
/// callers treat as a mismatch (the module docs' safe-direction rule).
fn header_addr_specs(source: &[u8]) -> Option<BTreeSet<String>> {
    let mut set = BTreeSet::new();
    for name in ["To", "Cc", "Bcc"] {
        for value in header_values(source, name) {
            if value.contains('"') {
                // A quoted display name may hide a comma; refusing to split
                // over-counts (safe) where mis-splitting under-counts (not).
                return None;
            }
            for token in value.split(',') {
                let spec = addr_spec_of(token.trim());
                if !spec.is_empty() {
                    set.insert(spec);
                }
            }
        }
    }
    Some(set)
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
