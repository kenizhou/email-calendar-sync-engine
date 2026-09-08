//! The execution dispatchers: the three halves that run a claimed outbox op's
//! provider call from its tagged intent alone — one for mail verbs, one for
//! contact verbs, one for calendar verbs.
//!
//! Every inline driver is enqueue-and-claim plus its verb's `execute_*` half
//! plus a mark; these dispatchers are that middle step reached from the durable
//! record instead of from the caller's arguments — the half a drainer
//! ([`drain_mail_ops`](super::drain::drain_mail_ops) /
//! [`drain_contact_ops`](super::drain::drain_contact_ops) /
//! [`drain_calendar_ops`](super::drain::drain_calendar_ops)) replays a claimed op
//! through. They never record: the caller holds the lease and marks whatever
//! comes back.
//!
//! Why three: the drains split by provider surface (a mail drain needs only
//! [`Provider`]; a contact drain needs [`ContactsProvider`] for its verbs), and
//! the dispatch halves split the same way — a mail-only provider (IMAP) can
//! drain its mail ops without carrying a contacts surface it does not have.
//!
//! A calendar replay re-reads the base the base-dependent verbs need from the
//! store, by id — the freshly fetched base the intent contract promises a retry
//! (the intent carries the change, never the base it was read at). A patch or
//! RSVP whose event is gone resolves as the `Conflict` the provider verbs yield
//! for a dead target; an occurrence deletion whose event is gone completes,
//! because an occurrence of an absent event is already removed.

use engine_core::{
    calendar::Event,
    contact::ContactCard,
    error::FailureClass,
    ids::{AccountId, ContactId, EventId},
    mail::Message,
    write::PendingOutcome,
};
use engine_provider::{ContactsProvider, DeleteTarget, Provider};
use engine_store::{LeasedPendingOp, Store, StoreError, StoreRead};

use super::{
    OutboxIntent,
    calendar::{
        execute_create_event, execute_delete_event, execute_patch_event, execute_put_event,
        execute_rsvp_event,
    },
    contact::{execute_create_contact, execute_delete_contact, execute_patch_contact},
    invite::execute_rsvp_event_from_invite,
    mail::{execute_edit_mail, execute_report_message, execute_submit_mail, send_failure_outcome},
    write_failure_outcome,
};

/// Why a claimed op produced no outcome — the structured discrimination a drain
/// loop needs and a flat error string cannot carry. The three cases take three
/// different accountings, so nothing may match on an error *message* to tell
/// them apart.
#[derive(Debug)]
pub(crate) enum ExecuteFailure {
    /// The op is **terminal poison**: its payload does not decode as a tagged
    /// [`OutboxIntent`] (or a stored base card a contact replay re-reads does
    /// not decode). No execution exists and none ever will, so the drain marks
    /// the op terminally `Failed` rather than letting its lease expire back
    /// into runnable forever. Carries the decode detail.
    ///
    /// The detail is the discrimination payload a host (and the tests) read to
    /// explain a poison mark; the lib itself cannot read it yet, because
    /// `PendingOutcome::Failed` carries no detail and outcome persistence is
    /// Phase 2 — when that lands, the detail flows into the mark and this
    /// allowance goes.
    #[allow(
        dead_code,
        reason = "read only by hosts/tests until outcome persistence lands"
    )]
    Undecodable(String),
    /// The op's verb belongs to another drain's scope (a contact verb in the
    /// mail or calendar drain; a mail verb in the contact or calendar drain;
    /// a calendar verb in the mail or contact drain). The caller skips the op
    /// unmarked and releases its lease back to `Pending` — the fencing token
    /// is bumped, so this lease is dead — and the right executor claims the
    /// op immediately: a skip costs its claim slot, never a lease TTL.
    OutOfScope,
    /// The store read a replay needed (a contact patch/delete's base card)
    /// failed — transient; the caller surfaces it, and the lease's expiry
    /// recycles the op for a later round.
    Store(StoreError),
}

