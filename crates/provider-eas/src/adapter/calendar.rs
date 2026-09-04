// SPDX-License-Identifier: MPL-2.0
//! The calendar read verbs: `sync_calendars` (FolderSync filtered to the
//! Calendar class) and `sync_events` (Sync class `Calendar` over the bound
//! calendar folder) — P2 Task 2.
//!
//! ## Containers (`sync_calendars`)
//!
//! The **same FolderSync machinery as the mail slice** (`mailboxes.rs`), with
//! the class filter inverted: `Calendar` folders (folder Type 8,
//! [MS-ASFD]) — and only they — land in the per-account
//! [`EasCalendarList`](engine_core::sync::SyncScope::EasCalendarList) scope.
//! The hierarchy SyncKey is the cursor (`None`/empty → bootstrap `"0"` →
//! snapshot; `Some(key)` → delta), and a status-9 invalidation recovers
//! inside the call as a re-bootstrapped snapshot — the mail slice's exact
//! recovery shape. Delta deletions pass through unfiltered (the wire's
//! Delete element carries no class; tombstoning a key the calendar scope
//! never held is a store no-op). One hierarchy SyncKey serves both the mail
//! and calendar container scopes, so a pass of one after the other sees the
//! stale key and takes its one-round status-9 snapshot recovery — correct,
//! self-healing, and rare (folder lists change rarely).
//!
//! ## Events (`sync_events`)
//!
//! Sync class `Calendar` per collection — the bound calendar folder IS the
//! `CollectionId` (the calendar binding the adapter was built with,
//! `EasAdapter::with_calendar`). The **collection SyncKey is the cursor**
//! (`None`/empty → `"0"`), `MoreAvailable` pages the pass inside the call,
//! and each round follows the mail slice's quirks: a response omitting its
//! SyncKey keeps the request's (the empty-key cursor-poisoning invariant),
//! Exchange 15.2's empty bootstrap round is followed once, and a
//! `MoreAvailable` round that makes no progress completes the pass.
//!
//! **SyncKey invalidation recovers inside the call.** Unlike the mail
//! stream (whose chunked yields pin the mode at the pass boundary), a
//! whole-scope `ScopeSync` is applied atomically — nothing has been handed
//! to the caller until the pass ends — so a resync-shaped collection status
//! (3, "MUST return to SyncKey value of 0"; 12, degraded to the same
//! collection reset) discards the accumulated rounds and restarts from
//! `"0"` **once** as a snapshot (present-set + tombstoning — the JMAP
//! `cannotCalculateChanges` recovery precedent). A status 3 answered to the
//! bootstrap key itself surfaces `NeedsResync` (nothing left to retry), as
//! does every other non-OK status through the Sync family classifier.
//!
//! Items convert through `calendar::calendar_event_from_props` (id =
//! ServerId, uid = the EAS UID, membership = the bound calendar); a
//! malformed item is skipped, never failing the pass
//! (`calendar-semantics.md`'s per-resource degrade — the CalDAV precedent).

use std::collections::BTreeSet;

use engine_core::{
    calendar::{Calendar, Event},
    ids::{CalendarId, ProviderKey},
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ProviderError, ProviderResult, ScopeSync};
use serde_json::json;
use tokio::sync::Mutex;

use super::{
    email::{MAX_WINDOW_SIZE, should_follow_empty_bootstrap},
    error::{provider_error, sync_status_error},
    mailboxes::{BOOTSTRAP_KEY, next_key, request_key},
};
use crate::{
    calendar::calendar_event_from_props,
    client::EasClient,
    status::{RecoveryAction, recovery_action_for_sync},
    types::{EasFolder, FolderSyncResult, SyncRequest},
};

/// FolderSync status 9: "folder hierarchy out of date" — the mail slice's
/// recovery, mirrored for the calendar container scope.
const HIERARCHY_OUT_OF_DATE: u32 = 9;

/// The adapter's extended-property namespace (the mail slices' convention).
const EXTENDED_NAMESPACE: &str = "eas";

// ============================================================================
// sync_calendars — FolderSync, Calendar class
// ============================================================================

