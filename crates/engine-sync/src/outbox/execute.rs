//! The execution dispatcher: one entry that runs any claimed outbox op's
//! provider call from its tagged intent alone.
//!
//! Every inline driver is enqueue-and-claim plus its verb's `execute_*` half
//! plus a mark; [`execute_claimed`] is that middle step reached from the
//! durable record instead of from the caller's arguments — the half a drainer
//! replays a claimed op through. It never records: the caller holds the lease
//! and marks whatever comes back.

use engine_core::{
    contact::ContactCard,
    error::FailureClass,
    ids::{AccountId, ContactId},
    write::PendingOutcome,
};
use engine_provider::ContactsProvider;
use engine_store::{LeasedPendingOp, Store, StoreRead};

use super::{
    OutboxIntent,
    contact::{execute_create_contact, execute_delete_contact, execute_patch_contact},
    mail::{execute_edit_mail, execute_report_message, execute_submit_mail, send_failure_outcome},
    write_failure_outcome,
};
use crate::SyncError;

/// Executes one claimed outbox op by dispatching on its tagged intent, with
/// exactly the inline drivers' semantics — including parking an ambiguous send
/// as `NeedsConfirmation` rather than a plain failure, so a replay can never
/// blind-retry one (`providers.md`). Returns the outcome for the caller to
/// record under the lease; does not touch the store's op state itself.
///
/// Calendar verbs are refused outright: replaying them needs a re-fetched base
/// and conflict recovery this phase does not build (`store-and-sync.md`), so
/// the caller — not this dispatcher — decides the accounting, and the inline
/// driver remains a calendar op's only executor. A payload that does not
/// decode is likewise returned as an error rather than executed, for the same
/// caller-decides rule.
///
/// Contact patch and delete re-read the base card the intent targets from the
/// store (the intent deliberately carries only the change; the inline path
/// already holds the caller's base). A patch whose card is already gone
/// resolves as the `Conflict` the provider verbs yield for a dead target —
/// terminal, corrected by the next sync, never retried into success — while a
/// delete whose card is gone completes with the success the verbs themselves
/// grant an already-absent card.
///
/// # Errors
///
/// Returns [`SyncError::Outbox`] for a payload (or stored base card) that does
/// not decode, or a calendar verb; [`SyncError::Store`] when the base-card read
/// fails. Provider failures are not errors: they arrive as the outcome the
/// caller records.
// The drain loop that calls this in production is the plan's next task; until
// it lands, only the direct tests reach this half (and its base-card helper).
#[allow(dead_code)]
pub(crate) async fn execute_claimed<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let intent = serde_json::from_value(leased.op.payload.clone()).map_err(|e| {
        SyncError::Outbox(format!(
            "undecodable payload for op {}: {e}",
            leased.id.get()
        ))
    })?;
    match intent {
        OutboxIntent::SubmitMail { payload } => Ok(
            match execute_submit_mail(provider, account, &payload).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.email_key,
                },
                Err(err) => send_failure_outcome(&err),
            },
        ),
        OutboxIntent::EditMail { edit } => {
            Ok(match execute_edit_mail(provider, account, &edit).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.message_key,
                },
                Err(err) => write_failure_outcome(&err),
            })
        }
        OutboxIntent::ReportMessage { report } => Ok(
            match execute_report_message(provider, account, &report).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.message_key,
                },
                Err(err) => write_failure_outcome(&err),
            },
        ),
        OutboxIntent::CreateContact { draft } => Ok(
            match execute_create_contact(provider, account, &draft).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.contact.key().clone(),
                },
                Err(err) => write_failure_outcome(&err),
            },
        ),
        OutboxIntent::PatchContact { contact, patch } => Ok(
            match contact_base(store, provider, account, &contact).await? {
                Some(base) => match execute_patch_contact(provider, account, &base, &patch).await {
                    Ok(receipt) => PendingOutcome::Succeeded {
                        provider_key: receipt.contact.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                },
                None => PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                },
            },
        ),
        OutboxIntent::DeleteContact { contact } => Ok(
            match contact_base(store, provider, account, &contact).await? {
                Some(base) => match execute_delete_contact(provider, account, &base).await {
                    Ok(()) => PendingOutcome::Succeeded {
                        provider_key: contact.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                },
                None => PendingOutcome::Succeeded {
                    provider_key: contact.key().clone(),
                },
            },
        ),
        OutboxIntent::CreateEvent { .. }
        | OutboxIntent::PatchEvent { .. }
        | OutboxIntent::PutEventDoc { .. }
        | OutboxIntent::RsvpEvent { .. }
        | OutboxIntent::DeleteEvent { .. } => Err(SyncError::Outbox(
            "calendar ops are not replayable in this phase".to_owned(),
        )),
    }
}

/// Reads the base card a replayed contact patch or delete applies to, by id,
/// from the provider's card scope as the store last synced it.
///
/// `Ok(None)` when the card is absent or tombstoned there. The inline drivers
/// never come here — they hold the caller's base; only a replay, whose sole
/// record is the intent, re-reads one.
///
/// # Errors
///
/// Returns [`SyncError::Store`] when the read fails, or [`SyncError::Outbox`]
/// when the stored payload does not decode as a card.
#[allow(dead_code)]
async fn contact_base<P, S>(
    store: &S,
    provider: &P,
    account: &AccountId,
    contact: &ContactId,
) -> Result<Option<ContactCard>, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let payload = store
        .object_payload(&provider.contact_scope(account), contact.key())
        .await?;
    payload
        .map(|value| {
            serde_json::from_value(value).map_err(|e| {
                SyncError::Outbox(format!("undecodable stored card {}: {e}", contact.as_str()))
            })
        })
        .transpose()
}
