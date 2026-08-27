// SPDX-License-Identifier: MPL-2.0
//! The `stream_email` verb: EAS `Sync` class "Email" mapped onto the engine's
//! [`EmailChunk`] stream (the spike's §3.4 verdict — "no gap", with the
//! Reconcile semantics matching SyncKey invalidation cleanly).
//!
//! ## Mapping
//!
//! **The collection SyncKey is the cursor.** `cursor: None` (or an empty
//! string) bootstraps from `"0"`; the wire's `MoreAvailable` pages a pass
//! round by round, and because **every round rotates the key**, every
//! completed round is a safe resume point — per-round chunks are
//! [`PassMode::Additive`] with `advance_to` = that round's rotated key (the
//! resumability edge EAS holds over JMAP/Graph, whose atomic HTTP pages must
//! hold the cursor mid-pass). A round wider than `chunk_size` splits into
//! sub-chunks that **hold** the cursor, its completing chunk carrying the
//! round's key: committing a later round's key before its rows would lose
//! them on a crash, and the pre-round key is always a valid resume point
//! (the server re-delivers the page).
//!
//! **SyncKey invalidation recovers inside the stream.** A collection status
//! the Sync family's classifier marks resync-shaped — 3, "MUST return to
//! SyncKey value of 0" ([MS-ASCMD] Status (Sync)); 12, "folder hierarchy
//! has changed" (degraded to the same collection reset per `status.rs`'s
//! MVP note) — restarts the pass **once** from `"0"` as a
//! [`PassMode::Reconcile`] pass: the full re-enumeration's pages accumulate
//! the `present` set (the JMAP `cannotCalculateChanges`→snapshot recovery
//! precedent, via `split_page`), and a final marker chunk advances the
//! cursor and tombstones local rows absent from it. The restart happens
//! only before the pass's first chunk — a mid-stream mode change would
//! break the constant-mode contract — so a mid-pass invalidation surfaces
//! as `NeedsResync` and the orchestrator's re-run takes the recovery path
//! at its first round. A status 3 answered to the bootstrap key itself
//! (nothing left to retry) surfaces `NeedsResync` too, exactly like T3's
//! status-9 guard. Everything else surfaces through the Sync family
//! classifier (`adapter/error.rs`): 5/16 transient → `Retryable`, 4/6/8 →
//! `Permanent`.
//!
//! **Scope:** the bound folder's `MailboxId` IS the `Sync` `CollectionId` —
//! T3's `sync_mailboxes` maps FolderSync ServerIds verbatim into
//! `MailboxId`s, so the email slice resolves the folder to a collection by
//! identity, no mapping table.
//!
//! **Round quirks** (both live-proven in the Kylins client, ported from its
//! `eas_source` drain loop): a success response that omits its SyncKey
//! echoes the request's key (an empty key would poison the cursor — the
//! shared `next_key` invariant); a bootstrap round that returns nothing but
//! a rotated key is **followed once** (Exchange 15.2 streams the real items
//! only from the second round); and a `MoreAvailable` round that neither
//! rotates the key nor delivers items cannot advance — the pass completes.
//!
//! **Depth window:** the wire filter stays off (`FilterType` 0 — the
//! Kylins client's live-proven shape). A bounded window's precise bound
//! holds at apply time (`SyncWindow::admits` filters every additive chunk,
//! `engine-sync`'s streaming loop), with the host's prune pass as the
//! backstop for a Reconcile recovery's coarser enumeration; the
//! `FilterType` ladder (an enum code table, not days: 1=1d, 2=3d, 3=1w,
//! 4=2w, 5=1m, 6=3m, 7=6m — [MS-ASCMD] §2.2.3.68) is a pending adapter
//! slice. **Bodies are not fetched** (`fetch_body: false`): the stream is
//! the metadata tier (subject/from/to/cc/reply-to, dates, flags, preview
//! when the server volunteers one) — the IMAP stream discipline ("previews
//! are not hydrated here"); whole bodies are T6's `fetch_message_source`.

