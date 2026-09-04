// SPDX-License-Identifier: MPL-2.0
//! The contacts write verbs (P2 Task 5, split from `contacts.rs` for the
//! 500-line rule — the `calendar_write` precedent): Sync Add/Change/Delete
//! upsync through `contacts_sync_changes`, riding the contacts
//! collection-key ledger. See `contacts.rs` module docs for the family
//! map and the honest refusals; the write-half rules restated:
//!
//! - **`create_contact` → Sync `Add`** with a synthesized `ClientId` — the
//!   only id-reveal point: the receipt keys the `ServerId` the ack assigns
//!   ([MS-ASCMD] §2.2.3.7.2); an ack-less success keys the ClientId
//!   placeholder, reconciled by the next card pass.
//! - **`patch_contact` → Sync `Change`** carrying only the patched families
//!   (the ghost model — untouched fields omitted, cleared slots emit empty
//!   values). An empty patch is a no-op receipt: the outbox driver does not
//!   pre-filter emptiness (checked `engine-sync/src/outbox/contact.rs`), so
//!   the adapter honors it here — no wire round.
//! - **`delete_contact` → Sync `Delete`** — already-gone is success (a
//!   per-item status 8, or no item status at all, §2.2.3.154).
//!
//! A cold ledger refuses `NeedsResync`; a dead key surfaces as Sync status
//! 3 through the family classifier; a failed item status surfaces with its
//! code (8 on a patch is `Conflict` — refetch, re-apply).

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactKind, ContactPatch, FieldPatch},
    ids::{AddressBookId, ContactId},
};
use engine_provider::{ContactWriteReceipt, ProviderError, ProviderResult};
use tokio::sync::Mutex;

use super::{
    CollectionKey, current_key, error::provider_error, error::sync_status_error, record_rotation,
};
use crate::contacts::write_from_patch;
use crate::{client::EasClient, commands::ContactsChange, contacts::write_from_draft};

/// The per-item "object not found" status ([MS-ASCMD] §2.2.3.177.17): the
/// delete's already-gone success, and a patch's refetch signal.
const ITEM_OBJECT_NOT_FOUND: u32 = 8;

/// Creates a card: Sync `Add` with a synthesized ClientId — the only
/// id-reveal point (see the module docs). The draft converts through
/// `contacts::write_from_draft` first.
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; an unconvertible draft refuses
/// `Permanent` before the wire; a failed collection status classifies
/// through the Sync family table; a failed Add ack surfaces with its item
/// status.
pub(super) async fn create_card(
    client: &Mutex<EasClient>,
    book: &AddressBookId,
    ledger: &CollectionKey,
    draft: &ContactDraft,
) -> ProviderResult<ContactWriteReceipt> {
    if draft.address_book != *book {
        return Err(ProviderError::invalid_state(format!(
            "the EAS adapter is bound to contact folder {} — it cannot create into {}",
            book.as_str(),
            draft.address_book.as_str()
        )));
    }
    let props = write_from_draft(&draft.card)?;
    let client_id = crate::types::new_contacts_client_id();
    let key = current_key(ledger)?;
    let outcome = upsync(
        client,
        book,
        &key,
        &[ContactsChange::Add {
            client_id: client_id.clone(),
            props,
        }],
    )
    .await?;
    record_rotation(ledger, &outcome);
    let assigned = outcome
        .add_acks
        .iter()
        .find(|ack| ack.client_id == client_id);
    let id = match assigned {
        // §2.2.3.7.2: the ack carries the assigned ServerId on success.
        Some(ack) if ack.success() => ack
            .server_id
            .clone()
            .ok_or_else(|| ack_without_an_id(ack))?,
        // A failed ack is the server's rejection of the item itself.
        Some(ack) => {
            return Err(ProviderError::permanent(format!(
                "the server rejected the contact Add (item status {}): {}",
                ack.status,
                crate::commands::common_status_message(ack.status)
                    .unwrap_or("no further detail available")
            )));
        }
        // §2.2.3.154: no ack means success with no id to correlate — the
        // ClientId placeholder reconciles away on the next card pass.
        None => {
            log::debug!(
                "EAS contact Add succeeded without a Responses ack; the receipt keys the \
                 ClientId placeholder until the next card pass"
            );
            client_id
        }
    };
    let contact = ContactId::try_from(id.as_str()).map_err(|e| {
        ProviderError::permanent(format!(
            "the server assigned a ServerId that cannot key a contact: {e}"
        ))
    })?;
    Ok(ContactWriteReceipt::new(contact))
}

