//! Mail writes: submission ([`submit_mail`], or caller-rendered bytes through
//! [`submit_mail_source`]), mutation ([`edit_mail`]) and reporting a message as
//! junk / not junk / phishing ([`report_message`]).
//!
//! Both submission paths share the outbox discipline (see the module docs): durable
//! op → claim → provider call → record the outcome under the lease — and one op
//! namespace keyed by `Message-ID`, so the same message through either path is one
//! op. Mutations follow the same discipline without the ambiguous-send branch.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey, SubmitPayload},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, ProviderError, SentCopy};
use engine_store::{LeasedPendingOp, Store, WorkerId};

use super::{enqueue_and_claim, record_failure};
use crate::SyncError;

/// The result of a successful submission through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// The durable op that recorded the send.
    pub op: PendingOpId,
    /// The provider key of the sent message (for reconciliation/threading).
    pub email_key: ProviderKey,
    /// The `Message-ID` that was sent.
    pub message_id: MessageIdHeader,
    /// What became of the sender's own copy.
    ///
    /// A delivered message whose copy was **not** filed still completes the op — the mail
    /// has gone, and re-sending it would be far worse than a missing copy — so the op state
    /// cannot carry this and the outcome is the only place the fact survives. A caller that
    /// ignores it drops it for good: nothing later can rediscover a copy that was never
    /// written to the server.
    pub sent_copy: SentCopy,
}

/// Sends `draft` through the outbox: durable op → claim → provider submit → record.
///
/// On a provider failure the op is recorded `Failed` (with the failure class) and
/// the error is returned — never blindly retried here.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the send fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the draft
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn submit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    draft: &Draft,
) -> Result<SubmitOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    // Durable record first: the draft as a tagged pending-op payload, idempotent by
    // Message-ID. The `kind` tag is what a future drainer dispatches on.
    let payload = serde_json::to_value(SubmitPayload::Draft(draft))
        .map_err(|e| SyncError::Outbox(format!("encode draft: {e}")))?;
    let message_id = draft.message_id.as_str();
    let idempotency = IdempotencyKey::new(format!("submit:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("draft:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, resource, payload),
    )
    .await?;

    // Provider side effect, then record the outcome under the lease.
    match provider.submit_email(account, draft).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.email_key.clone(),
                    },
                )
                .await?;
            Ok(SubmitOutcome {
                op: leased.id,
                email_key: receipt.email_key,
                message_id: receipt.message_id,
                sent_copy: receipt.sent_copy,
            })
        }
        Err(err) => {
            record_send_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Sends `source` — the caller's own final MIME bytes (e.g. a rendered draft the
/// host then signed or encrypted) — through the outbox **verbatim**: durable op →
/// claim → provider `submit_email_source` → record. The counterpart of
/// [`submit_mail`]: where that path renders a `Draft`, this one sends bytes the
/// caller rendered and never re-renders them. `recipients` is the envelope —
/// non-empty, the exact `RCPT TO` set (where Bcc lives without ever entering the
/// bytes); empty, derived from the bytes' own `To`/`Cc` headers (see
/// [`Provider::submit_email_source`] for the full Bcc semantics).
///
/// The op's keys are minted from the bytes' **own** `Message-ID` and share the
/// Draft path's namespace (`submit:{id}` / `draft:{id}` — the resource key keeps
/// its `draft:` name deliberately), so the same message submitted through either
/// path collapses to one op. The payload is
/// [`SubmitPayload::RenderedSource`](engine_core::write::SubmitPayload) — the
/// bytes ride in the op itself, tagged so a future drainer re-sends them verbatim
/// instead of re-rendering.
///
/// Two cheap validations run **before** the enqueue, so permanently-invalid bytes
/// never enter the outbox: the bytes must carry a `Message-ID` (the host must
/// stamp one before submitting — the Write Contract; the receipt and the op's keys
/// reconcile by it) and end in a line terminator. The provider's full
/// refuse-before-dial set (no `From`, no envelope recipient) still applies after
/// the enqueue — such an op is driven inline to a terminal `Failed` in this same
/// call, never left as junk to retry.
///
/// # Errors
///
/// Returns [`SyncError::Outbox`] when the bytes fail the pre-enqueue validations
/// (nothing is enqueued) or cannot be encoded, or when the just-enqueued op is not
/// claimable — which is also how a duplicate of an already-resolved op surfaces
/// (the enqueue dedups onto the earlier op, which is not claimable again; nothing
/// is re-sent); [`SyncError::Store`] on a store failure; [`SyncError::Provider`]
/// if the send fails (after recording it — `Failed` with its class, or
/// `NeedsConfirmation` when ambiguous; never blindly retried here).
pub async fn submit_mail_source<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    source: &[u8],
    recipients: &[String],
) -> Result<SubmitOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    // Validate before the enqueue: these bytes are permanently unsendable (the
    // provider refuses the same two pre-dial), so enqueueing them would only mint
    // an op destined to fail. The Message-ID is needed regardless — the op's keys
    // are minted from it.
    let message_id = engine_rfc5322::parse_message_id(source).ok_or_else(|| {
        SyncError::Outbox(
            "the submitted bytes carry no Message-ID; the caller must stamp one \
             before submitting them"
                .to_owned(),
        )
    })?;
    if !source.ends_with(b"\n") {
        return Err(SyncError::Outbox(
            "the submitted bytes do not end in a line terminator".to_owned(),
        ));
    }

    // Durable record first: the bytes as a tagged pending-op payload, idempotent by
    // the bytes' own Message-ID in the Draft path's namespace — one message, one op,
    // whichever path submits it.
    let payload = serde_json::to_value(SubmitPayload::<Draft>::RenderedSource {
        rfc5322: source.to_vec(),
    })
    .map_err(|e| SyncError::Outbox(format!("encode rendered source: {e}")))?;
    let idempotency = IdempotencyKey::new(format!("submit:{}", message_id.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("draft:{}", message_id.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, resource, payload),
    )
    .await?;

    // Provider side effect, then record the outcome under the lease — the same
    // discipline as the Draft path, including the ambiguous-send parking.
    match provider
        .submit_email_source(account, source, recipients)
        .await
    {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.email_key.clone(),
                    },
                )
                .await?;
            Ok(SubmitOutcome {
                op: leased.id,
                email_key: receipt.email_key,
                message_id: receipt.message_id,
                sent_copy: receipt.sent_copy,
            })
        }
        Err(err) => {
            record_send_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Records a failed submission under its lease: `NeedsConfirmation` for an
/// ambiguous send (e.g. a lost post-DATA SMTP ack) — parked, never recorded as a
/// plain retryable failure, so the outbox does not blind-retry and risk a
/// double-send (`providers.md`) — otherwise a classified `Failed` with its backoff
/// hint. Shared by both submission paths so their handling cannot drift; the plain
/// [`record_failure`](super::record_failure) serves the writes with no ambiguous
/// case (edits, reports).
async fn record_send_failure<S: Store>(
    store: &S,
    leased: &LeasedPendingOp,
    err: &ProviderError,
) -> Result<(), SyncError> {
    let outcome = if err.requires_confirmation() {
        PendingOutcome::NeedsConfirmation {
            detail: err.detail().to_owned(),
        }
    } else {
        PendingOutcome::Failed {
            class: err.class(),
            retry_after: err.retry_after(),
        }
    };
    store.mark_pending_op(&leased.lease, outcome).await?;
    Ok(())
}

/// The result of a successful mail edit through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEditOutcome {
    /// The durable op that recorded the edit.
    pub op: PendingOpId,
    /// The provider key the edit resolved to (the edited message; for a move, its
    /// source key — the next sync reconciles the destination copy).
    pub message_key: ProviderKey,
}