/// Executes one claimed **mail** op — `submit_mail`, `edit_mail`, or
/// `report_message` — with exactly the inline drivers' semantics, including
/// parking an ambiguous send as `NeedsConfirmation` rather than a plain
/// failure, so a replay can never blind-retry one (`providers.md`). Returns the
/// outcome for the caller to record under the lease; does not touch the store's
/// op state itself.
///
/// Every other verb is [`ExecuteFailure::OutOfScope`]: contact verbs belong to
/// [`execute_claimed_contact`], calendar verbs to
/// [`execute_claimed_calendar`].
pub(crate) async fn execute_claimed_mail<P>(
    provider: &P,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, ExecuteFailure>
where
    P: Provider,
{
    match decode_intent(leased)? {
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
        OutboxIntent::CreateContact { .. }
        | OutboxIntent::PatchContact { .. }
        | OutboxIntent::DeleteContact { .. }
        | OutboxIntent::CreateEvent { .. }
        | OutboxIntent::PatchEvent { .. }
        | OutboxIntent::PutEventDoc { .. }
        | OutboxIntent::RsvpEvent { .. }
        | OutboxIntent::RsvpEventFromInvite { .. }
        | OutboxIntent::DeleteEvent { .. } => Err(ExecuteFailure::OutOfScope),
    }
}

/// Executes one claimed **contact** op — `create_contact`, `patch_contact`, or
/// `delete_contact` — with exactly the inline drivers' semantics. Returns the
/// outcome for the caller to record under the lease; does not touch the store's
/// op state itself.
///
/// Patch and delete re-read the base card the intent targets from the store
/// (the intent deliberately carries only the change; the inline path already
/// holds the caller's base). A patch whose card is already gone resolves as the
/// `Conflict` the provider verbs yield for a dead target — terminal, corrected
/// by the next sync, never retried into success — while a delete whose card is
/// gone completes with the success the verbs themselves grant an already-absent
/// card.
///
/// Every other verb is [`ExecuteFailure::OutOfScope`]: mail verbs belong to
/// [`execute_claimed_mail`], calendar verbs to
/// [`execute_claimed_calendar`].
pub(crate) async fn execute_claimed_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, ExecuteFailure>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    match decode_intent(leased)? {
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
        OutboxIntent::SubmitMail { .. }
        | OutboxIntent::EditMail { .. }
        | OutboxIntent::ReportMessage { .. }
        | OutboxIntent::CreateEvent { .. }
        | OutboxIntent::PatchEvent { .. }
        | OutboxIntent::PutEventDoc { .. }
        | OutboxIntent::RsvpEvent { .. }
        | OutboxIntent::RsvpEventFromInvite { .. }
        | OutboxIntent::DeleteEvent { .. } => Err(ExecuteFailure::OutOfScope),
    }
}

/// Executes one claimed **calendar** op — `create_calendar_event`,
/// `patch_calendar_event`, `put_calendar_document`, `rsvp_calendar_event`,
/// `rsvp_event_from_invite`, or `delete_calendar_event` — with exactly the
/// inline drivers' semantics. Returns the outcome for the caller to record
/// under the lease; does not touch the store's op state itself.
///
/// Patch, RSVP, and an occurrence delete re-read the base event the intent
/// targets from the store (the intent deliberately carries only the change;
/// the inline path already holds the caller's base). A patch or RSVP whose
/// event is already gone resolves as the `Conflict` the provider verbs yield
/// for a dead target — terminal, corrected by the next sync, never retried
/// into success — while an occurrence delete whose event is gone completes,
/// because an occurrence of an absent event is already removed. A series
/// delete, a document replace, and a from-invite answer need no base at all:
/// the last answers from the invitation message, so a re-read that finds no
/// stored event still executes against the transports that address the email.
///
/// Every other verb is [`ExecuteFailure::OutOfScope`]: mail verbs belong to
/// [`execute_claimed_mail`], contact verbs to
/// [`execute_claimed_contact`].
pub(crate) async fn execute_claimed_calendar<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
) -> Result<PendingOutcome, ExecuteFailure>
where
    P: Provider,
    S: Store + StoreRead,
{
    match decode_intent(leased)? {
        OutboxIntent::CreateEvent { draft } => Ok(
            match execute_create_event(provider, account, &draft).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.event.key().clone(),
                },
                Err(err) => write_failure_outcome(&err),
            },
        ),
        OutboxIntent::PatchEvent { edit } => Ok(
            match event_base(store, provider, account, &edit.event).await? {
                Some(base) => match execute_patch_event(provider, account, &base, &edit).await {
                    Ok(receipt) => PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                },
                None => PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                },
            },
        ),
        OutboxIntent::PutEventDoc { write } => {
            Ok(match execute_put_event(provider, account, &write).await {
                Ok(receipt) => PendingOutcome::Succeeded {
                    provider_key: receipt.event.key().clone(),
                },
                Err(err) => write_failure_outcome(&err),
            })
        }
        OutboxIntent::RsvpEvent { rsvp } => Ok(
            match event_base(store, provider, account, &rsvp.event).await? {
                Some(base) => match execute_rsvp_event(provider, account, &base, &rsvp).await {
                    Ok(receipt) => PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                },
                None => PendingOutcome::Failed {
                    class: FailureClass::Conflict,
                    retry_after: None,
                },
            },
        ),
        // The from-invite answer is the one calendar verb whose replay runs
        // without a base: a message-referencing transport answers from the
        // email alone, so a re-read that finds none still executes — only the
        // document transports' default refuses, as it would inline.
        OutboxIntent::RsvpEventFromInvite { rsvp, invite } => {
            let base = event_base(store, provider, account, &rsvp.event).await?;
            let message = Message::new(invite.message.clone(), invite.mailboxes.clone());
            Ok(
                match execute_rsvp_event_from_invite(
                    provider,
                    account,
                    &message,
                    base.as_ref(),
                    &rsvp,
                )
                .await
                {
                    Ok(receipt) => PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                },
            )
        }
        OutboxIntent::DeleteEvent { deletion } => Ok(match &deletion.target {
            DeleteTarget::Series => {
                match execute_delete_event(provider, account, None, &deletion).await {
                    Ok(()) => PendingOutcome::Succeeded {
                        provider_key: deletion.event.key().clone(),
                    },
                    Err(err) => write_failure_outcome(&err),
                }
            }
            DeleteTarget::Occurrence { .. } => {
                match event_base(store, provider, account, &deletion.event).await? {
                    Some(base) => {
                        match execute_delete_event(provider, account, Some(&base), &deletion).await
                        {
                            Ok(()) => PendingOutcome::Succeeded {
                                provider_key: deletion.event.key().clone(),
                            },
                            Err(err) => write_failure_outcome(&err),
                        }
                    }
                    None => PendingOutcome::Succeeded {
                        provider_key: deletion.event.key().clone(),
                    },
                }
            }
        }),
        OutboxIntent::SubmitMail { .. }
        | OutboxIntent::EditMail { .. }
        | OutboxIntent::ReportMessage { .. }
        | OutboxIntent::CreateContact { .. }
        | OutboxIntent::PatchContact { .. }
        | OutboxIntent::DeleteContact { .. } => Err(ExecuteFailure::OutOfScope),
    }
}

