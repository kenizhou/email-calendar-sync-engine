//! The invitation-answer write drivers — the calendar writes that start from
//! an invitation **message** rather than a stored event (P2 Task 4).
//!
//! `rsvp_event_from_invite` is the message-referencing twin of
//! [`rsvp_calendar_event`](super::calendar::rsvp_calendar_event): the transports
//! whose protocol answers by addressing the email (EAS `MeetingResponse`) override
//! the [`Provider::rsvp_event_from_invite`] verb it drives, and every
//! event-answering transport inherits the trait default — which requires the
//! stored base and delegates to `rsvp_event`, keeping this driver's outbox
//! semantics identical to the event-addressed one. Split from `calendar.rs`
//! (the 500-line ceiling).

use core::time::Duration;

use engine_core::{calendar::Event, ids::AccountId, mail::Message};
use engine_provider::{EventRsvp, EventWriteReceipt, Provider, ProviderError};
use engine_store::{Store, WorkerId};

use super::{
    InviteRef, OutboxIntent,
    calendar::{CalendarWriteOutcome, enqueue_calendar_op, resolve},
};
use crate::SyncError;

/// Answers an invitation through the outbox by referencing the invitation
/// **message**: durable op → claim → provider from-invite RSVP → record.
///
/// The message-referencing twin of [`rsvp_calendar_event`], for the transports whose
/// protocol answers from the email (EAS `MeetingResponse`): `invite` is the invitation
/// message, and `base` — the stored event, when the store holds one — is optional,
/// because that is the shape the verb exists for. The default trait verb refuses a
/// missing `base`, so a document transport keeps its exact `rsvp_calendar_event`
/// semantics through this driver too.
///
/// The durable payload records the answer plus the invite's addressing half
/// ([`InviteRef`]: its id and mailbox membership — a `Message` is serialize-only by
/// design), so a replay reconstructs exactly those and re-reads the base. Serialized on
/// the same `UID` resource key as every other calendar write.
///
/// # Errors
///
/// As [`rsvp_calendar_event`]; additionally an adapter keeping the trait default
/// refuses a `None` base as
/// [`InvalidState`](engine_core::error::InvalidState).
// One argument past the lint's taste — the same split as `rsvp_calendar_event`:
// the outbox's lease params, and the write's invite, base, and answer, which must
// stay separate (the durable payload records the intent, never the base).
#[allow(
    clippy::too_many_arguments,
    reason = "the outbox's lease params plus the write's invite, base, and intent, which must stay separate"
)]
pub async fn rsvp_event_from_invite<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    invite: &Message,
    base: Option<&Event>,
    rsvp: &EventRsvp,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let intent = OutboxIntent::RsvpEventFromInvite {
        rsvp: rsvp.clone(),
        invite: InviteRef {
            message: invite.id.clone(),
            mailboxes: invite.mailboxes.clone(),
        },
    };
    let leased =
        enqueue_calendar_op(store, account, worker, ttl, idempotency, &rsvp.uid, intent).await?;
    resolve(
        store,
        leased,
        execute_rsvp_event_from_invite(provider, account, invite, base, rsvp).await,
    )
    .await
}

/// Executes one claimed invitation answer by referencing the message: the provider
/// call the `rsvp_event_from_invite` verb names — `base` as the caller had it on
/// the inline path, re-read from the store on a replay (legitimately `None` for a
/// message-referencing transport: that is why the verb carries it as an option).
/// The execution half the inline driver runs and the calendar dispatcher
/// ([`execute_claimed_calendar`](super::execute::execute_claimed_calendar))
/// replays; outcome classification and recording stay with the caller.
pub(crate) async fn execute_rsvp_event_from_invite<P: Provider>(
    provider: &P,
    account: &AccountId,
    invite: &Message,
    base: Option<&Event>,
    rsvp: &EventRsvp,
) -> Result<EventWriteReceipt, ProviderError> {
    provider
        .rsvp_event_from_invite(account, invite, base, rsvp)
        .await
}
