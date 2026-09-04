// SPDX-License-Identifier: MPL-2.0
//! The calendar write verbs: `create_event` / `patch_event` /
//! `delete_event` over the Sync `Commands` upsync (P2 Task 3), and the
//! documented `put_event` refusal.
//!
//! ## Mapping
//!
//! **`create_event` → Sync `Add`** with a synthesized `ClientId`
//! (`new_calendar_client_id` — the ≤40-char cap guaranteed). The Add is the
//! only id-reveal point: the server answers under the response Collection's
//! `Responses` element with the `ServerId` it assigned ([MS-ASCMD]
//! §2.2.3.7.2), and the receipt keys it. A success with **no** ack means
//! success-with-no-id (§2.2.3.154 — acks ride successes, but a conforming
//! server MAY omit the element); the receipt then keys the `ClientId`
//! placeholder, which the next `sync_events` pass reconciles away by `uid`.
//!
//! **`patch_event` → Sync `Change`** (a Replace in the crate's vocabulary):
//! `PatchTarget::Series` rebuilds the master's complete document from the
//! base (`calendar::convert_write::write_from_series` — the document
//! discipline); `PatchTarget::Instance` rebuilds the master carrying the
//! target occurrence as a modified exception (`write_exception`). An empty
//! patch is a no-op receipt — no wire round, nothing to ack.
//!
//! **`delete_event` → Sync `Delete`** for the series; a
//! `DeleteTarget::Occurrence` is a `Change` of the master carrying the
//! deleted-marker exception (`write_occurrence_deleted` — the EAS EXDATE
//! form). **Already-gone is success** (the trait's idempotent-delete rule):
//! a per-item status 8 ("Object not found", [MS-ASCMD] §2.2.3.177.17)
//! resolves cleanly, and per §2.2.3.154 the absence of any item status at
//! all IS the success shape — statuses under `Responses` ride failures
//! only.
//!
//! **`put_event` is refused** ([`put_refusal`]): EAS's update verb is a
//! field-level Sync `Change`, not a document PUT — the trait's rejecting
//! default for transports whose update is already a patch, and the
//! `EventWrite.ical` payload has no EAS form at all (there is no iCalendar
//! on an EAS server).
//!
//! ## The collection key
//!
//! Every verb rides the adapter's **calendar collection-key ledger**
//! (`EasAdapter.calendar_key` — the mail `edit_mail` precedent): seeded by
//! a completed `sync_events` pass, consumed and rotated by each write. A
//! cold ledger refuses `NeedsResync` rather than guessing; a dead key
//! surfaces as Sync status 3 through the family classifier.
//!
//! Per-item statuses: a **failed** Add ack or Change/Delete item status
//! surfaces as an error naming the code (6 = conversion error, permanent —
//! the live-probed organizer rejection's class; 8 = object not found).

use engine_core::{
    calendar::Event,
    error::FailureClass,
    ids::{CalendarId, EventId},
    version::RevisionTokens,
};
use engine_provider::{
    DeleteTarget, EventDeletion, EventDraft, EventEdit, EventWriteReceipt, PatchTarget,
    ProviderError, ProviderResult,
};
use tokio::sync::Mutex;

use super::{
    CollectionKey, current_key,
    error::{provider_error, sync_status_error},
    record_rotation,
};
use crate::{
    calendar::convert_write,
    client::{EasClient, EasError},
    commands::{CalendarAddAck, CalendarChange, CalendarItemStatus, SyncChangeOutcome},
    types::new_calendar_client_id,
};

/// The per-item "object not found" status ([MS-ASCMD] §2.2.3.177.17): the
/// delete's already-gone success, and a patch's refetch signal.
const ITEM_OBJECT_NOT_FOUND: u32 = 8;