/// Applies a targeted patch: Sync `Change` carrying only the patched
/// families (the ghost model — untouched fields are omitted, cleared
/// slots emit empty values). An empty patch is a no-op receipt.
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; an unrepresentable patch refuses
/// `Permanent` (see `contacts::write`); a failed collection status
/// classifies through the Sync family table; a failed item status
/// surfaces with its code (8 as `Conflict` — refetch, re-apply).
pub(super) async fn patch_card(
    client: &Mutex<EasClient>,
    book: &AddressBookId,
    ledger: &CollectionKey,
    base: &ContactCard,
    patch: &ContactPatch,
) -> ProviderResult<ContactWriteReceipt> {
    // The no-op patch: nothing to set and nothing to clear (a kind Set of
    // Individual is the identity on EAS — the class is individual-only).
    // The outbox driver does not pre-filter emptiness, so the no-op lives
    // here — no wire round, the receipt records the card as it stands.
    if patch.fields.is_empty()
        && matches!(
            patch.kind,
            None | Some(FieldPatch::Set(ContactKind::Individual))
        )
    {
        return Ok(ContactWriteReceipt::new(base.id.clone()));
    }
    let props = write_from_patch(patch)?;
    let key = current_key(ledger)?;
    let outcome = upsync(
        client,
        book,
        &key,
        &[ContactsChange::Replace {
            server_id: base.id.as_str().to_owned(),
            props,
        }],
    )
    .await?;
    record_rotation(ledger, &outcome);
    check_item_status(&outcome, base.id.as_str())?;
    Ok(ContactWriteReceipt::new(base.id.clone()))
}

/// Deletes a card: Sync `Delete` of the ServerId. Already-gone is success
/// (a per-item 8, or no item status at all — [MS-ASCMD] §2.2.3.154).
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; a failed collection status
/// classifies through the Sync family table; a failed item status
/// surfaces with its code.
pub(super) async fn delete_card(
    client: &Mutex<EasClient>,
    book: &AddressBookId,
    ledger: &CollectionKey,
    base: &ContactCard,
) -> ProviderResult<()> {
    let key = current_key(ledger)?;
    let outcome = upsync(
        client,
        book,
        &key,
        &[ContactsChange::Remove {
            server_id: base.id.as_str().to_owned(),
        }],
    )
    .await?;
    record_rotation(ledger, &outcome);
    if let Some(status) = outcome
        .item_statuses
        .iter()
        .find(|status| status.server_id == base.id.as_str())
    {
        if status.status != ITEM_OBJECT_NOT_FOUND {
            return Err(item_status_error(status));
        }
        log::debug!(
            "EAS contact delete answered object-not-found (status 8) — already gone, the \
             idempotent success"
        );
    }
    Ok(())
}

/// One contacts upsync round: the verb lock, the Sync-family status
/// classification, the plain transport map.
async fn upsync(
    client: &Mutex<EasClient>,
    book: &AddressBookId,
    key: &str,
    changes: &[ContactsChange],
) -> ProviderResult<crate::commands::SyncChangeOutcome> {
    let mut client = client.lock().await;
    match client
        .contacts_sync_changes(book.as_str(), key, changes)
        .await
    {
        Ok(outcome) => Ok(outcome),
        // The upsync's own statuses are Sync-family (a dead key is 3): the
        // classifier, never the family-blind error text.
        Err(
            crate::client::EasError::SyncStatus { status, .. }
            | crate::client::EasError::CommandStatus { status, .. },
        ) => Err(sync_status_error(status)),
        Err(e) => Err(provider_error(e)),
    }
}

/// Surfaces a failed item status for a patch (8 = the object is gone =
/// `Conflict`: refetch and re-apply, never blind-retry).
fn check_item_status(
    outcome: &crate::commands::SyncChangeOutcome,
    server_id: &str,
) -> ProviderResult<()> {
    if let Some(status) = outcome
        .item_statuses
        .iter()
        .find(|status| status.server_id == server_id)
    {
        if status.status == ITEM_OBJECT_NOT_FOUND {
            return Err(ProviderError::new(
                engine_core::error::FailureClass::Conflict,
                "the contact is gone server-side (item status 8) — refetch the card and \
                 re-apply the patch",
            ));
        }
        return Err(item_status_error(status));
    }
    Ok(())
}

/// The shared failed-item-status error, naming the code and its meaning.
fn item_status_error(status: &crate::commands::CalendarItemStatus) -> ProviderError {
    ProviderError::permanent(format!(
        "the server rejected the contact write for {} (item status {}): {}",
        status.server_id,
        status.status,
        crate::commands::common_status_message(status.status)
            .unwrap_or("no further detail available")
    ))
}

/// The ack-success-without-an-id anomaly: §2.2.3.7.2 assigns the ServerId
/// on success, so a success ack carrying none is a server-shape violation
/// worth naming rather than papering over.
fn ack_without_an_id(ack: &crate::commands::CalendarAddAck) -> ProviderError {
    ProviderError::permanent(format!(
        "the Add ack for {} reported success but carried no ServerId ([MS-ASCMD] §2.2.3.7.2 \
         assigns one) — retry the create; the next card pass reconciles any duplicate",
        ack.client_id
    ))
}
