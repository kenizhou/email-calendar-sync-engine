//! Contact create/patch/delete drivers through the durable outbox.

use core::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactPatch},
    ids::{AccountId, ContactId},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use engine_provider::{ContactWriteReceipt, ContactsProvider, ProviderError};
use engine_store::{LeasedPendingOp, Store, WorkerId};

use super::{OutboxIntent, enqueue_and_claim, record_failure};
use crate::SyncError;

/// Successful contact write identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactWriteOutcome {
    /// Durable operation.
    pub op: PendingOpId,
    /// Provider card identity.
    pub contact: ContactId,
}

/// Creates one explicitly targeted contact.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
pub async fn create_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    draft: &ContactDraft,
) -> Result<ContactWriteOutcome, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let resource = format!("contact-create:{}", draft.address_book.as_str());
    let leased = enqueue_contact_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &resource,
        OutboxIntent::CreateContact {
            draft: draft.clone(),
        },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_create_contact(provider, account, draft).await,
    )
    .await
}

/// Patches one backing source card.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
#[allow(
    clippy::too_many_arguments,
    reason = "outbox lease parameters plus the source base and patch intent"
)]
pub async fn patch_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &ContactCard,
    patch: &ContactPatch,
) -> Result<ContactWriteOutcome, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let leased = enqueue_contact_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &format!("contact:{}", base.id.as_str()),
        OutboxIntent::PatchContact {
            contact: base.id.clone(),
            patch: patch.clone(),
        },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_patch_contact(provider, account, base, patch).await,
    )
    .await
}

/// Deletes one backing source card.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
pub async fn delete_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &ContactCard,
) -> Result<PendingOpId, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let leased = enqueue_contact_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &format!("contact:{}", base.id.as_str()),
        OutboxIntent::DeleteContact {
            contact: base.id.clone(),
        },
    )
    .await?;
    match execute_delete_contact(provider, account, base).await {
        Ok(()) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: base.id.key().clone(),
                    },
                )
                .await?;
            Ok(leased.id)
        }
        Err(error) => {
            record_failure(store, &leased, &error).await?;
            Err(SyncError::Provider(error))
        }
    }
}

async fn resolve<S: Store>(
    store: &S,
    leased: LeasedPendingOp,
    result: engine_provider::ProviderResult<ContactWriteReceipt>,
) -> Result<ContactWriteOutcome, SyncError> {
    match result {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.contact.key().clone(),
                    },
                )
                .await?;
            Ok(ContactWriteOutcome {
                op: leased.id,
                contact: receipt.contact,
            })
        }
        Err(error) => {
            record_failure(store, &leased, &error).await?;
            Err(SyncError::Provider(error))
        }
    }
}

/// Executes one claimed contact create: the provider call the `create_contact`
/// verb names. The execution half the inline driver runs and the contact
/// dispatcher ([`execute_claimed_contact`](super::execute::execute_claimed_contact))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_create_contact<P: ContactsProvider>(
    provider: &P,
    account: &AccountId,
    draft: &ContactDraft,
) -> Result<ContactWriteReceipt, ProviderError> {
    provider.create_contact(account, draft).await
}

/// Executes one claimed contact patch: the provider call the `patch_contact`
/// verb names, applied to `base` — the card as the caller read it on the inline
/// path, re-read from the store on a replay (the intent deliberately carries
/// only the change and its target). The execution half the inline driver runs
/// and the contact dispatcher
/// ([`execute_claimed_contact`](super::execute::execute_claimed_contact))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_patch_contact<P: ContactsProvider>(
    provider: &P,
    account: &AccountId,
    base: &ContactCard,
    patch: &ContactPatch,
) -> Result<ContactWriteReceipt, ProviderError> {
    provider.patch_contact(account, base, patch).await
}

/// Executes one claimed contact delete: the provider call the `delete_contact`
/// verb names. `base` arrives exactly as on the patch half; an already-absent
/// card is the provider verb's own success, which the replay path honors
/// without a call. The execution half the inline driver runs and the contact
/// dispatcher ([`execute_claimed_contact`](super::execute::execute_claimed_contact))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_delete_contact<P: ContactsProvider>(
    provider: &P,
    account: &AccountId,
    base: &ContactCard,
) -> Result<(), ProviderError> {
    provider.delete_contact(account, base).await
}

#[allow(
    clippy::too_many_arguments,
    reason = "outbox lease parameters and serialized operation identity"
)]
async fn enqueue_contact_op<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    resource: &str,
    intent: OutboxIntent,
) -> Result<LeasedPendingOp, SyncError> {
    let payload = serde_json::to_value(&intent)
        .map_err(|error| SyncError::Outbox(format!("encode contact write: {error}")))?;
    let idempotency =
        IdempotencyKey::new(idempotency).map_err(|error| SyncError::Outbox(error.to_string()))?;
    let resource =
        ResourceKey::new(resource).map_err(|error| SyncError::Outbox(error.to_string()))?;
    enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, resource, payload),
    )
    .await
}
