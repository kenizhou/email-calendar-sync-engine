//! The invitation-answering write ([`Engine::rsvp_invitation`]) — the one
//! calendar write that starts from a **message**.
//!
//! Every other calendar write is event-addressed: the host read an event, it
//! edits or answers that event. An invitation in the mail is not that — it is
//! an iTIP payload inside an email, and on some transports (EAS
//! `MeetingResponse`) the *answer addresses the email*. So this facade composes
//! the scheduling read ([`Engine::message_scheduling`], `scheduling.rs`) with
//! the calendar write machinery (`calendar_writes.rs`): it parses the
//! invitation, matches the account's own attendee, locates the stored event
//! when the store holds one, and drives the outbox-mediated RSVP through
//! [`Provider::rsvp_event_from_invite`] — the verb a message-referencing
//! transport overrides and every event-answering transport inherits a correct
//! default for.
//!
//! Split from `calendar_writes.rs` (the 500-line module ceiling), whose
//! reconcile machinery this shares; the split follows `engine/mod.rs`'s
//! grouping rule — one write family per sibling module.

use core::time::Duration;

use engine_core::{
    calendar::{Event, ParticipantRole},
    ids::{AccountId, ProviderKey},
    mail::Message,
    scheduling::{ScheduleMethod, addresses_match},
    time::UtcDateTime,
};
use engine_provider::{EventRsvp, Provider, RsvpResponse};
use engine_recurrence::{Horizon, resolve_instant, resolve_instant_in};

use super::{CalendarWrite, LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

/// How far past an invitation's start the stored-event lookup scans for
/// occurrences. The window is a **recall aid, not the check** — `UID` equality
/// is the check — so it only has to cover where the stored copy of an
/// un-superseded invitation plausibly sits: at (or, for a recurring series,
/// starting at) the invitation's own start. A reschedule bumps `SEQUENCE`,
/// which the supersession gate below catches, so nothing turns on the window
/// being wider than this.
// `from_days` is the readable spelling but sits behind unstable
// `duration_constructors` on this toolchain; `from_mins` (the largest stable
// constructor — the `LEASE_TTL` precedent) is the honest equivalent.
#[allow(
    clippy::duration_suboptimal_units,
    reason = "from_days is unstable; from_mins is the largest stable constructor"
)]
fn lookahead() -> Duration {
    Duration::from_mins(14 * 24 * 60)
}