/// Decodes a claimed op's payload into its tagged intent — the one decode both
/// dispatchers dispatch on. An undecodable payload (including an unknown
/// `verb`, which fails to decode rather than decoding as a silent no-op) is
/// terminal poison, reported with the op's id and the decode error.
fn decode_intent(leased: &LeasedPendingOp) -> Result<OutboxIntent, ExecuteFailure> {
    serde_json::from_value(leased.op.payload.clone()).map_err(|e| {
        ExecuteFailure::Undecodable(format!(
            "undecodable payload for op {}: {e}",
            leased.id.get()
        ))
    })
}

/// Reads the base card a replayed contact patch or delete applies to, by id,
/// from the provider's card scope as the store last synced it.
///
/// `Ok(None)` when the card is absent or tombstoned there. The inline drivers
/// never come here — they hold the caller's base; only a replay, whose sole
/// record is the intent, re-reads one. A stored card that does not decode is
/// poison for the same reason an undecodable payload is: no execution of this
/// intent exists until a re-sync rewrites the card.
async fn contact_base<P, S>(
    store: &S,
    provider: &P,
    account: &AccountId,
    contact: &ContactId,
) -> Result<Option<ContactCard>, ExecuteFailure>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let payload = store
        .object_payload(&provider.contact_scope(account), contact.key())
        .await
        .map_err(ExecuteFailure::Store)?;
    payload
        .map(|value| {
            serde_json::from_value(value).map_err(|e| {
                ExecuteFailure::Undecodable(format!(
                    "undecodable stored card {}: {e}",
                    contact.as_str()
                ))
            })
        })
        .transpose()
}

/// Reads the base event a replayed calendar patch, RSVP, or occurrence delete
/// applies to, by id, from the provider's event scope as the store last synced
/// it.
///
/// `Ok(None)` when the event is absent or tombstoned there. The inline drivers
/// never come here — they hold the caller's base; only a replay, whose sole
/// record is the intent, re-reads one. A stored event that does not decode is
/// poison for the same reason an undecodable payload is: no execution of this
/// intent exists until a re-sync rewrites the event.
async fn event_base<P, S>(
    store: &S,
    provider: &P,
    account: &AccountId,
    event: &EventId,
) -> Result<Option<Event>, ExecuteFailure>
where
    P: Provider,
    S: Store + StoreRead,
{
    let payload = store
        .object_payload(&provider.event_scope(account), event.key())
        .await
        .map_err(ExecuteFailure::Store)?;
    payload
        .map(|value| {
            serde_json::from_value(value).map_err(|e| {
                ExecuteFailure::Undecodable(format!(
                    "undecodable stored event {}: {e}",
                    event.as_str()
                ))
            })
        })
        .transpose()
}
