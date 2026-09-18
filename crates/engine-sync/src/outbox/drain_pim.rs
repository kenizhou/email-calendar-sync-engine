//! The PIM halves of the outbox drainer: the background counterpart of the
//! inline contact and calendar drivers, replaying the ops they recorded and
//! never resolved.
//!
//! Upstream's [`drain_outbox`](super::drain::drain_outbox) is mail-only by
//! design — a calendar patch or delete needs the base event beside the
//! request, so its replay re-reads the base and re-applies the stored intent
//! (the fork's owed follow-up from the 2026-09-18 drainer discard; engine
//! `FORKING.md`). This file is that port, in the upstream queue's own shape:
//! list the account's queue, keep only this drain's kinds, take each under a
//! *targeted* claim (nothing foreign is ever leased), replay it through the
//! same `execute_*` halves the inline drivers run
//! ([`drain_replay`](super::drain_replay)), and record the outcome under the
//! claim's lease. The store owns park-vs-settle — attempt counting, the
//! 8-attempt bound, the backoff — exactly as the mail drainer records.
//!
//! `InFlight` rows are admitted to the claim alongside `Pending` ones because
//! the targeted claim reclaims an op whose lease died — the crash-orphan
//! founding case — and refuses a live one (`Busy`), so admitting the state
//! never leases an op mid-round-trip.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    write::{PendingOpId, PendingOpKind, PendingOutcome},
};
use engine_provider::{ContactsProvider, Provider};
use engine_store::{
    LeaseRequest, LeasedPendingOp, PendingOpClaim, PendingOpRow, PendingOpState, Store, StoreRead,
    WorkerId,
};

use super::{
    drain_replay::{ReplayFault, replay_calendar_op, replay_contact_op},
    record_failure,
};
use crate::SyncError;

/// Drains this account's queued **contact** ops — the `create_contact`,
/// `patch_contact`, and `delete_contact` intents that were recorded but never
/// resolved — under the same claim/replay/settle discipline as upstream's
/// [`drain_outbox`](super::drain::drain_outbox) (see its docs for the claim
/// contract). Returns how many ops this pass drove to a recorded outcome:
/// succeeded, settled-failed (a conflict, a refusal, or terminal poison), or
/// parked on a retryable class for a later pass.
///
/// # Errors
///
/// Returns [`SyncError::Store`] if the queue cannot be read, an op cannot be
/// claimed, or an outcome cannot be recorded. A provider failure is not an
/// error: it is recorded against its op and counted.
pub async fn drain_contact_ops<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<usize, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let mut drained = 0;
    for row in store.list_pending_ops(account.clone()).await? {
        if !contact_op(&row) {
            continue;
        }
        let Some(leased) = claim(store, account, row.id, &worker, ttl).await? else {
            continue;
        };
        settle_replay(
            store,
            &leased,
            replay_contact_op(provider, store, account, &leased).await,
        )
        .await?;
        drained += 1;
    }
    Ok(drained)
}

/// Drains this account's queued **calendar** ops — the `create_event`,
/// `patch_event`, `put_calendar_document`, `rsvp_event` (direct or
/// from-invite), and `delete_event` intents that were recorded but never
/// resolved — under the same discipline as [`drain_contact_ops`] (see its
/// docs). Every calendar verb lives on [`Provider`] itself, so this drain
/// needs no tighter provider bound than the mail one.
///
/// # Errors
///
/// As [`drain_contact_ops`].
pub async fn drain_calendar_ops<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<usize, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let mut drained = 0;
    for row in store.list_pending_ops(account.clone()).await? {
        if !calendar_op(&row) {
            continue;
        }
        let Some(leased) = claim(store, account, row.id, &worker, ttl).await? else {
            continue;
        };
        settle_replay(
            store,
            &leased,
            replay_calendar_op(provider, store, account, &leased).await,
        )
        .await?;
        drained += 1;
    }
    Ok(drained)
}

/// Takes one op under a targeted claim, or `None` when the store refused —
/// its answer that the op is not this pass's to take (still backing off, held
/// under a live lease, or settled), never a failure.
async fn claim<S>(
    store: &S,
    account: &AccountId,
    op: PendingOpId,
    worker: &WorkerId,
    ttl: Duration,
) -> Result<Option<LeasedPendingOp>, SyncError>
where
    S: Store,
{
    let req = LeaseRequest::new(worker.clone(), ttl);
    match store.claim_pending_op(account.clone(), op, req).await? {
        PendingOpClaim::Leased(leased) => Ok(Some(*leased)),
        PendingOpClaim::Refused(_) => Ok(None),
    }
}

/// Records one replay's result under the lease it was claimed with: a ready
/// outcome is marked as-is, a provider failure goes through `record_failure`
/// (whose mark the store turns into a parked retry or a terminal settle), and
/// poison settles `Failed`/`Permanent` so the op cannot recycle for ever.
async fn settle_replay<S>(
    store: &S,
    leased: &LeasedPendingOp,
    replay: Result<PendingOutcome, ReplayFault>,
) -> Result<(), SyncError>
where
    S: Store,
{
    match replay {
        Ok(outcome) => {
            store.mark_pending_op(&leased.lease, outcome).await?;
        }
        Err(ReplayFault::Provider(err)) => {
            record_failure(store, leased, &err).await?;
        }
        Err(ReplayFault::Poison) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Failed {
                        class: FailureClass::Permanent,
                        retry_after: None,
                    },
                )
                .await?;
        }
        Err(ReplayFault::Store(err)) => return Err(SyncError::Store(err)),
    }
    Ok(())
}

/// Whether a queued row is a contact op this drain can replay. The state admits
/// `InFlight` because the targeted claim reclaims an expired lease and refuses
/// a live one; `NeedsConfirmation` is excluded — a parked confirmation is
/// never re-driven.
fn contact_op(row: &PendingOpRow) -> bool {
    runnable(row)
        && matches!(
            row.kind,
            Some(
                PendingOpKind::ContactCreate
                    | PendingOpKind::ContactPatch
                    | PendingOpKind::ContactDelete
            )
        )
}

/// Whether a queued row is a calendar op this drain can replay.
fn calendar_op(row: &PendingOpRow) -> bool {
    runnable(row)
        && matches!(
            row.kind,
            Some(
                PendingOpKind::CalendarCreate
                    | PendingOpKind::CalendarPatch
                    | PendingOpKind::CalendarDocument
                    | PendingOpKind::CalendarRsvp
                    | PendingOpKind::CalendarDelete
            )
        )
}

fn runnable(row: &PendingOpRow) -> bool {
    matches!(
        row.state,
        PendingOpState::Pending | PendingOpState::InFlight
    )
}