use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{EmailAddress, Keyword, Message, SystemKeyword},
    membership::Memberships,
    sync::SyncState,
    time::UtcDateTime,
};
use engine_provider::{
    EmailChunk, EmailStream, PassMode, ProviderError, ProviderResult, split_page,
};
use serde_json::json;
use tokio::sync::Mutex;

use super::{
    error::provider_error,
    mailboxes::{BOOTSTRAP_KEY, next_key, request_key},
};
use crate::{
    client::{EasClient, EasError},
    commands::common_status_message,
    status::{RecoveryAction, recovery_action_for_sync},
    types::{EasItem, SyncRequest},
};

/// The adapter's maximum round size, used when `fetch_batch` is 0 ("the
/// adapter's maximum / one chunk per batch"): the Kylins/Android drain-loop
/// cap — larger windows risk multi-megabyte responses ([MS-ASCMD]
/// §2.2.3.199 notes values above the 100 optimum risk oversized,
/// error-prone responses).
const MAX_WINDOW_SIZE: u32 = 512;

/// The adapter's extended-property namespace (the mailboxes slice's
/// convention): the EAS-native facts with no first-class `Message` field.
const EXTENDED_NAMESPACE: &str = "eas";

/// Streams the bound folder's email for one pass. See the module docs for
/// the full mapping; the round loop is the Kylins `eas_source` drain shape
/// on the engine's streaming contract.
pub(super) fn stream<'a>(
    client: &'a Mutex<EasClient>,
    folder: &'a MailboxId,
    cursor: Option<&'a SyncState>,
    fetch_batch: usize,
    chunk_size: usize,
) -> EmailStream<'a> {
    Box::pin(async_stream::try_stream! {
        // The verb lock for the whole pass — the IMAP stream precedent: the
        // client's command methods rotate session state in place, so one
        // stream drives one client at a time.
        let mut client = client.lock().await;
        let mut key = request_key(cursor).to_owned();
        let mut mode = PassMode::Additive;
        // Whether this pass has yielded any chunk yet — the guard that keeps
        // the invalidation recovery at the pass boundary (constant mode).
        let mut emitted = false;
        loop {
            let result = client
                .sync(&SyncRequest {
                    collection_id: folder.as_str().to_owned(),
                    sync_key: key.clone(),
                    // Routes the response parser to the Email-shaped path;
                    // the request builder emits no Class element in 14.0+.
                    class: "Email".to_owned(),
                    window_size: window_size(fetch_batch),
                    // No wire filter — see the module docs' depth-window note.
                    filter_age_days: 0,
                    // Metadata tier — bodies are fetch_message_source (T6).
                    fetch_body: false,
                    truncation_size: None,
                    mime_support: None,
                    mime_truncation: None,
                    supported: None,
                })
                .await
                .map_err(provider_error)?;
            match recovery_action_for_sync(result.status) {
                RecoveryAction::Ok => {}
                // The stored key is dead — recover inside the stream,
                // exactly once, and only at the pass boundary: restart from
                // the bootstrap key as a Reconcile pass (the full
                // re-enumeration is the snapshot recovery). A dead key that
                // IS the bootstrap key, or an invalidation after chunks have
                // been yielded, falls through to the surface arm below —
                // structural guards, so this can never loop.
                RecoveryAction::ResetSyncKey | RecoveryAction::RunFolderSync
                    if !emitted && key != BOOTSTRAP_KEY =>
                {
                    key.clear();
                    key.push_str(BOOTSTRAP_KEY);
                    mode = PassMode::Reconcile;
                    continue;
                }
                // Surfaces through the Sync family classifier: 3/12 →
                // NeedsResync (the orchestrator re-runs, and the re-run
                // recovers at its first round), 5/16 → Retryable, else
                // Permanent.
                _ => {
                    let error = sync_status_error(result.status);
                    Err::<EmailChunk, ProviderError>(error)?;
                }
            }

            // A success response that omits its SyncKey keeps the request's
            // (the empty-key cursor-poisoning invariant).
            let next = next_key(&result.sync_key, &key).to_owned();
            let changed: Vec<Message> = result
                .added
                .iter()
                .chain(&result.updated)
                .map(|item| message(item, folder))
                .collect::<ProviderResult<_>>()?;
            let removed: Vec<ProviderKey> = result
                .deleted_server_ids
                .iter()
                .filter(|id| !id.is_empty())
                .map(|id| ProviderKey::new(id.clone()))
                .collect::<Result<_, _>>()
                .map_err(|e| unusable_id(&e.to_string()))?;
            let items_returned = changed.len() + removed.len();

            match mode {
                PassMode::Additive => {
                    for chunk in additive_round_chunks(changed, removed, chunk_size, &next) {
                        emitted = true;
                        yield chunk;
                    }
                }
                // A from-"0" re-enumeration has no prior server state to
                // delete against — any wire deletes are covered by the
                // pass-end tombstone against `present` (a safe superset),
                // so `removed` is folded away rather than emitted.
                PassMode::Reconcile => {
                    let present: Vec<ProviderKey> =
                        changed.iter().map(|m| m.id.key().clone()).collect();
                    for chunk in split_page(
                        PassMode::Reconcile,
                        changed,
                        Vec::new(),
                        Vec::new(),
                        present,
                        None,
                        chunk_size,
                    ) {
                        emitted = true;
                        yield chunk;
                    }
                }
            }

            // Round control: follow the Exchange 15.2 empty-bootstrap quirk
            // once; complete when no more pages remain; and treat a
            // MoreAvailable round that made no progress at all (no key
            // rotation, no items — the Kylins drain-loop stall rule) as the
            // pass's end rather than spinning on it.
            let follow =
                should_follow_empty_bootstrap(&key, items_returned, result.more_available, &next);
            let stalled = result.more_available && next == key && items_returned == 0;
            if (!result.more_available && !follow) || stalled {
                if mode == PassMode::Reconcile {
                    // The completing marker: advance to the last round's key
                    // and tombstone against the accumulated present set.
                    yield EmailChunk::reconcile_last(
                        Vec::new(),
                        Vec::new(),
                        None,
                        SyncState::new(next),
                    );
                }
                return;
            }
            key = next;
        }
    })
}

