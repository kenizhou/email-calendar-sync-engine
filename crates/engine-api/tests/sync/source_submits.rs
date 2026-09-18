//! The fork's rendered-source submission tests, split from `writes.rs` at the
//! 500-line cap. Fork-owned (`FORKING.md`, rendered-source submission seam row).

use engine_api::{ApiError, Engine, PendingOpState};
use engine_core::ids::ProviderKey;

use super::*;

/// The rendered-source seam through the facade: the caller's own final MIME —
/// rendered, then signed/encrypted, by it — goes to the provider **verbatim** and
/// the receipt's `Message-ID` is read back out of the bytes (there is no structured
/// field to echo), while the durable op commits `Succeeded` exactly as a draft
/// submission does, pollable by the returned id.
#[tokio::test]
async fn submit_mail_source_sends_the_callers_bytes_through_the_outbox() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let source = rendered_source("gen-3@test.local");
    // A Bcc-shaped recipient: in the envelope argument, never in the bytes.
    let recipients = vec!["bob@test.local".to_owned(), "carol@test.local".to_owned()];

    let outcome = engine
        .submit_mail_source(&provider, &account(), &source, &recipients)
        .await
        .unwrap();

    assert_eq!(outcome.email_key, ProviderKey::new("sent-1").unwrap());
    assert_eq!(outcome.message_id.as_str(), "gen-3@test.local");
    assert!(outcome.sent_copy.is_filed());
    // The durable op committed Succeeded, pollable by the returned id.
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// Bytes with no `Message-ID` are refused **before anything is enqueued** — the
/// caller must stamp one before submitting, because the op's keys, the receipt and
/// the reconciliation all hang off it. The facade surfaces that refusal as an
/// `ApiError::Sync` naming what the caller must fix; the nothing-enqueued half is
/// locked at the engine-sync layer.
#[tokio::test]
async fn submit_mail_source_refuses_bytes_without_a_message_id() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let unsigned = b"From: alice@test.local\r\nTo: bob@test.local\r\n\
                     Subject: No id\r\n\r\nbody\r\n";

    let err = engine
        .submit_mail_source(
            &provider,
            &account(),
            unsigned,
            &["bob@test.local".to_owned()],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
    assert!(
        err.to_string().contains("Message-ID"),
        "the refusal names what the caller must stamp, got {err}"
    );
}
