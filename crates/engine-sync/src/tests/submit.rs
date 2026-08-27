//! Mail-submission outbox-driver tests: enqueue-then-send success recording,
//! failure recorded without a blind retry, an ambiguous post-DATA send parked for
//! confirmation, and the tagged submit-payload round-trip a recovery worker
//! depends on. The `submit_mail_source` cases mirror them over caller-rendered
//! bytes — including the pre-enqueue refusals and the cross-path idempotency
//! convergence on one `Message-ID`. Uses the shared fakes and helpers from the
//! parent module via `use super::*`.

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
    // The outbox stores the submission intent inside the tagged envelope
    // (`verb`), with the payload's own `kind` tag nested inside it, so a
    // recovery worker can dispatch on both: the verb picks the driver, the kind
    // picks re-render-a-draft vs re-send-bytes. The draft must survive that
    // encoding intact — same construction `submit_mail` uses.
    let original = draft("durable@test.local");
    let payload = serde_json::to_value(OutboxIntent::SubmitMail {
        payload: SubmitPayload::Draft(original.clone()),
    })
    .unwrap();
    assert_eq!(payload["verb"], serde_json::json!("submit_mail"));
    assert_eq!(payload["payload"]["kind"], serde_json::json!("draft"));
    assert_eq!(
        serde_json::from_value::<OutboxIntent>(payload).unwrap(),
        OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(original)
        }
    );
}

#[test]
fn rendered_source_payload_round_trips_through_a_durable_op() {
    // Same construction `submit_mail_source` uses: the bytes themselves — and
    // the envelope recipients they must be sent to — are the durable intent,
    // tagged `rendered_source`, so a recovery worker re-sends them verbatim to
    // the same RCPT TO set instead of re-rendering. Non-UTF-8 bytes
    // (signed/encrypted MIME) ride as base64 — pinned in engine-core; this pins
    // the tagged round-trip a drainer's decode depends on.
    let bytes = rendered_source("durable-source@test.local");
    let payload = serde_json::to_value(OutboxIntent::SubmitMail {
        payload: SubmitPayload::<Draft>::RenderedSource {
            rfc5322: bytes.clone(),
            recipients: vec!["bob@test.local".to_owned()],
        },
    })
    .unwrap();
    assert_eq!(payload["verb"], serde_json::json!("submit_mail"));
    assert_eq!(
        payload["payload"]["kind"],
        serde_json::json!("rendered_source")
    );
    assert_eq!(
        serde_json::from_value::<OutboxIntent>(payload).unwrap(),
        OutboxIntent::SubmitMail {
            payload: SubmitPayload::<Draft>::RenderedSource {
                rfc5322: bytes,
                recipients: vec!["bob@test.local".to_owned()],
            }
        }
    );
}

#[tokio::test]
async fn submit_mail_source_enqueues_then_sends_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &rendered_source("send-4@test.local"),
        &["bob@test.local".to_owned()],
    )
    .await
    .unwrap();

    assert_eq!(outcome.email_key.as_str(), "sent-1");
    // The receipt's id is the bytes' own `Message-ID`, read back out of them.
    assert_eq!(outcome.message_id.as_str(), "send-4@test.local");
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    // The op is idempotent by that id, in the Draft path's namespace: a re-enqueue
    // under `submit:{id}` / `draft:{id}` finds the same op rather than minting one.
    let again = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-4@test.local").unwrap(),
                ResourceKey::new("draft:send-4@test.local").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(again, outcome.op);
}

#[tokio::test]
async fn submit_mail_source_records_failure_without_blind_retry() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::Submit);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &rendered_source("send-5@test.local"),
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Provider(_)));

    // Recover the op id via an idempotent re-enqueue and confirm it was recorded
    // Failed (not retried here) — the classification mirrors the Draft path.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-5@test.local").unwrap(),
                ResourceKey::new("draft:send-5@test.local").unwrap(),
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
async fn submit_mail_source_parks_an_ambiguous_send_for_confirmation() {
    // A post-DATA ambiguity must be recorded NeedsConfirmation, not Failed — the
    // source path shares the Draft path's no-blind-retry discipline.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &rendered_source("send-6@test.local"),
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Provider(_)));

    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-6@test.local").unwrap(),
                ResourceKey::new("draft:send-6@test.local").unwrap(),
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

#[tokio::test]
async fn submit_mail_source_refuses_bytes_without_a_message_id_without_enqueuing() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        b"From: alice@test.local\r\n\r\nbody\r\n",
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Outbox(_)));
    // The refusal happened before the enqueue: nothing was left in the outbox.
    let claimed = store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(1)),
            16,
        )
        .await
        .unwrap();
    assert!(claimed.is_empty());
}

#[tokio::test]
async fn submit_mail_source_refuses_unterminated_bytes_without_enqueuing() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    // A `Message-ID` is present, but the last line has no terminator — the same
    // bytes the provider refuses pre-dial are refused before the enqueue too.
    let err = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        b"Message-ID: <send-7@test.local>\r\nFrom: alice@test.local\r\n\r\nbody",
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Outbox(_)));
    let claimed = store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(1)),
            16,
        )
        .await
        .unwrap();
    assert!(claimed.is_empty());
}

#[tokio::test]
async fn submit_mail_source_converges_on_the_draft_paths_op_for_the_same_message_id() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let first = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-8@test.local"),
    )
    .await
    .unwrap();

    // The same `Message-ID` through the source path hits the same op (the keys
    // share one namespace): the duplicate enqueue dedups, the resolved op is not
    // claimable, and the driver surfaces that — exactly what a second `submit_mail`
    // of the same draft does. No second op is minted and nothing is re-sent.
    let err = submit_mail_source(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &rendered_source("send-8@test.local"),
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Outbox(_)));
    // The one op is still the Draft path's, still Succeeded — never re-driven.
    assert_eq!(
        store.pending_op_state(first.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    let claimed = store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(1)),
            16,
        )
        .await
        .unwrap();
    assert!(claimed.is_empty());
}

/// Caller-rendered source bytes a `submit_mail_source` test can submit: a minimal
/// RFC 5322 message with the given `Message-ID`, a `From` the envelope derives
/// from, and a trailing line terminator — the shape the seam accepts.
fn rendered_source(message_id: &str) -> Vec<u8> {
    format!(
        "Message-ID: <{message_id}>\r\nFrom: alice@test.local\r\nTo: bob@test.local\r\n\
         Subject: Rendered\r\n\r\nbody\r\n"
    )
    .into_bytes()
}
