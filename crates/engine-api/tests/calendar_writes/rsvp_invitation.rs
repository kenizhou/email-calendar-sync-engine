//! Answering an invitation FROM the message through the facade
//! ([`Engine::rsvp_invitation`]): the iTIP gate, the aliased attendee match,
//! the stored-event lookup by uid, the supersession refusal, and the
//! message-referencing verb a transport with no stored event answers through.
//!
//! The stored-event scenarios reuse the parent's stateful `CalendarServer` (the
//! default trait verb delegates to its `rsvp_event`); the no-stored-event
//! scenario carries its own small fake in the EAS shape — the invitation
//! message is the address, and no base is needed.

use std::sync::Mutex;

use engine_core::{
    calendar::Calendar,
    ids::{CalendarId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
    raw::RawMime,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EventWriteReceipt, Provider, ProviderError, ProviderResult,
    RsvpControls, RsvpResponse, ScopeSync, WriteGuard,
};

use super::*;

/// The invite email's provider id — the one fact a message-referencing
/// transport answers by.
const INVITE_MESSAGE_ID: &str = "imap:v1:u77@INBOX";

/// A `METHOD:REQUEST` invitation to the seeded event, delivered to the **alias**
/// (the account's own address appears nowhere in it), with a `SEQUENCE` the
/// caller can pass in so the supersession gate has something to refuse. The
/// attendee and the delivery header are separate knobs: matching either one is
/// the rule under test.
fn request_invite(sequence: u32, attendee: &str, delivered_to: &str) -> Vec<u8> {
    format!(
        "From: organizer@test.local\r\n\
         To: team@test.local\r\n\
         Delivered-To: {delivered_to}\r\n\
         Subject: Standup\r\n\
         Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
         --a\r\nContent-Type: text/plain\r\n\r\nWhen: 1 March\r\n\
         --a\r\nContent-Type: text/calendar; charset=\"utf-8\"; method=REQUEST\r\n\r\n\
         BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
         UID:evt-1@test.local\r\nDTSTAMP:20260201T080000Z\r\n\
         DTSTART:20260301T080000Z\r\nDTEND:20260301T083000Z\r\n\
         SUMMARY:Standup\r\nSEQUENCE:{sequence}\r\n\
         ORGANIZER;CN=Boss:mailto:organizer@test.local\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{attendee}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n\
         --a--\r\n"
    )
    .into_bytes()
}

/// The same body under any iTIP `METHOD` — everything but the answerable
/// `REQUEST` must be refused.
fn invitation_of_method(method: &str) -> Vec<u8> {
    let text = String::from_utf8(request_invite(0, ALIAS_ADDRESS, ALIAS_ADDRESS))
        .expect("fixture is utf-8");
    text.replace("method=REQUEST", &format!("method={method}"))
        .replace("METHOD:REQUEST", &format!("METHOD:{method}"))
        .into_bytes()
}

/// The invitation `Message` a host holds after a mail sync.
fn invite_message() -> Message {
    Message::new(
        MessageId::try_from(INVITE_MESSAGE_ID).unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

/// The account's own address set — deliberately not containing the alias, so a
/// passing scenario proves the delivery recipient matched.
fn own_addresses() -> Vec<String> {
    vec![SELF_ADDRESS.to_owned()]
}

// ---------------------------------------------------------------------------
// The stored-event path (the trait default delegating to rsvp_event)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_invitation_is_answered_as_the_alias_it_was_delivered_to() {
    // The host knows only its primary address; the invitation reached the
    // alias. The facade must match the ATTENDEE against the delivery
    // recipients, answer as that address, guard on the stored revision, and
    // reconcile — the whole outbox path, reached from the message alone.
    let server = CalendarServer::holding(seeded_event()).serving(
        INVITE_MESSAGE_ID,
        &request_invite(0, ALIAS_ADDRESS, ALIAS_ADDRESS),
    );
    let (engine, base) = synced(&server).await;

    let write = engine
        .rsvp_invitation(
            &server,
            &account(),
            &own_addresses(),
            &invite_message(),
            RsvpResponse::Accepted,
            None,
            true,
        )
        .await
        .expect("the invitation answers");
    assert!(
        matches!(write.reconciled, Reconciled::Applied(_)),
        "got {:?}",
        write.reconciled
    );

    // The exact intent the verb received: the matched alias (never the
    // account's primary), the answer, the stored event's identity and guard.
    let answers = server.answers();
    let rsvp = answers.last().expect("the event verb answered");
    assert_eq!(rsvp.attendee, ALIAS_ADDRESS);
    assert_eq!(rsvp.response, RsvpResponse::Accepted);
    assert!(rsvp.comment.is_none());
    assert!(rsvp.notify_organizer);
    assert_eq!(rsvp.event, base.id);
    assert_eq!(rsvp.uid, base.uid);
    assert_eq!(rsvp.guard.as_ref(), Some(&base.revisions));

    // And the store holds the server's answered copy the moment the call
    // returns — the read-your-writes contract every calendar write carries.
    let stored = engine.events(&account()).await.unwrap().remove(0);
    assert_eq!(
        status_of(&stored, ALIAS_ADDRESS),
        ParticipationStatus::Accepted
    );
}

#[tokio::test]
async fn a_stored_event_at_a_newer_sequence_refuses_the_answer() {
    // The organizer already sent a newer REQUEST than the one being answered:
    // answering the stale copy would commit the user to a meeting that no
    // longer exists as described. Refused before the verb, nothing recorded.
    let mut event = seeded_event();
    event.sequence = 2;
    let server = CalendarServer::holding(event).serving(
        INVITE_MESSAGE_ID,
        &request_invite(0, ALIAS_ADDRESS, ALIAS_ADDRESS),
    );
    let (engine, _base) = synced(&server).await;

    let refused = engine
        .rsvp_invitation(
            &server,
            &account(),
            &own_addresses(),
            &invite_message(),
            RsvpResponse::Accepted,
            None,
            true,
        )
        .await
        .expect_err("a superseded invitation must not be answered");
    assert!(matches!(refused, ApiError::InvalidInput(_)), "{refused}");
    assert!(
        format!("{refused}").contains("superseded"),
        "the refusal names the stale copy: {refused}"
    );
    assert!(
        server.answers().is_empty(),
        "nothing reached the answer verb"
    );
    assert_eq!(
        status_of(&engine.events(&account()).await.unwrap()[0], ALIAS_ADDRESS),
        ParticipationStatus::NeedsAction
    );
}

#[tokio::test]
async fn every_method_but_request_is_refused_with_the_method_named() {
    // A REPLY is somebody else's answer; a CANCEL is a withdrawal; a PUBLISH
    // asks for none. None of them is an invitation to answer, and the refusal
    // says which method arrived.
    for method in ["REPLY", "CANCEL", "PUBLISH"] {
        let server = CalendarServer::holding(seeded_event())
            .serving(INVITE_MESSAGE_ID, &invitation_of_method(method));
        let (engine, _base) = synced(&server).await;

        let refused = engine
            .rsvp_invitation(
                &server,
                &account(),
                &own_addresses(),
                &invite_message(),
                RsvpResponse::Accepted,
                None,
                true,
            )
            .await
            .expect_err("only a REQUEST is answerable");
        assert!(
            format!("{refused}").contains(&method.to_lowercase()),
            "the refusal names the method: {refused}"
        );
        assert!(server.answers().is_empty());
    }
}

#[tokio::test]
async fn a_message_with_no_attendee_of_ours_refuses_rather_than_guesses() {
    // An invitation delivered to somebody else's address must not be answered
    // as ours: the default attendee would be the account's primary identity,
    // which names an attendee the meeting does not have.
    let server = CalendarServer::holding(seeded_event()).serving(
        INVITE_MESSAGE_ID,
        &request_invite(0, "someone-else@test.local", ALIAS_ADDRESS),
    );
    let (engine, _base) = synced(&server).await;

    let refused = engine
        .rsvp_invitation(
            &server,
            &account(),
            &own_addresses(),
            &invite_message(),
            RsvpResponse::Accepted,
            None,
            true,
        )
        .await
        .expect_err("an invitation to someone else is not ours to answer");
    assert!(
        format!("{refused}").contains("no ATTENDEE"),
        "the refusal names what is missing: {refused}"
    );
    assert!(server.answers().is_empty());
}

#[tokio::test]
async fn a_message_that_carries_no_invitation_refuses() {
    // Ordinary mail has no iTIP payload; answering it is a category error the
    // facade reports rather than one it papers over with a default.
    let plain = b"From: a@test.local\r\nTo: b@test.local\r\n\
                  Content-Type: text/plain\r\n\r\njust mail\r\n";
    let server = CalendarServer::holding(seeded_event()).serving(INVITE_MESSAGE_ID, plain);
    let (engine, _base) = synced(&server).await;

    let refused = engine
        .rsvp_invitation(
            &server,
            &account(),
            &own_addresses(),
            &invite_message(),
            RsvpResponse::Accepted,
            None,
            true,
        )
        .await
        .expect_err("plain mail is not an invitation");
    assert!(
        format!("{refused}").contains("no invitation"),
        "the refusal says what the message lacks: {refused}"
    );
}

// ---------------------------------------------------------------------------
// The message-referencing path (a transport with no stored event — the EAS shape)
// ---------------------------------------------------------------------------

/// A fake in the EAS shape: it answers from the invitation message alone, so
/// `rsvp_event_from_invite` needs no base — and the event-addressed verb stays
/// unimplemented (its trait default would reject; this fake must never reach
/// it).
struct FromInviteServer {
    invite: Vec<u8>,
    answered: Mutex<Vec<(String, bool, String, RsvpResponse)>>,
    refused_event_verb: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for FromInviteServer {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Absent, OverrideSurvival::kept())
                .with_calendar_rsvp(RsvpControls {
                    comment: false,
                    suppress_notification: true,
                    guard: WriteGuard::Absent,
                }),
        )
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        let calendars = vec![Calendar::new(CalendarId::try_from("work").unwrap(), "Work")];
        let present = calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(calendars, present),
            SyncState::new("cal-1"),
        ))
    }

    /// An empty snapshot: this transport holds no stored copy of the event —
    /// the whole reason the message-referencing verb exists.
    async fn sync_events(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(Vec::new(), std::collections::BTreeSet::new()),
            SyncState::new("ev-1"),
        ))
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        if message.id.as_str() == INVITE_MESSAGE_ID {
            Ok(RawMime::new(self.invite.clone()))
        } else {
            Err(ProviderError::invalid_state("no such message source"))
        }
    }

    async fn rsvp_event_from_invite(
        &self,
        _account: &AccountId,
        invite: &Message,
        base: Option<&Event>,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        self.answered.lock().unwrap().push((
            invite.id.as_str().to_owned(),
            base.is_some(),
            rsvp.attendee.clone(),
            rsvp.response,
        ));
        // The EAS receipt shape: no server id, no revision — the next events
        // pass reconciles by uid.
        Ok(EventWriteReceipt::new(
            rsvp.event.clone(),
            rsvp.uid.clone(),
            RevisionTokens::default(),
        ))
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        // Unreachable from a correct facade: the from-invite verb is overridden.
        self.refused_event_verb.lock().unwrap().push(format!(
            "{}:{:?}",
            base.id.as_str(),
            rsvp.response
        ));
        Err(ProviderError::invalid_state("use rsvp_invitation"))
    }
}

