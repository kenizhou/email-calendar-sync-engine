// SPDX-License-Identifier: MPL-2.0
//! The `edit_mail` verb: the three trait mutations over their EAS commands.
//!
//! ## Mapping
//!
//! **`SetKeywords` → `Sync` Commands `Change`** ([MS-ASSYNC] §2.2.2). The
//! wire vocabulary is exactly `email:Read` and `email:Flag` — the engine's
//! `$seen` and `$flagged` — so any other keyword is refused permanently
//! BEFORE the wire (the IMAP `PERMANENTFLAGS` refusal spirit: never write a
//! flag the protocol will silently drop).
//!
//! **The collection SyncKey comes from the adapter's ledger** (see
//! [`EasAdapter`](super::EasAdapter)'s field docs): the trait's write seam
//! carries no cursor, and EAS keys are per-collection-per-device server
//! state, so the adapter owns the key the server last handed it — a
//! completed `stream_email` pass seeds it, an edit rides it and records its
//! rotation. The upsync request sends no `GetChanges`, so an edit rotation
//! carries no server rows and the next pass may resume additively from the
//! rotated key. A cold ledger (a fresh adapter that has not yet observed a
//! pass) refuses [`NeedsResync`](engine_core::error::FailureClass::NeedsResync)
//! rather than guessing: the orchestrator re-syncs, the pass re-seeds, and
//! the outbox retries the op.
//!
//! **`MoveTo` → `MoveItems`** with the adapter's bound folder as the source
//! collection and the destination `MailboxId` verbatim (both are folder
//! ServerIds under the T3 identity mapping). The move mints a NEW ServerId
//! (`DstMsgId`) server-side, so the receipt records the SOURCE key — the
//! destination copy reconciles on the next sync of that folder, the IMAP
//! move semantics the trait documents.
//!
//! **`Delete` is refused** ([`InvalidState`](engine_core::error::FailureClass::InvalidState)):
//! the trait's Delete is the PERMANENT delete, and EAS has no per-item
//! hard-delete command (only whole-folder `EmptyFolderContents`). The
//! documented adapter policy: move to the deleted-items folder with
//! `MoveTo` — what the trait itself calls "the mechanism behind a Trash
//! delete".

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::{Keyword, SystemKeyword},
};
use engine_provider::{MailEdit, MailEditReceipt, ProviderError, ProviderResult};
use tokio::sync::Mutex;

use super::{
    CollectionKey,
    error::{move_status_error, provider_error, sync_status_error},
};
use crate::{
    client::{EasClient, EasError},
    commands::{EasChange, SyncChangeOutcome},
};

/// Applies one [`MailEdit`] over the bound folder. See the module docs for
/// the per-verb mapping and the key-ledger discipline.
///
/// # Errors
///
/// A vocabulary violation and `Delete` are refused before the wire; a
/// non-success Sync collection status classifies through the Sync family
/// table (a dead key is `NeedsResync`), a MoveItems failure through the
/// Move table.
pub(super) async fn edit(
    client: &Mutex<EasClient>,
    folder: &MailboxId,
    ledger: &CollectionKey,
    edit: &MailEdit,
) -> ProviderResult<MailEditReceipt> {
    match edit {
        MailEdit::SetKeywords {
            target,
            add,
            remove,
        } => {
            let Some(change) = keyword_change(target, add, remove)? else {
                // Nothing expressible changed (both sets empty) — the
                // trait's no-op direction.
                return Ok(MailEditReceipt::new(target.clone()));
            };
            let key = current_key(ledger)?;
            let mut client = client.lock().await;
            let outcome = match client
                .sync_changes(folder.as_str(), &key, std::slice::from_ref(&change))
                .await
            {
                Ok(outcome) => outcome,
                // A Sync-Change collection failure is Sync-family: a dead
                // key (3) must reach the orchestrator as NeedsResync, not
                // the family-blind FolderSync table.
                Err(EasError::CommandStatus { status, .. }) => {
                    return Err(sync_status_error(status));
                }
                Err(e) => return Err(provider_error(e)),
            };
            record_rotation(ledger, &outcome);
            Ok(MailEditReceipt::new(target.clone()))
        }
        MailEdit::MoveTo {
            target,
            destination,
        } => {
            let mut client = client.lock().await;
            let moves = [(
                target.as_str().to_owned(),
                folder.as_str().to_owned(),
                destination.as_str().to_owned(),
            )];
            match client.move_items(&moves).await {
                // The client's gate already surfaced the first failing
                // per-Move status as an error — reaching Ok means the
                // inverted table's success shape (3 + DstMsgId).
                Ok(_) => Ok(MailEditReceipt::new(target.clone())),
                Err(EasError::CommandStatus { status, .. }) => Err(move_status_error(status)),
                Err(e) => Err(provider_error(e)),
            }
        }
        MailEdit::Delete { target } => Err(ProviderError::invalid_state(format!(
            "EAS has no per-item hard delete (only whole-folder \
             EmptyFolderContents); move {} to the deleted-items folder with \
             MailEdit::MoveTo instead",
            target.as_str()
        ))),
    }
}

