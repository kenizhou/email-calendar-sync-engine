//! Per-part attachment fetch: read the message's `bodyStructure` with one
//! `Email/get`, match the requested part to a body part by its
//! (contentType, name, size) tuple, and download just that part's blob —
//! falling back to the whole-source fetch whenever the mapping is not
//! provably unambiguous.
//!
//! # The part-mapping decision (binding — never guess)
//!
//! The requested [`AttachmentPartId`] is the **mail-parser attachment index**
//! into the raw RFC 5322 source (`engine-mime`'s extractor numbers parts by
//! `Message::attachment(i)`, mail-parser 0.11.4 `parsers/message.rs`). A JMAP
//! `bodyStructure` tree cannot reproduce that index space positionally: the
//! parser's body-vs-attachment classification is stateful
//! (`need_text_body`/`need_html_body`, `in_alternative`, per-container rules,
//! `Content-Type` `name` attributes), so a tree-order walk would be a guess.
//! The mapping is therefore a **tuple match**, and the expected tuple comes
//! from the only engine-side source available on this path:
//!
//! - The raw source is **unavailable by definition here** — a cached source already served the part
//!   through `engine-mime` upstream (engine-host's tiered read, the facade's `message_attachment`);
//!   reaching this fetch means nothing local holds it.
//! - The tuple source is the `Message` row's **stored attachment metadata**
//!   ([`Message::attachments`], the provider-synced normalized list), read **positionally**:
//!   `attachments[part]` is expected to be the part the extractor numbers `part`. That assumption
//!   holds when a sync stored the list in MIME document order over exactly the parser-classified
//!   attachment set; JMAP's Tier-1 sync stores **no** list at all, so for a JMAP-synced row today
//!   this lookup yields nothing and every fetch takes the whole-source fallback — behavior
//!   unchanged from before this seam. `mime_structure` is deliberately NOT a source: indexing the
//!   tree by `AttachmentPartId` would need the same irreproducible classification.
//!
//! The tuple match + duplicate detection contain a violated positional
//! assumption to *same-message* wrong-part bytes at worst (both sides of the
//! match describe this one message); any ambiguity — duplicate tuples, zero
//! matches, a missing `blobId`, a failed `bodyStructure` fetch, a session
//! without `downloadUrl` — falls back to the whole source with the reason
//! debug-logged. Only a matched part's own download failure propagates.

use engine_core::{
    attachment::Attachment,
    ids::AccountId,
    mail::{AttachmentPartId, Message},
};
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    provider::JmapProvider,
    request::{Request, capability},
};

/// One flattened `bodyStructure` part's identity and blob handle (RFC 8621
/// §4.1.4 `EmailBodyPart`).
///
/// The list [`JmapProvider::email_body_structure`] returns holds the tree's
/// **leaf and blob-bearing parts in document order** (attachments, inline
/// parts, and body parts alike — excluding a body part client-side would
/// require a classification guess, and the tuple match's duplicate detection
/// makes including it harmless). `part_id` is empty when the server left it
/// null; the matcher below never reads it (the match is by tuple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPartMeta {
    /// The server-assigned `partId` (empty when null).
    pub part_id: String,
    /// The part's decoded-bytes blob handle; `None` for a part the server
    /// does not serve as a blob (undownloadable here).
    pub blob_id: Option<String>,
    /// The media type (`type`, e.g. `application/pdf`).
    pub content_type: String,
    /// The filename (`name`), if any.
    pub name: Option<String>,
    /// The decoded size in octets (`size`).
    pub size: u64,
}

/// The requested part's expected identity, read off the message row — the
/// tuple the `bodyStructure` match is keyed on.
struct ExpectedPart {
    content_type: String,
    name: Option<String>,
    size: u64,
}