#[tokio::test]
async fn an_answer_with_no_stored_event_goes_through_the_invite_referencing_verb() {
    // The EAS shape end to end: the store holds no event for the invitation's
    // uid (the server holds it, not the store), the facade still answers —
    // through the verb that addresses the message — and the answer carries the
    // alias the delivery headers matched.
    let server = FromInviteServer {
        invite: request_invite(0, ALIAS_ADDRESS, ALIAS_ADDRESS),
        answered: Mutex::default(),
        refused_event_verb: Mutex::default(),
    };
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_calendar(&server, &account(), horizon(), &host_zone())
        .await
        .unwrap();
    assert!(
        engine.events(&account()).await.unwrap().is_empty(),
        "the premise: no stored event at all"
    );

    let write = engine
        .rsvp_invitation(
            &server,
            &account(),
            &own_addresses(),
            &invite_message(),
            RsvpResponse::Declined,
            None,
            true,
        )
        .await
        .expect("the message-referencing verb answers with no stored event");

    assert_eq!(write.write.uid.as_str(), "evt-1@test.local");
    let answered = server.answered.lock().unwrap();
    assert_eq!(
        answered[0],
        (
            INVITE_MESSAGE_ID.to_owned(),
            false,
            ALIAS_ADDRESS.to_owned(),
            RsvpResponse::Declined
        ),
        "the verb received the invite, no base, and the matched alias"
    );
    assert!(
        server.refused_event_verb.lock().unwrap().is_empty(),
        "the event-addressed verb was never consulted"
    );
}