/// One FolderSync pass over the calendar containers: request `key`, keep the
/// wire's Calendar-class folders (delta deletions unfiltered), and recover a
/// status-9 invalidation by re-bootstrapping once.
pub(super) async fn sync_calendars(
    client: &Mutex<EasClient>,
    cursor: Option<&SyncState>,
) -> ProviderResult<ScopeSync<Calendar>> {
    let key = request_key(cursor);
    let mut client = client.lock().await;
    client.set_hierarchy_sync_key(key.to_owned());
    match client.folder_sync(key).await {
        Ok(result) => Ok(scope_sync(&result, key)),
        // The stored hierarchy key is dead — restart from the bootstrap key,
        // exactly once (a server answering 9 to "0" itself surfaces through
        // `provider_error` as `NeedsResync`; this can never loop).
        Err(crate::client::EasError::CommandStatus {
            status: HIERARCHY_OUT_OF_DATE,
            ..
        }) if key != BOOTSTRAP_KEY => {
            let result = client
                .folder_sync(BOOTSTRAP_KEY)
                .await
                .map_err(provider_error)?;
            Ok(scope_sync(&result, BOOTSTRAP_KEY))
        }
        Err(e) => Err(provider_error(e)),
    }
}

/// Maps one FolderSync round into a `ScopeSync<Calendar>` — a snapshot when
/// the round requested the bootstrap key, a delta otherwise. Infallible: the
/// mapping only skips what it cannot key.
fn scope_sync(result: &FolderSyncResult, request_key: &str) -> ScopeSync<Calendar> {
    let next_cursor = SyncState::new(next_key(&result.sync_key, request_key));
    if request_key == BOOTSTRAP_KEY {
        let objects = calendars(&result.changes);
        let present: BTreeSet<ProviderKey> = objects.iter().map(key_of).collect();
        ScopeSync::new(SyncUpdate::snapshot(objects, present), next_cursor)
    } else {
        let changed = calendars(&result.changes);
        let removed: Vec<ProviderKey> = result
            .deletions
            .iter()
            .filter(|id| !id.is_empty())
            .filter_map(|id| ProviderKey::new(id.clone()).ok())
            .collect();
        ScopeSync::new(SyncUpdate::delta(changed, removed), next_cursor)
    }
}

/// The wire's Add/Update folders that belong to the calendar container
/// scope: folder Type 8 / class "Calendar" ([MS-ASFD] — the classless shape
/// a missing Type element parses to is mail by the mail slice's default, so
/// it is excluded here).
fn calendars(changes: &[EasFolder]) -> Vec<Calendar> {
    changes
        .iter()
        .filter(|folder| folder.class == "Calendar")
        .filter_map(|folder| {
            let Ok(id) = CalendarId::try_from(folder.server_id.as_str()) else {
                log::warn!(
                    "FolderSync calendar folder {:?} cannot key a Calendar; skipping it",
                    folder.server_id
                );
                return None;
            };
            let mut calendar = Calendar::new(id, folder.display_name.clone());
            calendar
                .extended
                .set(format!("{EXTENDED_NAMESPACE}/class"), json!(folder.class));
            if let Some(folder_type) = folder.folder_type {
                calendar.extended.set(
                    format!("{EXTENDED_NAMESPACE}/folder-type"),
                    json!(folder_type),
                );
            }
            Some(calendar)
        })
        .collect()
}

/// A Calendar's provider key — the store's row identity for it.
fn key_of(calendar: &Calendar) -> ProviderKey {
    calendar.id.key().clone()
}

// ============================================================================
// sync_events — Sync class "Calendar"
// ============================================================================