/// Creates an event: Sync `Add` over the bound calendar's collection, the
/// receipt keying the `ServerId` the server's ack assigns (see the module
/// docs for the ack-less success shape).
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; an unconvertible draft refuses
/// `Permanent` before the wire; a failed collection status classifies
/// through the Sync family table; a failed Add ack surfaces with its item
/// status.
pub(super) async fn create(
    client: &Mutex<EasClient>,
    calendar: &CalendarId,
    ledger: &CollectionKey,
    draft: &EventDraft,
) -> ProviderResult<EventWriteReceipt> {
    let props = convert_write::write_from_draft(draft)?;
    let client_id = new_calendar_client_id();
    let key = current_key(ledger)?;
    let outcome = upsync(
        client,
        calendar,
        &key,
        &[CalendarChange::Add {
            client_id: client_id.clone(),
            props,
        }],
    )
    .await?;
    record_rotation(ledger, &outcome);
    let assigned = outcome
        .add_acks
        .iter()
        .find(|ack| ack.client_id == client_id);
    let id = match assigned {
        // §2.2.3.7.2: the ack carries the assigned ServerId on success.
        Some(ack) if ack.success() => ack
            .server_id
            .clone()
            .ok_or_else(|| ack_without_an_id(ack))?,
        // A failed ack is the server's rejection of the item itself —
        // permanent (6 = conversion error is the live-probed class).
        Some(ack) => {
            return Err(ProviderError::permanent(format!(
                "the server rejected the calendar Add (item status {}): {}",
                ack.status,
                crate::commands::common_status_message(ack.status)
                    .unwrap_or("no further detail available")
            )));
        }
        // §2.2.3.154: no ack means success with no id to correlate — the
        // ClientId placeholder reconciles away on the next events pass.
        None => {
            log::debug!(
                "EAS calendar Add succeeded without a Responses ack; the receipt keys the \
                 ClientId placeholder until the next events pass"
            );
            client_id
        }
    };
    let event = EventId::try_from(id.as_str()).map_err(|e| {
        ProviderError::permanent(format!(
            "the server assigned a ServerId that cannot key an event: {e}"
        ))
    })?;
    Ok(EventWriteReceipt::new(
        event,
        draft.uid.clone(),
        RevisionTokens::default(),
    ))
}

/// Applies an edit: Sync `Change` over the master — a complete-document
/// Replace for a series target, the exception-carrying master for an
/// instance target (see the module docs).
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; the conversion refusals of
/// `convert_write` (form changes, unrepresentable rules/overrides) surface
/// verbatim; a failed collection status classifies through the Sync family
/// table; a failed item status surfaces with its code (8 as `Conflict` —
/// refetch, re-apply, never blind-retry).
pub(super) async fn patch(
    client: &Mutex<EasClient>,
    calendar: &CalendarId,
    ledger: &CollectionKey,
    base: &Event,
    edit: &EventEdit,
) -> ProviderResult<EventWriteReceipt> {
    // An empty patch changes nothing but its stamp — no wire round, and the
    // receipt simply records the event as it stands.
    if edit.patch.is_empty() {
        return Ok(EventWriteReceipt::new(
            edit.event.clone(),
            edit.uid.clone(),
            RevisionTokens::default(),
        ));
    }
    let props = match &edit.target {
        PatchTarget::Series => convert_write::write_from_series(base, &edit.patch)?,
        PatchTarget::Instance(occurrence) => {
            convert_write::write_exception(base, occurrence, &edit.patch)?
        }
    };
    let key = current_key(ledger)?;
    let outcome = upsync(
        client,
        calendar,
        &key,
        &[CalendarChange::Replace {
            server_id: edit.event.as_str().to_owned(),
            props,
        }],
    )
    .await?;
    record_rotation(ledger, &outcome);
    check_item_status(&outcome, edit.event.as_str(), "Change")?;
    Ok(EventWriteReceipt::new(
        edit.event.clone(),
        edit.uid.clone(),
        RevisionTokens::default(),
    ))
}