/// The outcome of matching the expected tuple against the flattened
/// `bodyStructure`: the one blob to download, or the reason the per-part
/// path cannot provably serve the request (the caller falls back to the
/// whole source).
enum PartMatch {
    Hit(String),
    Fallback(&'static str),
}

impl JmapProvider {
    /// Fetches one message's `bodyStructure` (`Email/get` with
    /// `properties: ["bodyStructure"]`, RFC 8621 §4.1.4) flattened to the
    /// part list in document order.
    ///
    /// The `account` parameter is interface symmetry with the `Provider`
    /// verbs: the session-bound mail account serves every request, so it is
    /// not consulted.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Protocol`] when the email is absent from the
    /// result (`notFound`) or carries no `bodyStructure` (absent or null),
    /// or a transport / method error from the `Email/get`.
    pub async fn email_body_structure(
        &self,
        _account: &AccountId,
        email_id: &str,
    ) -> Result<Vec<BodyPartMeta>, JmapError> {
        let account = self.mail_account()?;
        let mut request = Request::new([capability::CORE, capability::MAIL]);
        let call = request.invoke(
            "Email/get",
            json!({
                "accountId": account,
                "ids": [email_id],
                "properties": ["bodyStructure"],
            }),
        );
        let response = self.executor.execute(&request).await?;
        let result = response.result(&call)?;
        let not_found = result
            .get("notFound")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(email_id)));
        if not_found {
            return Err(JmapError::protocol(format!("email {email_id:?} not found")));
        }
        let structure = result
            .get("list")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            // `get` alone is not enough: an explicit `"bodyStructure": null`
            // is `Some(Value::Null)`, which would flatten into a phantom
            // zero-valued part instead of the documented error.
            .and_then(|email| email.get("bodyStructure"))
            .filter(|structure| !structure.is_null())
            .ok_or_else(|| {
                JmapError::protocol(format!("email {email_id:?} carried no bodyStructure"))
            })?;
        let mut parts = Vec::new();
        flatten_body_structure(structure, &mut parts);
        Ok(parts)
    }

    /// Downloads one blob by id through the session's `downloadUrl` template
    /// (RFC 8620 §6.2), the same substitution [`crate::blob`] applies to the
    /// whole-source fetch.
    ///
    /// The `account` parameter is interface symmetry with the `Provider`
    /// verbs: the session-bound mail account serves every download, so it is
    /// not consulted.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] when the server advertised no
    /// `downloadUrl`, or a transport/HTTP error from the download.
    pub async fn download_blob(
        &self,
        _account: &AccountId,
        blob_id: &str,
    ) -> Result<Vec<u8>, JmapError> {
        let account = self.mail_account()?;
        let template = self
            .executor
            .session()
            .download_url()
            .ok_or_else(|| JmapError::session("server advertised no downloadUrl"))?;
        // The template's `{type}`/`{name}` placeholders only steer the
        // response's Content-Type/Content-Disposition headers; fixed safe
        // literals keep the URL builder's encoding invariants untouched.
        let url = crate::blob::download_url(
            template,
            &account,
            blob_id,
            "application/octet-stream",
            "attachment",
        );
        self.executor.download(&url).await
    }

    /// Fetches the decoded bytes of `message`'s attachment `part`, per-part
    /// when the part provably resolves to one `bodyStructure` blob, else via
    /// the whole-source fallback. See the module docs for the mapping
    /// decision and the exact fallback conditions.
    ///
    /// # Errors
    ///
    /// Propagates the matched part's download failure, or the whole-source
    /// fallback's own error (its fetch failure, or a permanent "not found"
    /// when the source carries no such part).
    pub async fn fetch_attachment_part(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, JmapError> {
        match self.resolve_part_blob(account, message, part).await {
            // A matched part whose download fails is a REAL failure, not a
            // fallback: the bodyStructure promised these bytes.
            Some(blob_id) => self.download_blob(account, &blob_id).await,
            None => self.whole_source_part(message, part).await,
        }
    }

    /// Resolves the requested part to its `blobId`, or `None` (with the
    /// reason debug-logged) when the mapping is not provably unambiguous —
    /// the never-guess fallback conditions, in check order.
    async fn resolve_part_blob(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Option<String> {
        let Some(expected) = expected_part(message, part) else {
            log::debug!(
                "[jmap] {}: no stored attachment metadata maps part {} — \
                 falling back to the whole-source fetch",
                account.as_str(),
                part.as_u32()
            );
            return None;
        };
        // A session without downloadUrl cannot serve a blob: that is a
        // fallback condition, not an error (the fallback's own fetch is what
        // surfaces one if it equally cannot run).
        if self.executor.session().download_url().is_none() {
            log::debug!(
                "[jmap] {}: the session advertised no downloadUrl — \
                 falling back to the whole-source fetch",
                account.as_str()
            );
            return None;
        }
        let parts = match self
            .email_body_structure(account, message.id.as_str())
            .await
        {
            Ok(parts) => parts,
            Err(err) => {
                log::debug!(
                    "[jmap] {}: the bodyStructure fetch failed ({err}) — \
                     falling back to the whole-source fetch",
                    account.as_str()
                );
                return None;
            }
        };
        match match_part(&parts, &expected) {
            PartMatch::Hit(blob_id) => Some(blob_id),
            PartMatch::Fallback(reason) => {
                log::debug!(
                    "[jmap] {}: {reason} — falling back to the whole-source fetch",
                    account.as_str()
                );
                None
            }
        }
    }

    /// The whole-source fallback: download the raw RFC 5322 source and
    /// extract the requested part with the same `engine-mime` extractor the
    /// facade's own attachment read uses, so both surfaces agree on what a
    /// part is.
    async fn whole_source_part(
        &self,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, JmapError> {
        let raw = crate::blob::message_source(self.executor.as_ref(), message).await?;
        let content = engine_mime::extract_attachment(&raw, part).ok_or_else(|| {
            JmapError::protocol(format!(
                "attachment part {} not found in the source of {}",
                part.as_u32(),
                message.id.key().as_str()
            ))
        })?;
        Ok(content.into_bytes())
    }
}

/// The expected tuple for the requested part, from the message row's stored
/// attachment metadata — see the module docs for why this list, read
/// positionally, is the only sound tuple source on this path.
///
/// Returns `None` when the row carries nothing mappable at this index (no
/// list, out of range, a reference attachment with no bytes, or incomplete
/// metadata) — the caller falls back rather than guesses.
fn expected_part(message: &Message, part: AttachmentPartId) -> Option<ExpectedPart> {
    let stored = message.attachments.get(part.as_u32() as usize)?;
    if matches!(stored, Attachment::Reference { .. }) {
        return None;
    }
    let meta = stored.meta();
    Some(ExpectedPart {
        content_type: meta.media_type.clone()?,
        name: meta.name.clone(),
        size: meta.size?,
    })
}

/// Matches the expected tuple against the flattened `bodyStructure`:
/// case-insensitive on the media type (MIME types are case-insensitive),
/// exact on the name and decoded size. Zero or duplicate matches and a
/// matched part without a `blobId` are all fallback conditions — never a
/// guess.
fn match_part(parts: &[BodyPartMeta], expected: &ExpectedPart) -> PartMatch {
    let mut hits = parts.iter().filter(|part| {
        part.content_type
            .eq_ignore_ascii_case(&expected.content_type)
            && part.name == expected.name
            && part.size == expected.size
    });
    let Some(first) = hits.next() else {
        return PartMatch::Fallback("no bodyStructure part carries the stored attachment tuple");
    };
    if hits.next().is_some() {
        return PartMatch::Fallback(
            "the stored attachment tuple matches more than one bodyStructure part",
        );
    }
    match &first.blob_id {
        Some(blob_id) => PartMatch::Hit(blob_id.clone()),
        None => PartMatch::Fallback("the matched bodyStructure part carries no blobId"),
    }
}

/// Flattens an `EmailBodyPart` tree into `out` in document (depth-first
/// pre-order) order.
///
/// A node with sub-parts and no `blobId` is a pure container
/// (`multipart/*`): it contributes no entry of its own, only its children. A
/// node WITH a `blobId` is downloadable as one unit — including
/// `message/rfc822`, whose inner structure mail-parser counts as ONE
/// attachment, so its sub-parts are not flattened in — and a leaf without
/// either is still collected (its missing `blobId` is the matcher's
/// fallback condition, not a reason to drop it here).
fn flatten_body_structure(value: &Value, out: &mut Vec<BodyPartMeta>) {
    let sub_parts = value.get("subParts").and_then(Value::as_array);
    let blob_id = value.get("blobId").and_then(Value::as_str);
    if blob_id.is_none()
        && let Some(children) = sub_parts
        && !children.is_empty()
    {
        for child in children {
            flatten_body_structure(child, out);
        }
        return;
    }
    out.push(BodyPartMeta {
        part_id: value
            .get("partId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        blob_id: blob_id.map(str::to_owned),
        content_type: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: value.get("name").and_then(Value::as_str).map(str::to_owned),
        size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
    });
}