/// The round's chunks in [`PassMode::Additive`]: sub-chunks of `chunk_size`
/// (`0` = one) with every intermediate holding the cursor, and the
/// completing chunk carrying the round's whole delta — items, explicit
/// removals, and the rotated key as the checkpoint. The round's removals
/// ride the completing chunk (not the first): a crash mid-round resumes
/// from the pre-round key and the server re-delivers the whole page, so
/// nothing committed before it needs to describe the round.
fn additive_round_chunks(
    changed: Vec<Message>,
    removed: Vec<ProviderKey>,
    chunk_size: usize,
    checkpoint: &str,
) -> Vec<EmailChunk> {
    if chunk_size == 0 {
        return vec![EmailChunk::additive(
            changed,
            removed,
            None,
            SyncState::new(checkpoint),
        )];
    }
    let mut removed = Some(removed);
    let mut chunks = Vec::new();
    let mut iter = changed.into_iter().peekable();
    while iter.peek().is_some() {
        let batch: Vec<Message> = iter.by_ref().take(chunk_size).collect();
        if iter.peek().is_none() {
            chunks.push(EmailChunk::additive(
                batch,
                removed.take().unwrap_or_default(),
                None,
                SyncState::new(checkpoint),
            ));
        } else {
            chunks.push(EmailChunk::additive_held(batch, Vec::new(), None));
        }
    }
    if chunks.is_empty() {
        // An empty round still yields one chunk — the checkpoint the caller
        // persists (the no-changes shape).
        chunks.push(EmailChunk::additive(
            Vec::new(),
            removed.take().unwrap_or_default(),
            None,
            SyncState::new(checkpoint),
        ));
    }
    chunks
}

/// Empty-bootstrap follow-up (the Kylins `eas_source` rule, live evidence
/// 2026-08-02): Exchange 15.2 answers a bootstrap Sync ("0") with an EMPTY
/// response — a fresh key, no items, `more_available = false` — and only
/// streams items from the second round issued with that key. Follow the key
/// with one more round iff the round was a bootstrap, returned nothing,
/// claims nothing more, and rotated to a usable key. Loop-safe by
/// construction: the follow-up's request key is the rotated key (`!= "0"`),
/// so the predicate cannot re-fire.
fn should_follow_empty_bootstrap(
    request_key: &str,
    items_returned: usize,
    more_available: bool,
    result_key: &str,
) -> bool {
    request_key == BOOTSTRAP_KEY
        && items_returned == 0
        && !more_available
        && !result_key.is_empty()
        && result_key != BOOTSTRAP_KEY
}

