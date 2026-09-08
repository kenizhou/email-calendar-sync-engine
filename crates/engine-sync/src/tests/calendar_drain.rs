//! The calendar drain loop's own tests: the third drain's verbs replayed
//! exactly as the inline drivers resolve them. The founding case is the
//! crash-orphaned create (self-contained — nothing re-read); the pins are the
//! base re-read semantics — a patch or RSVP replays against the freshly
//! stored base and resolves as a terminal `Conflict` when the event is gone,
//! while an occurrence delete of a gone event completes, and a series delete
//! needs no base at all.

use engine_core::time::UtcDateTime;
use engine_provider::{DeleteTarget, Occurrence};

use super::{
    drain::{at, crash_orphan, enqueue_op, stored_event, ttl},
    *,
};
use crate::outbox::drain::drain_calendar_ops;

/// One calendar drain round: the calendar loop under the shared fixtures.
async fn drain_calendar(
    provider: &FakeMail,
    store: &SqliteStore<ManualClock>,
) -> Result<usize, crate::SyncError> {
    drain_calendar_ops(provider, store, &account(), worker(), ttl(), 16).await
}

/// The revision stamp every calendar write intent in these tests carries.
fn stamp() -> UtcDateTime {
    "2026-08-01T10:00:00Z".parse().unwrap()
}

fn event_draft(uid: &str) -> EventDraft {
    EventDraft::new(
        CalendarId::new(key("/cal/default/")),
        Uid::new(uid).unwrap(),
        "Sprint planning",
        at(9),
        at(10),
        stamp(),
    )
}

