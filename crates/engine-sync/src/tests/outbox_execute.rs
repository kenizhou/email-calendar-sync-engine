//! Direct execute-half tests: the dispatch entry a drainer replays a claimed op
//! through, exercised with no inline driver above it. A hand-enqueued,
//! hand-claimed op plus the shared fakes reproduce the drainer's exact inputs —
//! an ambiguous send parks as `NeedsConfirmation`, a calendar op is refused
//! without any accounting, an undecodable payload is a terminal error, and a
//! contact delete whose base card is already gone from the store completes
//! idempotently.

use engine_core::{
    calendar::Event,
    ids::{CalendarId, ContactId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, LocalDateTime},
    write::PendingOutcome,
};
use engine_provider::{ContactsProvider, EventDeletion};
use engine_store::LeasedPendingOp;

use super::*;
use crate::outbox::execute::execute_claimed;

/// The dispatch entry serves every verb, so the fake must carry the contacts
/// surface too. The trait's defaults error, which no mail-verb test here
/// reaches — the delete test never reaches the provider at all.
#[async_trait::async_trait]
impl ContactsProvider for FakeMail {}

/// Enqueues `payload` and claims it back — the exact state a drainer holds when
/// it calls the execute half: a lease-wrapped op whose payload is the only
/// record of the write.
async fn hand_claimed(
    store: &SqliteStore<ManualClock>,
    idempotency: &str,
    resource: &str,
    payload: serde_json::Value,
) -> LeasedPendingOp {
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(idempotency).unwrap(),
                ResourceKey::new(resource).unwrap(),
                payload,
            ),
        )
        .await
        .unwrap();
    store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(1)),
            16,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|op| op.id == op_id)
        .expect("the just-enqueued op is claimable")
}

#[tokio::test]
async fn an_ambiguous_hand_claimed_submit_parks_for_confirmation() {
    // The never-blind-retry red line, driven through the dispatch entry alone:
    // the same NeedsConfirmation parking the inline driver records — proved
    // here without an inline driver in sight.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let leased = hand_claimed(
        &store,
        "execute:submit:ambiguous",
        "draft:send-1@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(draft("send-1@test.local")),
        })
        .unwrap(),
    )
    .await;

    let outcome = execute_claimed(&provider, &store, &account(), &leased)
        .await
        .unwrap();
    assert!(matches!(outcome, PendingOutcome::NeedsConfirmation { .. }));
}

#[tokio::test]
async fn a_hand_claimed_submit_succeeds_and_resolves_to_the_sent_key() {
    // The happy counterpart: the outcome is the same Succeeded the inline
    // driver records — the dispatched provider key is the sent copy's.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let leased = hand_claimed(
        &store,
        "execute:submit:ok",
        "draft:send-2@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(draft("send-2@test.local")),
        })
        .unwrap(),
    )
    .await;

    let outcome = execute_claimed(&provider, &store, &account(), &leased)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        PendingOutcome::Succeeded {
            provider_key: key("sent-1"),
        }
    );
}

#[tokio::test]
async fn a_calendar_op_is_refused_without_any_accounting() {
    // Calendar replay needs a re-fetched base and conflict recovery this phase
    // does not build; the dispatcher refuses, and the refusal resolves nothing
    // — the op stays lease-held for the caller's accounting.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let leased = hand_claimed(
        &store,
        "execute:calendar:refused",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::DeleteEvent {
            deletion: EventDeletion::of(&stored_event()),
        })
        .unwrap(),
    )
    .await;

    let err = execute_claimed(&provider, &store, &account(), &leased)
        .await
        .unwrap_err();
    match err {
        crate::SyncError::Outbox(detail) => {
            assert_eq!(detail, "calendar ops are not replayable in this phase");
        }
        other => panic!("expected an outbox refusal, got {other:?}"),
    }
    assert_eq!(
        store.pending_op_state(leased.id).await.unwrap(),
        Some(PendingOpState::InFlight),
        "a refused op must not be resolved by the dispatcher"
    );
}

#[tokio::test]
async fn an_undecodable_payload_is_a_terminal_error() {
    // A payload that does not decode as a tagged intent cannot be executed;
    // the dispatcher reports it and resolves nothing — the caller decides.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let leased = hand_claimed(
        &store,
        "execute:undecodable",
        "mail:whatever",
        serde_json::Value::Null,
    )
    .await;

    let err = execute_claimed(&provider, &store, &account(), &leased)
        .await
        .unwrap_err();
    match err {
        crate::SyncError::Outbox(detail) => {
            assert!(detail.contains("undecodable"), "got: {detail}");
        }
        other => panic!("expected an outbox error, got {other:?}"),
    }
    assert_eq!(
        store.pending_op_state(leased.id).await.unwrap(),
        Some(PendingOpState::InFlight)
    );
}

#[tokio::test]
async fn a_delete_of_an_already_gone_card_completes_idempotently() {
    // The delete intent carries only the id — the base card is re-read from the
    // store. Absent there means the deletion already reconciled upstream, so
    // the replay completes with the same success the provider verbs grant an
    // already-absent card, without a provider call.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let contact = ContactId::try_from("card-gone").unwrap();
    let leased = hand_claimed(
        &store,
        "execute:delete:gone",
        "contact:card-gone",
        serde_json::to_value(OutboxIntent::DeleteContact {
            contact: contact.clone(),
        })
        .unwrap(),
    )
    .await;

    let outcome = execute_claimed(&provider, &store, &account(), &leased)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        PendingOutcome::Succeeded {
            provider_key: contact.key().clone(),
        }
    );
}

/// A stored event just complete enough for an `EventDeletion` to target.
fn stored_event() -> Event {
    Event::new(
        EventId::try_from("/cal/default/evt-1.ics").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        Memberships::of_one(CalendarId::new(key("/cal/default/"))),
        at(9),
    )
}

fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(
        format!("2026-08-01T{hour:02}:00:00")
            .parse::<LocalDateTime>()
            .unwrap(),
    )
}