/// The `WindowSize` a round requests: the caller's fetch batch, with 0
/// ("the adapter's maximum") resolving to the drain-loop cap.
fn window_size(fetch_batch: usize) -> u32 {
    if fetch_batch == 0 {
        MAX_WINDOW_SIZE
    } else {
        u32::try_from(fetch_batch).unwrap_or(MAX_WINDOW_SIZE)
    }
}

/// Maps one wire email item onto the engine's `Message`: ServerId → id,
/// membership the bound folder, `Read`/`Flag`/`IsDraft` → `$seen`/
/// `$flagged`/`$draft`, `DateReceived` → `received_at` (an unparseable
/// timestamp stays `None` — undated mail is admitted by the window, never
/// dropped for a missing header), `From`/`To`/`Cc` → envelope addresses
/// verbatim (EAS sends free-form strings; display-name splitting is the
/// parser tier's job), and the EAS-native facts (`Importance`,
/// `MessageClass`, `MeetingMessageType`) survive under the adapter's
/// extended namespace — the meeting-type is what arms the invitation UI
/// ([MS-ASCMD] §3.1.5.6: values 1|2).
fn message(item: &EasItem, folder: &MailboxId) -> ProviderResult<Message> {
    let id =
        MessageId::try_from(item.server_id.as_str()).map_err(|e| unusable_id(&e.to_string()))?;
    let mut message = Message::new(id, Memberships::of_one(folder.clone()));
    message.envelope.subject.clone_from(&item.subject);
    if let Some(from) = &item.from {
        message.envelope.from.push(EmailAddress::new(from.clone()));
    }
    if let Some(to) = &item.to {
        message.envelope.to.push(EmailAddress::new(to.clone()));
    }
    if let Some(cc) = &item.cc {
        message.envelope.cc.push(EmailAddress::new(cc.clone()));
    }
    if let Some(reply_to) = &item.reply_to {
        message
            .envelope
            .reply_to
            .push(EmailAddress::new(reply_to.clone()));
    }
    message.received_at = item
        .date_received
        .as_deref()
        .and_then(|s| UtcDateTime::parse_rfc3339(s).ok());
    message.preview.clone_from(&item.preview);
    message.has_attachment = item.has_attachments;
    if item.read == Some(true) {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Seen));
    }
    if item.flag == Some(true) {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Flagged));
    }
    if item.is_draft == Some(true) {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Draft));
    }
    if let Some(importance) = item.importance {
        message.extended.set(
            format!("{EXTENDED_NAMESPACE}/importance"),
            json!(importance),
        );
    }
    if let Some(class) = &item.message_class {
        message
            .extended
            .set(format!("{EXTENDED_NAMESPACE}/message-class"), json!(class));
    }
    if let Some(meeting_type) = item.meeting_message_type {
        message.extended.set(
            format!("{EXTENDED_NAMESPACE}/meeting-message-type"),
            json!(meeting_type),
        );
    }
    Ok(message)
}

/// The surfaced error for a non-success collection status — through the
/// family-tagged variant, so it classifies via the Sync table with the
/// protocol failure kept as the `source` chain.
fn sync_status_error(status: u32) -> ProviderError {
    provider_error(EasError::SyncStatus {
        status,
        message: format!(
            "Sync failed: {}",
            common_status_message(status).unwrap_or("collection status not success")
        ),
    })
}

/// The shared error for an id the engine cannot key (an empty ServerId): a
/// malformed-change failure, permanent because resending the same round
/// returns the same bytes (the mailboxes slice's rule).
fn unusable_id(detail: &str) -> ProviderError {
    ProviderError::permanent(format!("Sync change with an unusable id: {detail}"))
}

#[cfg(test)]
#[path = "email_tests.rs"]
mod tests;
