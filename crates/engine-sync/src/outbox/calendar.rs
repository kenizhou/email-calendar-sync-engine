//! The calendar write drivers: create, patch, replace-document, RSVP, delete.
//!
//! Each mirrors [`submit_mail`](super::submit_mail) — durable op → claim → provider call →
//! record — and each is provider-neutral: the driver never knows whether the adapter under
//! it will `PUT` an iCalendar document or post a JSCalendar patch.

use core::time::Duration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, EventId, Uid},
    version::RevisionTokens,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, EventWrite, EventWriteReceipt,
    PatchTarget, Provider, ProviderError, ReplyDelivery,
};
use engine_store::{LeasedPendingOp, Store, WorkerId};

use super::{OutboxIntent, enqueue_and_claim, record_failure};
use crate::SyncError;

/// The result of a successful calendar write through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarWriteOutcome {
    /// The durable op that recorded the write.
    pub op: PendingOpId,
    /// The event the write resolved to. For a **create** this is the id the server assigned
    /// (JMAP) or the href the adapter minted (CalDAV) — either way, the caller learns it
    /// here.
    pub event: EventId,
    /// The event's `UID`, echoed for sync-time reconciliation.
    pub uid: Uid,
    /// The revision the write's response reported, if any. Empty when the transport carries
    /// no per-object revision (JMAP) or the server returned none — the next sync then
    /// carries it.
    ///
    /// **Keep this if you are about to write again.** The store still holds the *pre-write*
    /// revision until the next sync reconciles it, so a second edit built from the store
    /// would guard on a superseded revision and be refused. Chaining writes off this
    /// receipt is what makes "edit, edit again" work.
    pub revisions: RevisionTokens,
    /// For an **RSVP**, what the server said about getting the answer to the organizer.
    ///
    /// [`ReplyDelivery::NotReported`] on every other verb, and on any transport that does
    /// not report. Silence is not success — see [`ReplyDelivery`].
    pub reply_delivery: ReplyDelivery,
}

/// Creates an event through the outbox: durable op → claim → provider create → record.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the create fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request cannot
/// be encoded or the just-enqueued op is not claimable.
pub async fn create_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    draft: &EventDraft,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &draft.uid,
        OutboxIntent::CreateEvent {
            draft: draft.clone(),
        },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_create_event(provider, account, draft).await,
    )
    .await
}

/// Applies an [`EventEdit`] to a stored event through the outbox.
///
/// `base` is the event **as the caller read it** — the document the patch applies to, and
/// the revision it is guarded by. The durable payload records the *edit*, not the document
/// it produces, so a conflict recovery can re-apply it to a freshly fetched base.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the patch fails (after recording it) — a stale guard
/// is a [`Conflict`](engine_core::error::FailureClass::Conflict), to be recovered by
/// re-syncing and re-applying, **never** by a blind retry. Returns [`SyncError::Store`] on
/// a store failure, or [`SyncError::Outbox`] if the request cannot be encoded or the
/// just-enqueued op is not claimable.
// One argument past the lint's taste. They do not fold: (worker, ttl) and the idempotency
// key belong to the *outbox*, while `base` and the intent belong to the *write* — and `base`
// cannot move into the `EventEdit`, because the durable payload must stay the intent alone
// (a retry re-applies it to a freshly fetched base, never to this one). A wrapper struct
// would rename the problem, not solve it.
#[allow(
    clippy::too_many_arguments,
    reason = "the outbox's lease params plus the write's base and intent, which must stay separate"
)]
pub async fn patch_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &Event,
    target: PatchTarget,
    patch: EventPatch,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let edit = EventEdit::new(base, target, patch);
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &edit.uid,
        OutboxIntent::PatchEvent { edit: edit.clone() },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_patch_event(provider, account, base, &edit).await,
    )
    .await
}

