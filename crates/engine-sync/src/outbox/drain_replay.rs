//! The replay halves of the PIM drainer: run one claimed contact or calendar
//! op's provider call from its tagged intent alone — the middle step the
//! inline drivers reach from the caller's arguments, reached here from the
//! durable record instead.
//!
//! A replay re-reads the base the base-dependent verbs need from the store, by
//! id — the freshly fetched base the intent contract promises a retry (the
//! payload carries the change, never the base it was read at). A patch or RSVP
//! whose target is gone resolves as the [`FailureClass::Conflict`] the inline
//! semantics grant a dead target — terminal, corrected by the next sync, never
//! retried into success — while a delete of a gone target succeeds, because an
//! occurrence of an absent event (or a card already absent) is already removed.
//!
//! Nothing here records an outcome: the caller holds the lease and marks
//! whatever comes back ([`drain_pim`](super::drain_pim)).

use engine_core::{
    calendar::Event,
    contact::ContactCard,
    error::FailureClass,
    ids::{AccountId, ContactId, EventId},
    mail::Message,
    write::PendingOutcome,
};
use engine_provider::{ContactsProvider, DeleteTarget, Provider, ProviderError};
use engine_store::{LeasedPendingOp, Store, StoreError, StoreRead};

use super::{
    OutboxIntent,
    calendar::{
        execute_create_event, execute_delete_event, execute_patch_event, execute_put_event,
        execute_rsvp_event,
    },
    contact::{execute_create_contact, execute_delete_contact, execute_patch_contact},
    invite::execute_rsvp_event_from_invite,
};

/// Why a replay produced no ready outcome — the structured discrimination the
/// drain loop needs; never an error string.
pub(super) enum ReplayFault {
    /// The provider call failed. Recorded through `record_failure`, whose mark
    /// the store turns into a parked retry or a terminal settle — it owns the
    /// attempt bound, not the drainer.
    Provider(ProviderError),
    /// The op is terminal poison: its payload does not decode as the tagged
    /// intent its kind column names (or a stored base it re-reads does not
    /// decode as the object its id names). No execution exists and none ever
    /// will; the drain settles the op `Failed`/`Permanent` so it cannot
    /// recycle for ever.
    Poison,
    /// A store read the replay needed (a base re-read) failed: transient; the
    /// pass surfaces it, and the claim's lease expiry recycles the op for a
    /// later one.
    Store(StoreError),
}

/// Executes one claimed **contact** op — `CreateContact`, `PatchContact`, or
/// `DeleteContact` — with exactly the inline drivers' semantics, the base the
/// patch/delete verbs need re-read from the store by id. A patch whose card is
/// gone resolves as the `Conflict` the inline verbs yield a dead target; a
/// delete whose card is gone completes, because an already-absent card is
/// already deleted.
pub(super) async fn replay_contact_op<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, ReplayFault>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let intent = decode_matching(leased)?;
    match intent {
        OutboxIntent::CreateContact { draft } => {
            match execute_create_contact(provider, account, &draft).await {
                Ok(receipt) => Ok(PendingOutcome::Succeeded {
                    provider_key: receipt.contact.key().clone(),
                }),
                Err(err) => Err(ReplayFault::Provider(err)),
            }
        }
        OutboxIntent::PatchContact { contact, patch } => {
            match contact_base(store, provider, account, &contact).await? {
                Some(base) => match execute_patch_contact(provider, account, &base, &patch).await {
                    Ok(receipt) => Ok(PendingOutcome::Succeeded {
                        provider_key: receipt.contact.key().clone(),
                    }),
                    Err(err) => Err(ReplayFault::Provider(err)),
                },
                // A patch whose card is gone is the Conflict the inline verbs
                // yield a dead target: terminal, corrected by the next sync.
                None => Ok(PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                }),
            }
        }
        OutboxIntent::DeleteContact { contact } => {
            match contact_base(store, provider, account, &contact).await? {
                Some(base) => match execute_delete_contact(provider, account, &base).await {
                    Ok(()) => Ok(PendingOutcome::Succeeded {
                        provider_key: contact.key().clone(),
                    }),
                    Err(err) => Err(ReplayFault::Provider(err)),
                },
                // An already-absent card is already deleted: success.
                None => Ok(PendingOutcome::Succeeded {
                    provider_key: contact.key().clone(),
                }),
            }
        }
        // The drain's kind filter admits only contact kinds; a row whose
        // payload names another verb disagrees with its kind column and is
        // poison.
        _ => Err(ReplayFault::Poison),
    }
}

