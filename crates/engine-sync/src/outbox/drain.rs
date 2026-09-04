//! The outbox drainer: the background counterpart of the inline drivers.
//!
//! An inline driver resolves the op it just enqueued in the same call; a
//! drainer resolves the ops nobody finished — an unstarted `Pending` op, or a
//! crash orphan (`InFlight` under a lease that has expired) — by claiming a
//! batch and replaying each op through the same execute halves the inline path
//! runs ([`execute_claimed_mail`](super::execute::execute_claimed_mail) /
//! [`execute_claimed_contact`](super::execute::execute_claimed_contact) /
//! [`execute_claimed_calendar`](super::execute::execute_claimed_calendar)),
//! then settling each under its lease. One claim batch per call: work is
//! bounded, and a deeper backlog simply needs another call.
//!
//! Three entry points, one per provider surface, because the split is the
//! providers' own: a mail-only provider (IMAP) cannot satisfy
//! `ContactsProvider` yet still has mail ops to drain, and every calendar verb
//! lives on `Provider` itself, so the calendar drain needs no tighter bound
//! than the mail one. The claim and settle machinery — everything except the
//! one execute call per op — is shared ([`settle_claimed`]).

use core::time::Duration;

use engine_core::{error::FailureClass, ids::AccountId, write::PendingOutcome};
use engine_provider::{ContactsProvider, Provider};
use engine_store::{LeaseRequest, LeasedPendingOp, Store, StoreError, StoreRead, WorkerId};

use super::execute::{
    ExecuteFailure, execute_claimed_calendar, execute_claimed_contact, execute_claimed_mail,
};
use crate::SyncError;

/// Drains up to `limit` of this account's runnable **mail** ops — `submit_mail`,
/// `edit_mail`, and `report_message` intents — claiming them under a fresh lease
/// and replaying each through the mail execute half with exactly the inline
/// drivers' semantics (an ambiguous send parks as `NeedsConfirmation`, never
/// blind-retried).
///
/// Returns how many ops this call drove to a recorded outcome — `Succeeded`,
/// `Failed` (including the terminal `Failed` a payload that does not decode as
/// a tagged intent is poison-marked with), or a parked `NeedsConfirmation`.
/// Not counted: ops left unmarked, which are (a) **foreign-scope verbs** — a
/// calendar or contact intent, claimed because claims are scope-blind, skipped
/// without a mark and **released** back to `Pending` under the claim's own
/// lease, so the right executor — the calendar or contact drain — can claim it
/// immediately, in the same round — and (b) an op whose
/// mark came back `StaleLease` (another worker re-claimed it; its outcome is
/// that worker's to record, dropped here silently).
///
/// The cost of a skip is its claim slot, not a lease TTL: the claim moves a
/// skipped op to `InFlight` only momentarily — the settle half hands it
/// straight back, with its fencing token bumped so the skipper's dead lease
/// can never mark or release it again. Drains can therefore run in any order
/// without burning each other's ops into lease-holds; a skip costs the op its
/// place in this batch, nothing more.
///
/// A replayed submission's `SentCopy` fact (what became of the sender's own
/// copy) is lost: the outcome records only the op state. Phase-1 limitation;
/// the host observes completion through the op state.
///
/// # Errors
///
/// Returns [`SyncError::Store`] when the claim, a mark, a release, or a
/// replay's store read fails (an execution failure is not an error: it arrives
/// as the outcome this call records).
pub async fn drain_mail_ops<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    limit: usize,
) -> Result<usize, SyncError>
where
    P: Provider,
    S: Store,
{
    let claimed = store
        .claim_pending_ops(account.clone(), LeaseRequest::new(worker, ttl), limit)
        .await?;
    let mut driven = 0;
    for leased in &claimed {
        let executed = execute_claimed_mail(provider, account, leased).await;
        driven += usize::from(settle_claimed(store, leased, executed).await?);
    }
    Ok(driven)
}