/// Deletes an event or one occurrence: Sync `Delete` for the series, the
/// deleted-marker exception under a `Change` of the master for an
/// occurrence (see the module docs). Already-gone is success.
///
/// # Errors
///
/// A cold ledger refuses `NeedsResync`; an occurrence delete without the
/// base series refuses `InvalidState`; a failed collection status
/// classifies through the Sync family table; a failed item status surfaces
/// with its code.
pub(super) async fn delete(
    client: &Mutex<EasClient>,
    calendar: &CalendarId,
    ledger: &CollectionKey,
    base: Option<&Event>,
    deletion: &EventDeletion,
) -> ProviderResult<()> {
    let change = match &deletion.target {
        DeleteTarget::Series => CalendarChange::Remove {
            server_id: deletion.event.as_str().to_owned(),
        },
        DeleteTarget::Occurrence { occurrence, .. } => {
            // The occurrence form is a rewrite of the series document — the
            // base as the caller read it is the only source for the
            // master's own fields and its other overrides. CalDAV says so
            // loudly; EAS needs it no less.
            let Some(base) = base else {
                return Err(ProviderError::invalid_state(
                    "deleting one occurrence rewrites the series document — pass the event as \
                     the caller read it (base), not just the deletion",
                ));
            };
            CalendarChange::Replace {
                server_id: deletion.event.as_str().to_owned(),
                props: convert_write::write_occurrence_deleted(base, occurrence)?,
            }
        }
    };
    let key = current_key(ledger)?;
    let outcome = upsync(client, calendar, &key, &[change]).await?;
    record_rotation(ledger, &outcome);
    // A per-item 8 is the idempotent delete's already-gone success; any
    // other surfaced item status is the failure the server reported.
    if let Some(status) = item_status(&outcome, deletion.event.as_str()) {
        if status.status != ITEM_OBJECT_NOT_FOUND {
            return Err(item_status_error(status, "Delete"));
        }
        log::debug!(
            "EAS calendar delete answered object-not-found (status 8) — already gone, the \
             idempotent success"
        );
    }
    Ok(())
}

/// The `put_event` refusal: EAS has no whole-document calendar write (see
/// the module docs). Kept as a value so the trait half stays a one-liner
/// with one source of truth.
pub(super) fn put_refusal() -> ProviderError {
    ProviderError::invalid_state(
        "EAS has no whole-document calendar write — its update verb is a field-level Sync \
         Change (patch_event), and an EAS server stores no iCalendar document to PUT",
    )
}

/// One calendar upsync round: the verb lock, the Sync-family status
/// classification, the plain transport map.
async fn upsync(
    client: &Mutex<EasClient>,
    calendar: &CalendarId,
    key: &str,
    changes: &[CalendarChange],
) -> ProviderResult<SyncChangeOutcome> {
    let mut client = client.lock().await;
    match client
        .calendar_sync_changes(calendar.as_str(), key, changes)
        .await
    {
        Ok(outcome) => Ok(outcome),
        // The upsync's own statuses are Sync-family (a dead key is 3): the
        // classifier, never the family-blind error text.
        Err(EasError::SyncStatus { status, .. } | EasError::CommandStatus { status, .. }) => {
            Err(sync_status_error(status))
        }
        Err(e) => Err(provider_error(e)),
    }
}

/// The item status a Change/Delete response carried for `server_id`, if
/// any — per [MS-ASCMD] §2.2.3.154 presence IS the failure signal.
fn item_status<'a>(
    outcome: &'a SyncChangeOutcome,
    server_id: &str,
) -> Option<&'a CalendarItemStatus> {
    outcome
        .item_statuses
        .iter()
        .find(|status| status.server_id == server_id)
}

/// Surfaces a failed item status for a patch (8 = the object is gone =
/// `Conflict`: refetch and re-apply, never blind-retry).
fn check_item_status(
    outcome: &SyncChangeOutcome,
    server_id: &str,
    verb: &str,
) -> ProviderResult<()> {
    if let Some(status) = item_status(outcome, server_id) {
        if status.status == ITEM_OBJECT_NOT_FOUND {
            return Err(ProviderError::new(
                FailureClass::Conflict,
                format!(
                    "the calendar item is gone server-side (item status 8) — refetch the \
                     event and re-apply the {verb}"
                ),
            ));
        }
        return Err(item_status_error(status, verb));
    }
    Ok(())
}

/// The shared failed-item-status error, naming the code and its meaning.
fn item_status_error(status: &CalendarItemStatus, verb: &str) -> ProviderError {
    ProviderError::permanent(format!(
        "the server rejected the calendar {verb} for {} (item status {}): {}",
        status.server_id,
        status.status,
        crate::commands::common_status_message(status.status)
            .unwrap_or("no further detail available")
    ))
}

/// The ack-success-without-an-id anomaly: §2.2.3.7.2 assigns the ServerId
/// on success, so a success ack carrying none is a server-shape violation
/// worth naming rather than papering over.
fn ack_without_an_id(ack: &CalendarAddAck) -> ProviderError {
    ProviderError::permanent(format!(
        "the Add ack for {} reported success but carried no ServerId ([MS-ASCMD] §2.2.3.7.2 \
         assigns one) — retry the create; the next events pass reconciles any duplicate by uid",
        ack.client_id
    ))
}

#[cfg(test)]
#[path = "calendar_write_tests.rs"]
mod tests;