/// Seeds the store with the one stored event the base-dependent replays target:
/// a full calendar sync of the fake's snapshot, so the event's object payload —
/// the base a replay re-reads — is exactly what sync recorded.
async fn seed_stored_event(provider: &FakeMail, store: &SqliteStore<ManualClock>) {
    sync_calendar(
        provider,
        store,
        &account(),
        worker(),
        ttl(),
        Horizon::new(
            "2026-01-01T00:00:00Z".parse().unwrap(),
            "2026-12-31T00:00:00Z".parse().unwrap(),
        )
        .unwrap(),
        &TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_crash_orphaned_event_create_drains_to_succeeded() {
    // The founding calendar case: a create intent is self-contained (the draft
    // is the whole write), so the drain replays it with nothing re-read and
    // records the receipt's event key, exactly as the inline driver resolves it.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = crash_orphan(
        &store,
        &clock,
        "drain:calendar:create",
        "event:evt-9@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: event_draft("evt-9@test.local"),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the orphaned create was driven to an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_provider_failure_drains_to_a_counted_terminal_failed() {
    // The provider-failure arm every calendar verb's replay shares: the drain
    // executes the create, the provider refuses it (the WriteGuard fault — a
    // classified conflict), and the outcome is a terminal Failed — counted as
    // an outcome, never re-claimed, exactly as the inline driver records the
    // same refusal. Only a foreign-scope skip or a failed store read recycles
    // through the lease; a provider failure does not.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::WriteGuard);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:calendar:create-refused",
        "event:evt-9@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: event_draft("evt-9@test.local"),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the terminal Failed mark is an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );

    let again = drain_calendar(&provider, &store).await.unwrap();
    assert_eq!(again, 0, "a terminally Failed op is never re-claimed");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn a_crash_orphaned_patch_replays_against_the_stored_base() {
    // The base the intent deliberately does not carry: the replay re-reads the
    // event from the store — the freshly fetched base the intent contract
    // promises a retry gets — and applies the edit to that.
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![Calendar::new(
            CalendarId::new(key("/cal/default/")),
            "Calendar",
        )],
        vec![stored_event()],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    seed_stored_event(&provider, &store).await;
    let edit = EventEdit::new(
        &stored_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let op = enqueue_op(
        &store,
        "drain:calendar:patch",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::PatchEvent { edit }).unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the replay applied against the stored base");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_crash_orphaned_rsvp_replays_against_the_stored_base() {
    // The RSVP is the same shape as the patch: the answer names the event by
    // id, and the replay re-reads the base to answer against.
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![Calendar::new(
            CalendarId::new(key("/cal/default/")),
            "Calendar",
        )],
        vec![stored_event()],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    seed_stored_event(&provider, &store).await;
    let rsvp = EventRsvp::to(&stored_event(), "alice@test.local", RsvpResponse::Accepted);
    let op = enqueue_op(
        &store,
        "drain:calendar:rsvp",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::RsvpEvent { rsvp }).unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_patch_whose_event_is_gone_fails_as_conflict() {
    // A replayed patch whose base is gone from the store resolves as the
    // Conflict the provider verbs yield for a dead target — terminal, corrected
    // by the next calendar sync, never retried into success (the contacts
    // patch's exact semantics).
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let edit = EventEdit::new(
        &stored_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let op = enqueue_op(
        &store,
        "drain:calendar:patch-gone",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::PatchEvent { edit }).unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the terminal Failed mark is an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn a_series_delete_replays_without_a_stored_base() {
    // A series deletion needs no base (the inline driver's own contract: only
    // an occurrence deletion rewrites a document), so the replay runs with no
    // re-read at all — proven here by succeeding over a store that never held
    // the event.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:calendar:delete-series",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::DeleteEvent {
            deletion: EventDeletion::of(&stored_event()),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn an_occurrence_delete_replays_against_the_stored_base() {
    // An occurrence deletion is a series rewrite on a document transport, so it
    // is the one deletion that needs the base — re-read from the store here.
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![Calendar::new(
            CalendarId::new(key("/cal/default/")),
            "Calendar",
        )],
        vec![stored_event()],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    seed_stored_event(&provider, &store).await;
    let mut deletion = EventDeletion::of(&stored_event());
    deletion.target = DeleteTarget::Occurrence {
        occurrence: Occurrence::starting(at(9)),
        stamp: stamp(),
    };
    let op = enqueue_op(
        &store,
        "drain:calendar:delete-occurrence",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::DeleteEvent { deletion }).unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn an_occurrence_delete_whose_event_is_gone_completes() {
    // The gone-event mirror of the contacts delete: an occurrence of an event
    // that no longer exists is already removed, so the replay completes with
    // the success the verbs grant an absent target rather than failing.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let mut deletion = EventDeletion::of(&stored_event());
    deletion.target = DeleteTarget::Occurrence {
        occurrence: Occurrence::starting(at(9)),
        stamp: stamp(),
    };
    let op = enqueue_op(
        &store,
        "drain:calendar:delete-occurrence-gone",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::DeleteEvent { deletion }).unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_crash_orphaned_invite_rsvp_replays_without_a_stored_event() {
    // The from-invite answer is the one calendar verb whose replay needs no
    // base: the message-referencing transports answer even when the store
    // holds no event, so a replay re-reads the base, finds none, and the verb
    // still runs — resolving Succeeded, not the Conflict the event-addressed
    // RSVP resolves as.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let rsvp = EventRsvp::to(&stored_event(), "alice@test.local", RsvpResponse::Accepted);
    let op = enqueue_op(
        &store,
        "drain:calendar:rsvp-invite",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::RsvpEventFromInvite {
            rsvp,
            invite: crate::outbox::InviteRef {
                message: engine_core::ids::MessageId::try_from("imap:v1:u42@INBOX").unwrap(),
                mailboxes: engine_core::membership::Memberships::of_one(
                    engine_core::ids::MailboxId::try_from("INBOX").unwrap(),
                ),
            },
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert_eq!(
        provider.invite_answers.lock().unwrap()[0],
        (
            "imap:v1:u42@INBOX".to_owned(),
            false,
            "alice@test.local".to_owned()
        ),
        "the replay reconstructed the invite's addressing half and passed no base"
    );
}
