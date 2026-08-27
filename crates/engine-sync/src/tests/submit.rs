//! Mail-submission outbox-driver tests: enqueue-then-send success recording,
//! failure recorded without a blind retry, an ambiguous post-DATA send parked for
//! confirmation, and the tagged submit-payload round-trip a recovery worker
//! depends on. Uses the shared fakes and helpers from the parent module via
//! `use super::*`.

use super::*;

#[tokio::test]
async fn submit_mail_enqueues_then_sends_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-1@test.local"),
    )
    .await
    .unwrap();

    assert_eq!(outcome.email_key.as_str(), "sent-1");
    assert_eq!(outcome.message_id.as_str(), "send-1@test.local");
    // The durable op reached terminal success.
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn submit_mail_records_failure_without_blind_retry() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::Submit);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-2@test.local"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Provider(_)));

    // Recover the op id via an idempotent re-enqueue and confirm it was recorded
    // Failed (not retried here).
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-2@test.local").unwrap(),
                ResourceKey::new("draft:send-2@test.local").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn submit_mail_parks_an_ambiguous_send_for_confirmation() {
    // A post-DATA ambiguity must be recorded NeedsConfirmation, not Failed — so the
    // outbox never blind-retries and risks a double-send (`providers.md`).
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-3@test.local"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Provider(_)));

    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-3@test.local").unwrap(),
                ResourceKey::new("draft:send-3@test.local").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
}

#[test]
fn submit_payload_round_trips_through_a_durable_op() {
    // The outbox stores the submission intent as a tagged payload (`kind`), so a
    // recovery worker can dispatch on it: re-render a draft, or re-send rendered
    // bytes. The draft must survive that encoding intact — same construction
    // `submit_mail` uses.
    let original = draft("durable@test.local");
    let payload = serde_json::to_value(SubmitPayload::Draft(&original)).unwrap();
    assert_eq!(payload["kind"], serde_json::json!("draft"));
    let restored: SubmitPayload<Draft> = serde_json::from_value(payload).unwrap();
    assert_eq!(restored, SubmitPayload::Draft(original));
}