/// Drains up to `limit` of this account's runnable **contact** ops —
/// `create_contact`, `patch_contact`, and `delete_contact` intents — with the
/// same claim/replay/settle discipline and the same counting semantics as
/// [`drain_mail_ops`] (see its docs for the exact accounting and the skip's
/// release). Patch and delete replays re-read the base card by id from the store,
/// exactly as the contact execute half prescribes.
///
/// # Errors
///
/// Returns [`SyncError::Store`] when the claim, a mark, a release, or a
/// replay's base-card read fails (an execution failure is not an error: it
/// arrives as the outcome this call records).
pub async fn drain_contact_ops<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    limit: usize,
) -> Result<usize, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead,
{
    let claimed = store
        .claim_pending_ops(account.clone(), LeaseRequest::new(worker, ttl), limit)
        .await?;
    let mut driven = 0;
    for leased in &claimed {
        let executed = execute_claimed_contact(provider, store, account, leased).await;
        driven += usize::from(settle_claimed(store, leased, executed).await?);
    }
    Ok(driven)
}

/// Drains up to `limit` of this account's runnable **calendar** ops —
/// `create_event`, `patch_event`, `put_event_doc`, `rsvp_event`, and
/// `delete_event` intents — with the same claim/replay/settle discipline and
/// the same counting semantics as [`drain_mail_ops`] (see its docs for the
/// exact accounting and the skip's release). A replayed patch, RSVP, or
/// occurrence delete re-reads the base event by id from the store, exactly as
/// the calendar execute half prescribes: a patch or RSVP whose event is gone
/// is a terminal `Conflict`, an occurrence delete whose event is gone is a
/// success, and a series delete needs no base at all.
///
/// # Errors
///
/// Returns [`SyncError::Store`] when the claim, a mark, a release, or a
/// replay's base-event read fails (an execution failure is not an error: it
/// arrives as the outcome this call records).
pub async fn drain_calendar_ops<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    limit: usize,
) -> Result<usize, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let claimed = store
        .claim_pending_ops(account.clone(), LeaseRequest::new(worker, ttl), limit)
        .await?;
    let mut driven = 0;
    for leased in &claimed {
        let executed = execute_claimed_calendar(provider, store, account, leased).await;
        driven += usize::from(settle_claimed(store, leased, executed).await?);
    }
    Ok(driven)
}

/// The settle half both drains share: records one claimed op's execution result
/// under its lease, discriminating on the structured
/// [`ExecuteFailure`](super::execute::ExecuteFailure) the execute halves report
/// — never on an error string.
///
/// Returns whether this drain drove the op to an outcome (the count the loops
/// report). The two no-count cases:
///
/// - **Out of scope** — the op is another drain's to execute; skipped *unmarked* and **released**
///   back to `Pending` under the lease the claim minted (its fencing token bumped, so this drain's
///   lease is dead), so the right executor can claim it immediately rather than being resolved by a
///   loop that cannot know its semantics — or waiting out a lease TTL, the pre-release cost.
/// - **Stale lease on the mark or release** — another worker re-claimed the op underneath; its
///   outcome is that worker's to record, so the result is dropped silently.
///
/// Terminal poison (an undecodable payload) is marked terminally `Failed` with
/// class [`Permanent`](FailureClass::Permanent) so the lease never expires back
/// into runnable and the op cannot recycle forever. The decode detail stays on
/// the failure surface — the same place every other `Failed` mark's detail (the
/// inline drivers' provider errors) stays; Phase 1 persists no outcome payload.
pub(crate) async fn settle_claimed<S>(
    store: &S,
    leased: &LeasedPendingOp,
    executed: Result<PendingOutcome, ExecuteFailure>,
) -> Result<bool, SyncError>
where
    S: Store,
{
    let outcome = match executed {
        Ok(outcome) => outcome,
        Err(ExecuteFailure::Undecodable(_)) => PendingOutcome::Failed {
            class: FailureClass::Permanent,
            retry_after: None,
        },
        Err(ExecuteFailure::OutOfScope) => {
            // Hand the op straight back: this drain claimed an intent it cannot
            // execute, and the lease it holds is the only thing standing between
            // the op and its own drain. A stale release means the lease already
            // expired and another worker re-claimed the op — that worker owns it
            // now, dropped silently exactly as a stale mark is.
            return match store.release_pending_op(&leased.lease).await {
                Ok(()) | Err(StoreError::StaleLease) => Ok(false),
                Err(err) => Err(SyncError::Store(err)),
            };
        }
        Err(ExecuteFailure::Store(err)) => return Err(SyncError::Store(err)),
    };
    match store.mark_pending_op(&leased.lease, outcome).await {
        Ok(()) => Ok(true),
        Err(StoreError::StaleLease) => Ok(false),
        Err(err) => Err(SyncError::Store(err)),
    }
}
