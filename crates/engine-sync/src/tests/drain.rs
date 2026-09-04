//! Drain-loop tests: the background counterpart of the inline drivers — claim a
//! batch of runnable ops, execute each through the dispatch halves, settle each
//! under its lease. The founding case is the crash orphan (an op an inline
//! worker claimed and died holding, its lease expired); the pins are that an
//! ambiguous send parks as `NeedsConfirmation` and never recycles, poison is
//! terminally `Failed`, and a foreign-scope verb is skipped unmarked at the
//! documented cost of one lease TTL.

use engine_core::{
    calendar::Event,
    contact::{ContactCard, ContactDraft},
    ids::{AddressBookId, CalendarId, ContactId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, LocalDateTime},
    write::{PendingOpId, PendingOutcome},
};
use engine_store::PendingOpState;

use super::*;
use crate::outbox::drain::{drain_contact_ops, drain_mail_ops, settle_claimed};

/// The lease the tests arm — long enough to span a claim, short enough that a
/// two-minute advance expires it.
pub(super) fn ttl() -> Duration {
    Duration::from_mins(1)
}

/// Enqueues one op and returns its id — an unstarted (`Pending`) op exactly as
/// the inline drivers' enqueue half leaves it.
pub(super) async fn enqueue_op(
    store: &SqliteStore<ManualClock>,
    idempotency: &str,
    resource: &str,
    payload: serde_json::Value,
) -> PendingOpId {
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(idempotency).unwrap(),
                ResourceKey::new(resource).unwrap(),
                payload,
            ),
        )
        .await
        .unwrap()
}

/// Hand-builds the drainer's founding case: an op an inline worker claimed and
/// then died holding, its lease long expired. The clock advances past the
/// lease, so the next claim — the drain's — finds it runnable again.
pub(super) async fn crash_orphan(
    store: &SqliteStore<ManualClock>,
    clock: &ManualClock,
    idempotency: &str,
    resource: &str,
    payload: serde_json::Value,
) -> PendingOpId {
    let op = enqueue_op(store, idempotency, resource, payload).await;
    store
        .claim_pending_ops(account(), LeaseRequest::new(worker(), ttl()), 16)
        .await
        .unwrap();
    clock.advance(Duration::from_mins(2));
    op
}

/// One drain round: the mail loop under the shared fixtures.
async fn drain_mail(
    provider: &FakeMail,
    store: &SqliteStore<ManualClock>,
) -> Result<usize, crate::SyncError> {
    drain_mail_ops(provider, store, &account(), worker(), ttl(), 16).await
}

#[tokio::test]
async fn a_crash_orphaned_submit_is_re_driven_to_succeeded() {
    // The founding case: the inline worker died between claim and mark, the
    // lease expired, and the drain re-drives the send to the same Succeeded
    // the inline path would have recorded.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = crash_orphan(
        &store,
        &clock,
        "drain:submit:orphan",
        "draft:send-1@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(draft("send-1@test.local")),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the orphan was driven to an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn an_ambiguous_re_drive_parks_needs_confirmation_and_never_recycles() {
    // The never-blind-retry red line, driven through the loop: a replayed send
    // whose provider ack was lost parks as NeedsConfirmation exactly as the
    // inline driver parks it, and a parked op is not claimable — a later round
    // must not touch it, let alone re-send.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = crash_orphan(
        &store,
        &clock,
        "drain:submit:ambiguous",
        "draft:send-2@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(draft("send-2@test.local")),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "parking is an outcome — counted, not skipped");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );

    let again = drain_mail(&provider, &store).await.unwrap();
    assert_eq!(again, 0, "a parked op never returns to the runnable set");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
}

#[tokio::test]
async fn an_undecodable_payload_is_terminally_failed_and_never_reclaimed() {
    // Poison: a payload that does not decode as a tagged intent can never
    // execute, so the drain marks it terminally Failed instead of letting the
    // lease expire recycle it forever. A second round must find nothing to do.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:poison",
        "mail:poison",
        serde_json::Value::Null,
    )
    .await;

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the terminal Failed mark is an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );

    let again = drain_mail(&provider, &store).await.unwrap();
    assert_eq!(again, 0);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed),
        "a terminally Failed op is never re-claimed"
    );
}