/// Replaces an event's whole stored document through the outbox — the iMIP RSVP path.
///
/// **Not** the neutral edit verb: prefer [`patch_calendar_event`]. This exists for the
/// operations that are naturally a finished document rather than a property patch, and only
/// a document-oriented adapter (CalDAV) supports it at all
/// ([`EventWrite`](engine_provider::EventWrite)).
///
/// # Errors
///
/// As [`patch_calendar_event`], plus
/// [`InvalidState`](engine_core::error::FailureClass::InvalidState) from an adapter with no
/// document verb.
pub async fn put_calendar_document<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    write: &EventWrite,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &write.uid,
        OutboxIntent::PutEventDoc {
            write: write.clone(),
        },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_put_event(provider, account, write).await,
    )
    .await
}

/// Answers an invitation through the outbox: durable op → claim → provider RSVP → record.
///
/// `base` is the event **as the caller read it**: the document the answer is written into on
/// a document transport, and the revision the write is guarded by. The durable payload
/// records the *answer*, not the document it produces, so a recovery retry re-applies it to
/// a freshly fetched base.
///
/// Serialized on the same `UID` resource key as every other calendar write, so an RSVP and
/// an edit of one event can never interleave.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the RSVP fails (after recording it) — an adapter that
/// cannot honour a requested control, or an event with no `ATTENDEE` at the answering
/// address, is an
/// [`InvalidState`](engine_core::error::FailureClass::InvalidState); a stale guard is a
/// [`Conflict`](engine_core::error::FailureClass::Conflict), to be recovered by re-syncing
/// and re-answering, **never** by a blind retry. Returns [`SyncError::Store`] on a store
/// failure, or [`SyncError::Outbox`] if the request cannot be encoded or the just-enqueued
/// op is not claimable.
// One argument past the lint's taste, and they do not fold — the same split as
// `patch_calendar_event`: (worker, ttl) and the idempotency key belong to the *outbox*,
// while `base` and the answer belong to the *write*. `base` cannot move into the
// `EventRsvp`, because the durable payload must stay the intent alone (a retry re-applies it
// to a freshly fetched base, never to this one).
#[allow(
    clippy::too_many_arguments,
    reason = "the outbox's lease params plus the write's base and intent, which must stay separate"
)]
pub async fn rsvp_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &Event,
    rsvp: &EventRsvp,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &rsvp.uid,
        OutboxIntent::RsvpEvent { rsvp: rsvp.clone() },
    )
    .await?;
    resolve(
        store,
        leased,
        execute_rsvp_event(provider, account, base, rsvp).await,
    )
    .await
}

/// Deletes an event — or one of its occurrences — through the outbox. Returns the durable op
/// id; the next sync tombstones the local row (or re-expands the series without that
/// occurrence).
///
/// `base` is the event **as the caller read it**, and only a
/// [`DeleteTarget::Occurrence`](engine_provider::DeleteTarget::Occurrence) needs it: removing
/// one occurrence is a rewrite of the series on a document transport, which runs over the
/// document the caller read. It stays out of the [`EventDeletion`] for the same reason
/// `patch_calendar_event`'s does — the durable payload records the *intent*, so a recovery
/// retry re-applies it to a freshly fetched base rather than to a stale one.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the delete fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request cannot
/// be encoded or the just-enqueued op is not claimable.
// One argument past the lint's taste, and they do not fold — the same split as
// `patch_calendar_event`: (worker, ttl) and the idempotency key belong to the *outbox*,
// while `base` and the deletion belong to the write.
#[allow(
    clippy::too_many_arguments,
    reason = "the outbox's lease params plus the write's base and intent, which must stay separate"
)]
pub async fn delete_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: Option<&Event>,
    deletion: &EventDeletion,
) -> Result<PendingOpId, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &deletion.uid,
        OutboxIntent::DeleteEvent {
            deletion: deletion.clone(),
        },
    )
    .await?;

    match execute_delete_event(provider, account, base, deletion).await {
        Ok(()) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: deletion.event.key().clone(),
                    },
                )
                .await?;
            Ok(leased.id)
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Records the outcome of a write that returns a receipt, under the op's lease.
pub(super) async fn resolve<S: Store>(
    store: &S,
    leased: LeasedPendingOp,
    result: engine_provider::ProviderResult<EventWriteReceipt>,
) -> Result<CalendarWriteOutcome, SyncError> {
    match result {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.event.key().clone(),
                    },
                )
                .await?;
            Ok(CalendarWriteOutcome {
                op: leased.id,
                event: receipt.event,
                uid: receipt.uid,
                revisions: receipt.revisions,
                reply_delivery: receipt.reply_delivery,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Executes one claimed event create: the provider call the
/// `create_calendar_event` verb names. The execution half the inline driver
/// runs and the calendar dispatcher
/// ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_create_event<P: Provider>(
    provider: &P,
    account: &AccountId,
    draft: &EventDraft,
) -> Result<EventWriteReceipt, ProviderError> {
    provider.create_event(account, draft).await
}

/// Executes one claimed event patch: the provider call the
/// `patch_calendar_event` verb names, applied to `base` — the event as the
/// caller read it on the inline path, re-read from the store on a replay (the
/// intent deliberately carries only the change and its target). The execution
/// half the inline driver runs and the calendar dispatcher
/// ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_patch_event<P: Provider>(
    provider: &P,
    account: &AccountId,
    base: &Event,
    edit: &EventEdit,
) -> Result<EventWriteReceipt, ProviderError> {
    provider.patch_event(account, base, edit).await
}

