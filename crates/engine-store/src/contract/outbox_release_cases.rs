//! Outbox release contract cases: the fork's `release_pending_op` verb
//! (release-under-current-lease, lease fencing after release). Kept out of
//! `outbox_cases.rs` so that file stays byte-identical to upstream's and future
//! upstream merges to it stay clean.

use core::time::Duration;

use engine_core::write::PendingOutcome;

use super::{acct, lease_request, pending_op, pk};
use crate::{
    error::StoreError,
    lease::ManualClock,
    outbox::PendingOpState,
    store::{Store, StoreRead},
};

/// `release_pending_op` hands a claimed op straight back to `Pending` under the
/// holder's own lease: the released op is claimable again immediately, and the
/// released lease is dead — its token was bumped, so neither a mark nor another
/// release under it can ever apply. The still-leased sibling is untouched.
pub(super) async fn release_returns_a_claimed_op_to_runnable<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-release");
    let held = store
        .enqueue_pending_op(account.clone(), pending_op("rel-held", "res-held"))
        .await
        .unwrap();
    let released = store
        .enqueue_pending_op(account.clone(), pending_op("rel-freed", "res-freed"))
        .await
        .unwrap();

    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker-a", 300), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);

    store.release_pending_op(&claimed[1].lease).await.unwrap();
    assert_eq!(
        store.pending_op_state(released).await.unwrap(),
        Some(PendingOpState::Pending),
        "the release handed the op back to Pending"
    );
    let dead = store
        .mark_pending_op(
            &claimed[1].lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server"),
            },
        )
        .await
        .expect_err("the released lease is fenced out");
    assert_eq!(dead, StoreError::StaleLease);

    // The next claimant gets exactly the released op — never the held one.
    let reclaimed = store
        .claim_pending_ops(account.clone(), lease_request("worker-b", 300), 10)
        .await
        .unwrap();
    assert_eq!(
        reclaimed.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![released],
        "only the released op is runnable again"
    );
    assert_eq!(
        store.pending_op_state(held).await.unwrap(),
        Some(PendingOpState::InFlight),
        "the still-leased op was not disturbed"
    );
}

/// Release is the current lease holder's alone: a superseded lease (the op was
/// re-claimed after expiry) is rejected, and so is the *current* lease once the
/// op has recorded an outcome — a terminal state must never walk back to
/// runnable.
pub(super) async fn release_requires_the_current_lease<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-release-guard");
    store
        .enqueue_pending_op(account.clone(), pending_op("rel-stale", "res-stale"))
        .await
        .unwrap();
    store
        .enqueue_pending_op(account.clone(), pending_op("rel-done", "res-done"))
        .await
        .unwrap();
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker-a", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);

    // Stale: the lease expired and another worker re-claimed the ops.
    clock.advance(Duration::from_secs(90));
    let reclaimed = store
        .claim_pending_ops(account.clone(), lease_request("worker-b", 300), 10)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 2);
    let stale = store
        .release_pending_op(&claimed[0].lease)
        .await
        .expect_err("a superseded lease cannot release");
    assert_eq!(stale, StoreError::StaleLease);
    assert_eq!(
        store.pending_op_state(claimed[0].id).await.unwrap(),
        Some(PendingOpState::InFlight),
        "the re-claiming worker's hold is untouched"
    );

    // Resurrection guard: `mark` does not bump the token, so the token alone
    // cannot tell a released claim from a marked one — the op being `InFlight`
    // is what refuses to walk a recorded outcome back to runnable.
    store
        .mark_pending_op(
            &reclaimed[1].lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server"),
            },
        )
        .await
        .unwrap();
    let resurrect = store
        .release_pending_op(&reclaimed[1].lease)
        .await
        .expect_err("an op past its outcome cannot be released");
    assert_eq!(resurrect, StoreError::StaleLease);
    assert_eq!(
        store.pending_op_state(reclaimed[1].id).await.unwrap(),
        Some(PendingOpState::Succeeded),
        "the terminal outcome stands"
    );
}