#[tokio::test]
async fn a_calendar_op_in_the_mail_drain_is_skipped_unmarked() {
    // The mail drain cannot execute a calendar verb (its provider may carry no
    // calendar surface), so it claims the op (claims are scope-blind) and leaves
    // it unmarked — InFlight under the drain's lease until it expires, the
    // documented one-TTL cost. The calendar drain is the op's executor once the
    // lease recycles it.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:calendar",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::DeleteEvent {
            deletion: EventDeletion::of(&stored_event()),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 0, "a foreign-scope op is not counted");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::InFlight),
        "skipped unmarked: lease-held until expiry"
    );
}

#[tokio::test]
async fn a_contact_verb_in_the_mail_drain_is_skipped_unmarked() {
    // The same skip for the contacts half of the split: the mail drain cannot
    // execute a contact verb (its provider may carry no contacts surface), so
    // it claims, skips, and leaves the op to its lease expiry.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:contact-verb",
        "contact:card-1",
        serde_json::to_value(OutboxIntent::CreateContact {
            draft: contact_draft(),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 0);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::InFlight)
    );
}

#[tokio::test]
async fn a_mail_verb_in_the_contact_drain_is_skipped_unmarked() {
    // The mirror: the contact drain returns the favor for a mail verb.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:mail-verb",
        "mail:msg-1",
        serde_json::to_value(OutboxIntent::EditMail {
            edit: MailEdit::mark_seen(key("msg-1"), true),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_contact_ops(&provider, &store, &account(), worker(), ttl(), 16)
        .await
        .unwrap();

    assert_eq!(drained, 0);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::InFlight)
    );
}

#[tokio::test]
async fn an_empty_outbox_drains_to_zero() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let drained = drain_mail(&provider, &store).await.unwrap();

    assert_eq!(drained, 0);
}

#[tokio::test]
async fn a_stale_mark_drops_the_op_without_error_or_count() {
    // Another worker re-claimed the op underneath (the lease expired, the
    // second claim superseded the token): the first worker's mark is rejected
    // as StaleLease and the result is dropped silently — no error, no count.
    // The op belongs to whoever holds the fresh lease now.
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        "drain:stale",
        "draft:send-3@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(draft("send-3@test.local")),
        })
        .unwrap(),
    )
    .await;
    let stale = store
        .claim_pending_ops(account(), LeaseRequest::new(worker(), ttl()), 16)
        .await
        .unwrap()
        .into_iter()
        .find(|leased| leased.id == op)
        .expect("the just-enqueued op is claimable");
    clock.advance(Duration::from_mins(2));
    store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(WorkerId::new("w-2"), ttl()),
            16,
        )
        .await
        .unwrap();

    let driven = settle_claimed(
        &store,
        &stale,
        Ok(PendingOutcome::Succeeded {
            provider_key: key("sent-1"),
        }),
    )
    .await
    .unwrap();

    assert!(
        !driven,
        "a stolen op's result is not this worker's to count"
    );
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::InFlight),
        "the re-claiming worker owns the op now"
    );
}

#[tokio::test]
async fn a_contact_create_orphan_drains_to_succeeded_through_the_provider() {
    // The contacts loop drives its own verbs end to end: the fake's create
    // returns a receipt, the drain marks the op Succeeded under its lease.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = crash_orphan(
        &store,
        &clock,
        "drain:contact:create",
        "contact-create:personal",
        serde_json::to_value(OutboxIntent::CreateContact {
            draft: contact_draft(),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_contact_ops(&provider, &store, &account(), worker(), ttl(), 16)
        .await
        .unwrap();

    assert_eq!(drained, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// The create intent's draft — one card in the personal book.
fn contact_draft() -> ContactDraft {
    ContactDraft {
        address_book: AddressBookId::try_from("personal").unwrap(),
        card: ContactCard::new(
            ContactId::try_from("card-1").unwrap(),
            Memberships::of_one(AddressBookId::try_from("personal").unwrap()),
        ),
    }
}

/// A stored event just complete enough for an `EventDeletion` to target.
pub(super) fn stored_event() -> Event {
    Event::new(
        EventId::try_from("/cal/default/evt-1.ics").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        Memberships::of_one(CalendarId::new(key("/cal/default/"))),
        at(9),
    )
}

pub(super) fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(
        format!("2026-08-01T{hour:02}:00:00")
            .parse::<LocalDateTime>()
            .unwrap(),
    )
}