impl Engine {
    /// Answers the invitation `message` carries, through the durable outbox,
    /// then reconciles the store to the server's copy of the event — the
    /// invitation-shaped front door to the same write
    /// [`rsvp_calendar_event`](Self::rsvp_calendar_event) performs.
    ///
    /// The message must carry a `METHOD:REQUEST` iTIP payload with an
    /// `ATTENDEE` that is one of this account's addresses: `addresses` is the
    /// account's own address set (primary plus aliases — the engine does not
    /// know an account's identity, the host does), widened by the
    /// [`delivery_recipients`](crate::InboundScheduling::delivery_recipients)
    /// the scheduling read found, so an invitation to an alias nobody
    /// configured is still recognized and **answered as the address the
    /// organizer used**.
    ///
    /// The stored event the invitation's `UID` names is located and handed to
    /// the provider verb as `base`; when the store holds none (an EAS account
    /// can answer before any calendar sync), `base` is `None` and only a
    /// message-referencing transport can answer — exactly the split
    /// [`Provider::rsvp_event_from_invite`] exists to express.
    ///
    /// `comment` and `notify` map onto [`EventRsvp::comment`] and
    /// [`EventRsvp::notify_organizer`]; read
    /// [`Capabilities::calendar_rsvp`](engine_provider::Capabilities::calendar_rsvp)
    /// before offering either, as for [`rsvp_calendar_event`](Self::rsvp_calendar_event).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] before any write when the message is
    /// not an answerable invitation: no iTIP payload, a method other than
    /// `REQUEST` (a `REPLY` is someone else's answer, a `CANCEL` a
    /// withdrawal), no `ATTENDEE` of this account's, or an invitation whose
    /// stored event carries a **newer** `SEQUENCE` than the message (a later
    /// `REQUEST` already superseded it — re-read the latest one, never answer
    /// a copy the organizer has moved past). Otherwise as
    /// [`rsvp_calendar_event`](Self::rsvp_calendar_event).
    // One argument past the lint's taste. They do not fold: `addresses` and the
    // `message` are the *host's* facts (the account's identity set and the mail
    // it read), while `response`/`comment`/`notify` are the *answer* — the
    // brief's fixed interface, not a struct to be invented around.
    #[allow(
        clippy::too_many_arguments,
        reason = "the host's address set and invitation message beside the answer's three knobs"
    )]
    pub async fn rsvp_invitation<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        addresses: &[String],
        message: &Message,
        response: RsvpResponse,
        comment: Option<&str>,
        notify: bool,
    ) -> Result<CalendarWrite, ApiError> {
        let scheduling = self
            .message_scheduling(provider, account, message)
            .await?
            .ok_or_else(|| {
                ApiError::InvalidInput(
                    "the message carries no invitation — no text/calendar part it holds would \
                     parse"
                        .to_owned(),
                )
            })?;
        if scheduling.message.method != ScheduleMethod::Request {
            return Err(ApiError::InvalidInput(format!(
                "only a METHOD:REQUEST can be answered — this message carries METHOD:{}",
                scheduling.message.method
            )));
        }
        // The matched address is the invitation's own ATTENDEE spelling, not a
        // normalized rebuild of it: that is the line the server looks for.
        let attendee = matched_attendee(
            &scheduling.message.event,
            addresses,
            &scheduling.delivery_recipients,
        )
        .ok_or_else(|| {
            ApiError::InvalidInput(
                "no ATTENDEE on this invitation matches the account's addresses or the mailbox \
                     it was delivered to"
                    .to_owned(),
            )
        })?
        .to_owned();
        let base = self
            .stored_event_by_uid(account, &scheduling.message.event)
            .await?;
        if let Some(stored) = &base
            && stored.sequence > scheduling.message.event.sequence
        {
            return Err(ApiError::InvalidInput(format!(
                "the invitation is superseded — the stored event is at SEQUENCE {} while the \
                 message carries {}; a newer REQUEST has since arrived, answer that one",
                stored.sequence, scheduling.message.event.sequence
            )));
        }

        let mut rsvp = match &base {
            Some(base) => EventRsvp::to(base, attendee, response),
            // No stored event: the answer names the invitation's own event
            // identity (the parsed placeholder id, reconciled away by uid on
            // the next events pass) and guards on nothing — there is nothing
            // local to guard with.
            None => EventRsvp {
                event: scheduling.message.event.id.clone(),
                uid: scheduling.message.event.uid.clone(),
                attendee,
                response,
                comment: None,
                notify_organizer: true,
                guard: None,
            },
        };
        if let Some(comment) = comment {
            rsvp = rsvp.comment(comment);
        }
        if !notify {
            rsvp = rsvp.quietly();
        }

        // Unique per intent: the same invitation can be answered again after a
        // change of mind, and neither the message id nor the answer alone makes
        // two intents one (`enqueue_pending_op` dedups by this key across all
        // op states — a derived stable key would collapse the second answer
        // onto the first op).
        let idempotency = format!(
            "rsvp-invite:{}:{}",
            message.id.as_str(),
            uuid::Uuid::new_v4()
        );

        let write = engine_sync::rsvp_event_from_invite(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            &idempotency,
            message,
            base.as_ref(),
            &rsvp,
        )
        .await
        .map_err(map_sync_error)?;
        Ok(self.reconciling(provider, account, write).await)
    }

    /// The stored event carrying `invite`'s `UID`, located the only way the
    /// store allows — it has no `UID` index — so exactly the way the reference
    /// product does: occurrence rows over a window at the invitation's start,
    /// then the masters they point back at, then `UID` equality. `Ok(None)`
    /// when nothing matches (never an error: "no stored event" is a legitimate
    /// answer state, the one the message-referencing verb exists for).
    async fn stored_event_by_uid(
        &self,
        account: &AccountId,
        invite: &Event,
    ) -> Result<Option<Event>, ApiError> {
        let Some(anchor) = anchor_of(invite) else {
            return Ok(None);
        };
        let Some(end) = anchor.checked_add(lookahead()) else {
            return Ok(None);
        };
        // Unfailable in practice (`end` is strictly after `anchor`), but the
        // horizon constructor says what it says — surface rather than assume.
        let window = Horizon::new(anchor, end)
            .map_err(|e| ApiError::Store(engine_store::StoreError::Backend(e.to_string())))?;
        let occurrences = self.occurrences_in(account, window).await?;
        let mut keys: Vec<ProviderKey> = Vec::new();
        for occurrence in &occurrences {
            if !keys.contains(&occurrence.event) {
                keys.push(occurrence.event.clone());
            }
        }
        let events = self.events_by_keys(account, &keys).await?;
        Ok(events.into_iter().find(|stored| stored.uid == invite.uid))
    }
}

/// The invitation's start as an anchor instant: its own zone when it has one,
/// else UTC (a floating or all-day start has no zone; the window is a recall
/// aid that only needs to land within a day of the stored occurrence, which
/// `lookahead()` absorbs). `None` for a start no bundled zone can resolve.
fn anchor_of(invite: &Event) -> Option<UtcDateTime> {
    // A custom/VTIMEZONE zone that resolve_instant rejects falls through to the
    // UTC read (which resolves floating times) and then to None — a window that
    // cannot be placed answers "no stored event", never a wrong one.
    match resolve_instant(&invite.start) {
        Ok(instant) => instant,
        Err(_) => resolve_instant_in(&invite.start, &engine_core::time::TimeZoneId::utc()).ok(),
    }
}

/// The invitation `ATTENDEE` that is one of `mine`: the account's own
/// addresses widened by the mailbox's delivery recipients. Alias-aware and
/// case-insensitive through [`addresses_match`] — the same comparison the
/// trust decision makes, never `==`.
fn matched_attendee<'a>(
    invite: &'a Event,
    addresses: &[String],
    delivered_to: &[String],
) -> Option<&'a str> {
    invite
        .participants
        .iter()
        // The organizer is never "us" on a REQUEST we are being asked: an
        // ORGANIZER row carrying both roles merges with its ATTENDEE self on
        // parse, so a self-organized event still offers its attendee copy.
        .filter(|p| !p.has_role(&ParticipantRole::Owner))
        .filter_map(|p| p.email.as_deref())
        .find(|attendee| {
            addresses
                .iter()
                .chain(delivered_to)
                .any(|mine| addresses_match(attendee, mine))
        })
}
