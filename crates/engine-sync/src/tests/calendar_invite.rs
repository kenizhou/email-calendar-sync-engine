//! The from-invite RSVP driver's own tests — the message-referencing answer
//! with no stored event (the shape the verb exists for) and the base riding
//! beside the invite when the caller located one. Split from
//! `calendar_write.rs` (the 500-line ceiling); drivers and fakes are shared.

use super::{calendar_write::stored, *};

#[tokio::test]
async fn rsvp_event_from_invite_answers_with_no_stored_event_and_records_success() {
    // The verb exists for exactly this shape: the transport addresses the
    // invitation MESSAGE, so the store need not hold the event at all. The
    // driver enqueues the same durable op as the event-addressed RSVP, hands
    // the verb the invite and `None`, and records the receipt.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-11.ics", "evt-11@test.local");
    let invite = engine_core::mail::Message::new(
        MessageId::try_from("imap:v1:u42@INBOX").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    );

    let outcome = rsvp_event_from_invite(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "rsvp-invite:evt-11:accepted",
        &invite,
        None,
        &EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted),
    )
    .await
    .unwrap();

    assert_eq!(outcome.uid.as_str(), "evt-11@test.local");
    assert!(outcome.revisions.is_empty(), "the fake echoes no revision");
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert_eq!(
        *provider.invite_answers.lock().unwrap(),
        vec![(
            "imap:v1:u42@INBOX".to_owned(),
            false,
            "info@example.com".to_owned()
        )],
        "the verb received the invite message and no base"
    );
}

#[tokio::test]
async fn rsvp_event_from_invite_passes_the_stored_base_when_the_caller_had_one() {
    // A caller that located the stored event still hands it over: a
    // document transport's default answers from it, and the serialization key
    // stays the event's uid either way.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-12.ics", "evt-12@test.local");
    let invite = engine_core::mail::Message::new(
        MessageId::try_from("imap:v1:u43@INBOX").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    );

    rsvp_event_from_invite(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "rsvp-invite:evt-12:declined",
        &invite,
        Some(&base),
        &EventRsvp::to(&base, "me@example.com", RsvpResponse::Declined),
    )
    .await
    .unwrap();

    assert_eq!(
        provider.invite_answers.lock().unwrap()[0],
        (
            "imap:v1:u43@INBOX".to_owned(),
            true,
            "me@example.com".to_owned()
        ),
        "the base travels beside the invite"
    );
}
