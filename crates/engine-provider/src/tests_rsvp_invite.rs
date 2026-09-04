//! The invitation-RSVP trait verb's default behavior: the delegation to
//! `rsvp_event` with the stored base, and the refusal when there is none.
//! Split from `tests.rs` (the 500-line ceiling); the fixtures are shared.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
};

use super::{
    tests::{account, stored_event},
    *,
};

/// An event-answering adapter (the CalDAV/JMAP/Graph shape): it records what its
/// `rsvp_event` received, so the invite-verb default's delegation is observable.
#[derive(Default)]
struct EventAnsweringProvider {
    answered: std::sync::Mutex<Vec<(engine_core::calendar::Event, EventRsvp)>>,
}

#[async_trait]
impl Provider for EventAnsweringProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_calendars())
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        base: &engine_core::calendar::Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        self.answered
            .lock()
            .unwrap()
            .push((base.clone(), rsvp.clone()));
        Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            engine_core::version::RevisionTokens::default(),
        ))
    }
}

/// The invitation `Message` the default verb receives and ignores: addressing
/// only, exactly what a message-referencing transport needs from it.
fn invite_message() -> Message {
    Message::new(
        MessageId::try_from("imap:v1:u9@INBOX").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

#[tokio::test]
async fn the_invite_rsvp_default_delegates_to_the_event_verb_with_the_base() {
    // The default exists for the transports that answer from the stored event: it
    // must hand `rsvp_event` exactly the base it was given, never a synthesized
    // one — and the box must forward to the override, not answer some other way.
    let provider = EventAnsweringProvider::default();
    let boxed: Box<dyn Provider> = Box::new(EventAnsweringProvider::default());
    let base = stored_event();
    let rsvp = EventRsvp::to(&base, "info@test.local", crate::RsvpResponse::Accepted);

    for answered_by in ["direct", "boxed"] {
        let target = if answered_by == "direct" {
            &provider as &dyn Provider
        } else {
            boxed.as_ref()
        };
        let receipt = target
            .rsvp_event_from_invite(&account(), &invite_message(), Some(&base), &rsvp)
            .await
            .unwrap();
        assert_eq!(receipt.event, base.id, "{answered_by}");
        assert_eq!(receipt.uid, base.uid, "{answered_by}");
    }

    let answered = provider.answered.lock().unwrap();
    assert_eq!(answered.len(), 1, "the default delegates exactly once");
    assert_eq!(answered[0].0.id, base.id);
    assert_eq!(answered[0].1.attendee, "info@test.local");
}

#[tokio::test]
async fn the_invite_rsvp_default_refuses_without_a_stored_event() {
    // A transport that answers from the event has nothing to say when the store
    // holds none — a refusal, never a guess, and the box keeps the same shape.
    let provider = EventAnsweringProvider::default();
    let base = stored_event();
    let rsvp = EventRsvp::to(&base, "info@test.local", crate::RsvpResponse::Declined);

    let boxed: Box<dyn Provider> = Box::new(EventAnsweringProvider::default());
    for err in [
        provider
            .rsvp_event_from_invite(&account(), &invite_message(), None, &rsvp)
            .await
            .unwrap_err(),
        boxed
            .rsvp_event_from_invite(&account(), &invite_message(), None, &rsvp)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
        assert!(
            err.detail().contains("no stored event to answer"),
            "the refusal names what is missing: {}",
            err.detail()
        );
    }
    assert!(
        provider.answered.lock().unwrap().is_empty(),
        "nothing reached the event verb"
    );
}