/// Applies a [`MailEdit`] through the outbox: durable op → claim → provider
/// `edit_mail` → record. The mail counterpart of
/// [`patch_calendar_event`](super::patch_calendar_event).
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent — it
/// must be **unique per edit intent** (the store dedups by `(account, key)` across
/// every op state, so a key derived only from the target would wrongly collapse two
/// distinct edits of one message — e.g. mark-read then mark-unread — into one op).
/// The op's `resource_key` is the target message key, so the store serializes edits
/// to one message (a second edit whose target is already in flight is *deferred*; the
/// thin inline driver assumes low outbox contention — the background worker is the
/// right driver under contention). A provider failure is recorded `Failed` (with its
/// class) and returned — never blindly retried here. Unlike an SMTP send there is no
/// `NeedsConfirmation` case: `UID STORE`/`MOVE`/`EXPUNGE` are not post-`DATA`-ambiguous
/// (a periodic snapshot reconciles the true state), and a stale-target `Conflict` is
/// self-correcting after a re-sync (`imap-smtp.md`).
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the edit fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn edit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    edit: &MailEdit,
) -> Result<MailEditOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(edit)
        .map_err(|e| SyncError::Outbox(format!("encode mail edit: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", edit.target().as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, resource, payload),
    )
    .await?;

    match provider.edit_mail(account, edit).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(MailEditOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// The result of a successful report through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOutcome {
    /// The durable op that recorded the report.
    pub op: PendingOpId,
    /// The provider key the report resolved to — the reported message. Where the move
    /// mints a new key (IMAP) this is the source key; the next sync of the destination
    /// reconciles the copy, exactly as for [`MailEditOutcome`].
    pub message_key: ProviderKey,
}

/// Reports `report.target` through the outbox: durable op → claim → provider
/// `report_message` → record. The reporting counterpart of [`edit_mail`].
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent, and must be
/// **unique per report intent** for the same reason it is on an edit: the store dedups by
/// `(account, key)` across every op state, so a key derived only from the target would
/// collapse "junk" and a later "not junk" on one message into a single op — which is
/// precisely the pair a user is most likely to perform in sequence, having reported the
/// wrong message.
///
/// The op's `resource_key` is the target message key, shared with [`edit_mail`], so a
/// report and an edit of the same message serialize against each other rather than racing.
///
/// There is no `NeedsConfirmation` case: a report sends no mail, every transport's report
/// is idempotent (re-reporting is accepted), and a stale-target `Conflict` is
/// self-correcting after a re-sync.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the report fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn report_message<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    report: &MessageReport,
) -> Result<ReportOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(report)
        .map_err(|e| SyncError::Outbox(format!("encode message report: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", report.target.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, resource, payload),
    )
    .await?;

    match provider.report_message(account, report).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(ReportOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}