/// Executes one claimed document replace: the provider call the
/// `put_calendar_document` verb names — self-contained, the document is the
/// whole write, so nothing is re-read. The execution half the inline driver
/// runs and the calendar dispatcher
/// ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_put_event<P: Provider>(
    provider: &P,
    account: &AccountId,
    write: &EventWrite,
) -> Result<EventWriteReceipt, ProviderError> {
    provider.put_event(account, write).await
}

/// Executes one claimed invitation answer: the provider call the
/// `rsvp_calendar_event` verb names, written against `base` — the event as
/// the caller read it on the inline path, re-read from the store on a replay
/// (the intent deliberately carries only the answer). The execution half the
/// inline driver runs and the calendar dispatcher
/// ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_rsvp_event<P: Provider>(
    provider: &P,
    account: &AccountId,
    base: &Event,
    rsvp: &EventRsvp,
) -> Result<EventWriteReceipt, ProviderError> {
    provider.rsvp_event(account, base, rsvp).await
}

/// Executes one claimed event delete: the provider call the
/// `delete_calendar_event` verb names. `base` arrives exactly as on the
/// inline path — `None` for a series delete, which needs no document, and the
/// freshly re-read series document for an occurrence delete, whose rewrite
/// runs over it. The execution half the inline driver runs and the calendar
/// dispatcher ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_delete_event<P: Provider>(
    provider: &P,
    account: &AccountId,
    base: Option<&Event>,
    deletion: &EventDeletion,
) -> Result<(), ProviderError> {
    provider.delete_event(account, base, deletion).await
}

/// Builds and claims a calendar write op: the intent in a tagged envelope
/// under a caller-minted idempotency key, serialized on the event's `UID` so
/// writes to one event never race.
///
/// `idempotency` must be **unique per write intent** — the store dedups by `(account, key)`
/// across every op state, so a key derived only from the event would wrongly collapse two
/// distinct edits of it into one op. The `resource_key` is the `UID` rather than the
/// provider id, because the `UID` is the one identity that exists *before* a create has an
/// id and survives a transport that assigns its own — so a create and a follow-up edit of
/// the same event serialize against each other on either provider.
pub(super) async fn enqueue_calendar_op<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    uid: &Uid,
    intent: OutboxIntent,
) -> Result<LeasedPendingOp, SyncError> {
    let payload = serde_json::to_value(&intent)
        .map_err(|e| SyncError::Outbox(format!("encode calendar write: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("event:{}", uid.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, resource, payload),
    )
    .await
}
