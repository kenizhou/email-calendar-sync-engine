//! The calendar half of the PIM drain tests: the create/crash-orphan founding
//! cases, the base re-read pins (fresh base, gone base, occurrence delete),
//! poison, and the NeedsResync-settles pin — over the shared fixtures the
//! parent module holds.

use engine_core::write::PendingOpKind;
use engine_provider::{EventDeletion, EventEdit, EventPatch, Occurrence, PatchTarget};
use engine_store::{ManualClock, PendingOpState};

use super::*;
use crate::outbox::{OutboxIntent, drain_calendar_ops};

pub(super) async fn drain_calendar(
    provider: &FakeMail,
    store: &SqliteStore<ManualClock>,
) -> Result<usize, crate::SyncError> {
    drain_calendar_ops(provider, store, &account(), worker(), ttl()).await
}

fn stamp() -> engine_core::time::UtcDateTime {
    "2026-08-01T10:00:00Z".parse().unwrap()
}

fn at(hour: u8) -> engine_core::time::CalendarDateTime {
    engine_core::time::CalendarDateTime::utc(
        format!("2026-08-01T{hour:02}:00:00")
            .parse::<engine_core::time::LocalDateTime>()
            .unwrap(),
    )
}

fn draft(uid: &str) -> engine_provider::EventDraft {
    engine_provider::EventDraft::new(
        CalendarId::try_from("/cal/default/").unwrap(),
        Uid::new(uid).unwrap(),
        "Sprint planning",
        at(9),
        at(10),
        stamp(),
    )
}

#[tokio::test]
async fn an_unstarted_calendar_create_drains_to_succeeded() {
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        PendingOpKind::CalendarCreate,
        "drain:calendar:create",
        "event:evt-9@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: draft("evt-9@test.local"),
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
async fn a_crash_orphaned_calendar_create_is_reclaimed_and_drained() {
    // The founding case: the inline worker died between claim and mark, the
    // lease expired, and the targeted claim reclaims the `InFlight` op.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = crash_orphan(
        &store,
        &clock,
        PendingOpKind::CalendarCreate,
        "drain:calendar:orphan",
        "event:evt-8@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: draft("evt-8@test.local"),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the expired InFlight op was reclaimed");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_calendar_patch_replays_against_the_freshly_read_base() {
    // The intent contract: the payload carries the change, never the base it
    // was read at — the replay re-reads the stored event and re-applies.
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let provider = seed_stored_event(&store).await;
    let base = super::super::calendar_write::stored("/cal/default/evt-1.ics", "evt-1@test.local");
    let op = enqueue_op(
        &store,
        PendingOpKind::CalendarPatch,
        "drain:calendar:patch",
        "event:evt-1@test.local",
        serde_json::to_value(OutboxIntent::PatchEvent {
            edit: EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            ),
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
async fn a_calendar_patch_whose_event_is_gone_settles_conflict() {
    // A dead target is terminal — corrected by the next sync, never retried
    // into success.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let base = super::super::calendar_write::stored("/cal/gone.ics", "gone@test.local");
    let op = enqueue_op(
        &store,
        PendingOpKind::CalendarPatch,
        "drain:calendar:patch-gone",
        "event:gone@test.local",
        serde_json::to_value(OutboxIntent::PatchEvent {
            edit: EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            ),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "settling a conflict is an outcome — counted");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn an_occurrence_delete_whose_event_is_gone_succeeds() {
    // An occurrence of an absent event is already removed.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let base = super::super::calendar_write::stored("/cal/gone.ics", "gone@test.local");
    let deletion = EventDeletion::occurrence(&base, Occurrence::starting(at(9)), stamp());
    let op = enqueue_op(
        &store,
        PendingOpKind::CalendarDelete,
        "drain:calendar:delete-gone",
        "event:gone@test.local",
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
async fn a_poison_payload_settles_permanently_and_does_not_block_the_queue() {
    // A payload this build cannot decode never reaches the provider and
    // settles, so it cannot recycle for ever or stop the ops behind it.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let poison = enqueue_op(
        &store,
        PendingOpKind::CalendarPatch,
        "drain:calendar:poison",
        "event:poison@test.local",
        serde_json::json!({"verb": "not-a-real-verb"}),
    )
    .await;
    let healthy = enqueue_op(
        &store,
        PendingOpKind::CalendarCreate,
        "drain:calendar:after-poison",
        "event:evt-7@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: draft("evt-7@test.local"),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 2, "both ops were driven to outcomes");
    assert_eq!(
        store.pending_op_state(poison).await.unwrap(),
        Some(PendingOpState::Failed)
    );
    assert_eq!(
        store.pending_op_state(healthy).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_needs_resync_write_settles_failed_not_parked() {
    // The upstream queue's bounded semantics, pinned: a resync-required class
    // is NOT retryable (`FailureClass::is_retryable` is Retryable|RateLimited
    // only), so the store settles the op `Failed` on the first attempt — the
    // recovery is a fresh sync and a re-issued write, never a blind replay.
    // The T13 fact the fork's FORKING.md asks the host to honor.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::ResyncWrite);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        PendingOpKind::CalendarCreate,
        "drain:calendar:resync",
        "event:evt-6@test.local",
        serde_json::to_value(OutboxIntent::CreateEvent {
            draft: draft("evt-6@test.local"),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_calendar(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "settling is an outcome — counted");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed),
        "a resync-required class settles; only Retryable|RateLimited park"
    );
    let again = drain_calendar(&provider, &store).await.unwrap();
    assert_eq!(again, 0, "a settled op is never re-driven");
}