/// One whole-scope event pass over the bound calendar folder: pages
/// `MoreAvailable` rounds to the end, converts every item through the
/// read-side seam, and recovers a SyncKey invalidation by re-bootstrapping
/// once as a snapshot (see the module docs).
pub(super) async fn sync_events(
    client: &Mutex<EasClient>,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
) -> ProviderResult<ScopeSync<Event>> {
    let mut client = client.lock().await;
    let mut key = request_key(cursor).to_owned();
    // A pass that STARTED from the bootstrap key enumerates everything —
    // snapshot semantics (the present set tombstones at apply time). A pass
    // resumed from a cursor is a delta until an invalidation restarts it.
    let mut snapshot = key == BOOTSTRAP_KEY;
    let mut changed: Vec<Event> = Vec::new();
    let mut removed: Vec<ProviderKey> = Vec::new();
    let mut present: BTreeSet<ProviderKey> = BTreeSet::new();
    loop {
        let result = client
            .sync(&SyncRequest {
                collection_id: calendar.as_str().to_owned(),
                sync_key: key.clone(),
                // Routes the response parser to the Calendar-shaped path
                // (the request builder emits no Class element in 14.0+).
                class: "Calendar".to_owned(),
                // The trait verb has no fetch-batch knob: the drain-loop cap.
                window_size: MAX_WINDOW_SIZE,
                filter_age_days: 0,
                // No wire filter and no body preference — the metadata tier
                // (the email slice's discipline; bodies ride a later slice).
                fetch_body: false,
                truncation_size: None,
                mime_support: None,
                mime_truncation: None,
                supported: None,
            })
            .await
            .map_err(provider_error)?;
        match recovery_action_for_sync(result.status) {
            RecoveryAction::Ok => {}
            // The stored key is dead. Nothing has been handed to the caller
            // yet — the whole-scope result applies atomically — so the
            // accumulated rounds are discarded and the pass restarts from
            // "0" as a snapshot, exactly once (a dead key that IS the
            // bootstrap key falls through to the surface arm below).
            RecoveryAction::ResetSyncKey | RecoveryAction::RunFolderSync if !snapshot => {
                key.clear();
                key.push_str(BOOTSTRAP_KEY);
                snapshot = true;
                changed.clear();
                removed.clear();
                present.clear();
                continue;
            }
            // 3/12 → NeedsResync, 5/16/111 → Retryable, else Permanent —
            // the Sync family classifier.
            _ => return Err(sync_status_error(result.status)),
        }

        // A success response that omits its SyncKey keeps the request's (the
        // empty-key cursor-poisoning invariant).
        let next = next_key(&result.sync_key, &key).to_owned();
        let mut round_items = 0usize;
        for item in result.calendar_added.iter().chain(&result.calendar_updated) {
            // A malformed item is skipped, never failing the pass — the
            // per-resource degrade (`calendar-semantics.md`).
            if item.server_id.is_empty() {
                log::warn!(
                    "Sync calendar item without a ServerId in collection {}; skipping it",
                    calendar.as_str()
                );
                continue;
            }
            let event = calendar_event_from_props(calendar.as_str(), &item.server_id, &item.props);
            present.insert(event.id.key().clone());
            changed.push(event);
            round_items += 1;
        }
        // A from-"0" re-enumeration has no prior server state to delete
        // against — the wire deletes are covered by the pass-end tombstone
        // against `present` (a safe superset), so in snapshot mode they are
        // folded away rather than emitted (the mail stream's Reconcile rule).
        if !snapshot {
            let deletes: Vec<ProviderKey> = result
                .deleted_server_ids
                .iter()
                .filter(|id| !id.is_empty())
                .filter_map(|id| ProviderKey::new(id.clone()).ok())
                .collect();
            round_items += deletes.len();
            removed.extend(deletes);
        }

        // Round control: follow the Exchange 15.2 empty-bootstrap quirk
        // once; complete when no pages remain; treat a MoreAvailable round
        // that made no progress as the pass's end (the drain-loop stall
        // rule).
        let follow = should_follow_empty_bootstrap(&key, round_items, result.more_available, &next);
        let stalled = result.more_available && next == key && round_items == 0;
        if !result.more_available && !follow || stalled {
            let update = if snapshot {
                SyncUpdate::snapshot(changed, present)
            } else {
                SyncUpdate::delta(changed, removed)
            };
            return Ok(ScopeSync::new(update, SyncState::new(next)));
        }
        key = next;
    }
}

/// The shared error for an unusable calendar binding — the trait verb's
/// caller-facing refusal when the adapter was built without one (its
/// capabilities then never advertise the calendar family, so a
/// capability-checking caller never reaches this).
pub(super) fn unbound_calendar() -> ProviderError {
    ProviderError::invalid_state(
        "this EasAdapter is not calendar-bound — build it with \
         EasAdapter::with_calendar before syncing events",
    )
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