/// Executes one claimed **calendar** op — `CreateEvent`, `PatchEvent`,
/// `PutEventDoc`, `RsvpEvent` (direct or from-invite), or `DeleteEvent` — with
/// exactly the inline drivers' semantics, the base the base-dependent verbs
/// need re-read from the store by id. A patch or RSVP whose event is gone
/// resolves as the `Conflict` the inline verbs yield a dead target; an
/// occurrence delete whose event is gone completes, because an occurrence of
/// an absent event is already removed; a series delete and a document replace
/// need no base at all.
///
/// The from-invite answer is the one calendar verb whose replay runs without a
/// base: a message-referencing transport answers from the email alone, so a
/// re-read that finds none still executes — only the document transports'
/// default refuses, as it would inline.
pub(super) async fn replay_calendar_op<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, ReplayFault>
where
    P: Provider,
    S: Store + StoreRead,
{
    let intent = decode_matching(leased)?;
    match intent {
        OutboxIntent::CreateEvent { draft } => {
            match execute_create_event(provider, account, &draft).await {
                Ok(receipt) => Ok(PendingOutcome::Succeeded {
                    provider_key: receipt.event.key().clone(),
                }),
                Err(err) => Err(ReplayFault::Provider(err)),
            }
        }
        OutboxIntent::PatchEvent { edit } => {
            match event_base(store, provider, account, &edit.event).await? {
                Some(base) => match execute_patch_event(provider, account, &base, &edit).await {
                    Ok(receipt) => Ok(PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    }),
                    Err(err) => Err(ReplayFault::Provider(err)),
                },
                None => Ok(PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                }),
            }
        }
        OutboxIntent::PutEventDoc { write } => {
            match execute_put_event(provider, account, &write).await {
                Ok(receipt) => Ok(PendingOutcome::Succeeded {
                    provider_key: receipt.event.key().clone(),
                }),
                Err(err) => Err(ReplayFault::Provider(err)),
            }
        }
        OutboxIntent::RsvpEvent { rsvp } => {
            match event_base(store, provider, account, &rsvp.event).await? {
                Some(base) => match execute_rsvp_event(provider, account, &base, &rsvp).await {
                    Ok(receipt) => Ok(PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    }),
                    Err(err) => Err(ReplayFault::Provider(err)),
                },
                None => Ok(PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                }),
            }
        }
        OutboxIntent::RsvpEventFromInvite { rsvp, invite } => {
            let base = event_base(store, provider, account, &rsvp.event).await?;
            let message = Message::new(invite.message.clone(), invite.mailboxes.clone());
            match execute_rsvp_event_from_invite(provider, account, &message, base.as_ref(), &rsvp)
                .await
            {
                Ok(receipt) => Ok(PendingOutcome::Succeeded {
                    provider_key: receipt.event.key().clone(),
                }),
                Err(err) => Err(ReplayFault::Provider(err)),
            }
        }
        OutboxIntent::DeleteEvent { deletion } => match &deletion.target {
            DeleteTarget::Series => {
                match execute_delete_event(provider, account, None, &deletion).await {
                    Ok(()) => Ok(PendingOutcome::Succeeded {
                        provider_key: deletion.event.key().clone(),
                    }),
                    Err(err) => Err(ReplayFault::Provider(err)),
                }
            }
            DeleteTarget::Occurrence { .. } => {
                match event_base(store, provider, account, &deletion.event).await? {
                    Some(base) => {
                        match execute_delete_event(provider, account, Some(&base), &deletion).await
                        {
                            Ok(()) => Ok(PendingOutcome::Succeeded {
                                provider_key: deletion.event.key().clone(),
                            }),
                            Err(err) => Err(ReplayFault::Provider(err)),
                        }
                    }
                    // An occurrence of an absent event is already removed.
                    None => Ok(PendingOutcome::Succeeded {
                        provider_key: deletion.event.key().clone(),
                    }),
                }
            }
        },
        // The drain's kind filter admits only calendar kinds; a row whose
        // payload names another verb disagrees with its kind column and is
        // poison.
        _ => Err(ReplayFault::Poison),
    }
}

/// Decodes a claimed op's payload into its tagged intent, refusing any row
/// whose intent verb disagrees with its kind column — guessing the verb is
/// what the kind column exists to prevent.
fn decode_matching(leased: &LeasedPendingOp) -> Result<OutboxIntent, ReplayFault> {
    let intent = serde_json::from_value::<OutboxIntent>(leased.op.payload.clone())
        .map_err(|_| ReplayFault::Poison)?;
    if intent.pending_op_kind() == leased.op.kind {
        Ok(intent)
    } else {
        Err(ReplayFault::Poison)
    }
}

/// Reads the base card a replayed contact patch or delete applies to, by id,
/// from the provider's card scope as the store last synced it. `Ok(None)` when
/// the card is absent or tombstoned there; a stored card that does not decode
/// is poison for the same reason an undecodable payload is.
async fn contact_base<P, S>(
    store: &S,
    provider: &P,
    account: &AccountId,
    contact: &ContactId,
) -> Result<Option<ContactCard>, ReplayFault>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let payload = store
        .object_payload(&provider.contact_scope(account), contact.key())
        .await
        .map_err(ReplayFault::Store)?;
    payload
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| ReplayFault::Poison)
}

/// Reads the base event a replayed calendar patch, RSVP, or occurrence delete
/// applies to, by id, from the provider's event scope as the store last synced
/// it. `Ok(None)` when the event is absent or tombstoned there; a stored event
/// that does not decode is poison.
async fn event_base<P, S>(
    store: &S,
    provider: &P,
    account: &AccountId,
    event: &EventId,
) -> Result<Option<Event>, ReplayFault>
where
    P: Provider,
    S: Store + StoreRead,
{
    let payload = store
        .object_payload(&provider.event_scope(account), event.key())
        .await
        .map_err(ReplayFault::Store)?;
    payload
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| ReplayFault::Poison)
}