/// One keyword edit → the wire change. Returns `Ok(None)` when neither
/// expressible keyword appears (the trait's no-op direction), and refuses
/// any keyword outside the `$seen`/`$flagged` pair permanently — the
/// protocol has no element that would carry it.
fn keyword_change(
    target: &engine_core::ids::ProviderKey,
    add: &BTreeSet<Keyword>,
    remove: &BTreeSet<Keyword>,
) -> ProviderResult<Option<EasChange>> {
    for keyword in add.iter().chain(remove) {
        if keyword != &Keyword::system(SystemKeyword::Seen)
            && keyword != &Keyword::system(SystemKeyword::Flagged)
        {
            return Err(ProviderError::permanent(format!(
                "EAS upsyncs exactly read and flagged; the keyword {} has no \
                 Sync Change form",
                keyword.as_str()
            )));
        }
    }
    let read = toggle_of(add, remove, SystemKeyword::Seen);
    let starred = toggle_of(add, remove, SystemKeyword::Flagged);
    Ok(match (read, starred) {
        (None, None) => None,
        _ => Some(EasChange {
            server_id: target.as_str().to_owned(),
            read,
            starred,
        }),
    })
}

/// Which state one keyword moves to: set, cleared, or untouched.
fn toggle_of(
    add: &BTreeSet<Keyword>,
    remove: &BTreeSet<Keyword>,
    keyword: SystemKeyword,
) -> Option<bool> {
    let keyword = Keyword::system(keyword);
    if add.contains(&keyword) {
        Some(true)
    } else if remove.contains(&keyword) {
        Some(false)
    } else {
        None
    }
}

/// The key an edit rides: the ledger's, or the cold-ledger refusal.
fn current_key(ledger: &CollectionKey) -> ProviderResult<String> {
    ledger
        .lock()
        .expect("collection-key ledger")
        .clone()
        .ok_or_else(|| {
            ProviderError::new(
                FailureClass::NeedsResync,
                "the collection's sync key is unknown to this adapter — run a \
                 sync pass first (it seeds the key); the outbox retries the \
                 edit after it",
            )
        })
}

/// Records an edit's key rotation. A response that piggybacked server rows
/// (no `GetChanges` was sent, so a conforming server sends none — a
/// nonconforming one might) drops the ledger instead: those rows cannot
/// ride the receipt back, and falling to the engine cursor surfaces the
/// gap as a Reconcile on the next pass rather than skipping them forever.
fn record_rotation(ledger: &CollectionKey, outcome: &SyncChangeOutcome) {
    let mut slot = ledger.lock().expect("collection-key ledger");
    if outcome.has_piggybacked() {
        log::warn!(
            "EAS Sync change response piggybacked server commands — dropping \
             the collection-key ledger; the next pass reconciles"
        );
        *slot = None;
    } else {
        *slot = Some(outcome.new_key.clone());
    }
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
