// SPDX-License-Identifier: MPL-2.0
//! The shared collection-key ledger discipline (P2 Task 5, split from
//! `adapter/mod.rs` for the 500-line rule): the key a write rides and
//! the rotation a write records — the `edit_mail` / `calendar_write` /
//! `contacts` write verbs' shared key source.
//!
//! An EAS `SyncKey` is per-collection server state the client must
//! thread through every command — and the trait's write seams carry no
//! cursor. The adapter therefore owns a one-key ledger per bound
//! collection: a completed read pass records its final key (the same
//! value the engine persists as its cursor — the cursor stays the
//! authority for what has been delivered), and each write rides the
//! ledger's key and records its rotation. Resuming a pass from a
//! rotation is lossless because the upsync request sends no
//! `GetChanges` ([MS-ASCMD]: invalid in 16.1) — a rotation carries no
//! server rows. A cold ledger (a fresh adapter that has not yet
//! observed a pass) refuses `NeedsResync` rather than guessing: the
//! orchestrator re-syncs, the pass re-seeds, the outbox retries the op.

use crate::commands::SyncChangeOutcome;
use engine_provider::{ProviderError, ProviderResult};

/// The bound collection's SyncKey ledger: the key the server last handed
/// this adapter for that collection — the write path's key source. A
/// plain mutex, never held across an await: every toucher already holds
/// the verb lock.
pub(crate) type CollectionKey = std::sync::Mutex<Option<String>>;

/// The key a write rides: the ledger's, or the cold-ledger refusal (the
/// `NeedsResync` cold path — the orchestrator re-syncs, the pass
/// re-seeds the ledger, and the outbox retries the write after it;
/// never a guessed key).
pub(crate) fn current_key(ledger: &CollectionKey) -> ProviderResult<String> {
    ledger
        .lock()
        .expect("collection-key ledger")
        .clone()
        .ok_or_else(|| {
            ProviderError::new(
                engine_core::error::FailureClass::NeedsResync,
                "the collection's sync key is unknown to this adapter — run a                  sync pass first (it seeds the key); the outbox retries the                  write after it",
            )
        })
}

/// Records a write's key rotation. A response that piggybacked server
/// rows (no `GetChanges` was sent, so a conforming server sends none —
/// a nonconforming one might) drops the ledger instead: those rows
/// cannot ride the receipt back, and falling to the engine cursor
/// surfaces the gap as a Reconcile on the next pass rather than
/// skipping them forever.
pub(crate) fn record_rotation(ledger: &CollectionKey, outcome: &SyncChangeOutcome) {
    let mut slot = ledger.lock().expect("collection-key ledger");
    if outcome.has_piggybacked() {
        log::warn!(
            "EAS Sync change response piggybacked server commands — dropping              the collection-key ledger; the next pass reconciles"
        );
        *slot = None;
    } else {
        *slot = Some(outcome.new_key.clone());
    }
}
