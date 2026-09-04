//! The PIM round: one account, calendars then contacts, every fact reported
//! as it lands.
//!
//! [`run_pim_round`] is the PIM counterpart of the mail round
//! ([`run_account_round`](crate::run_account_round)) a host's scheduler calls
//! on the same timer — and it holds to the same posture: composition, no
//! policy. It syncs the account's calendars, keeps the materialization window
//! honest against the requested horizon, drains the durable outbox's calendar
//! ops, then runs the identical pass over contacts, telling the sink one fact
//! per change and one depth per drain that settled something. What it does
//! **not** do is everything a scheduler owns: no timing, no loop, no backoff —
//! when to run again is the caller's.
//!
//! # The change-emission discipline
//!
//! `CalendarChanged`/`ContactsChanged` fire only when a scope's rows actually
//! moved: a sync report carrying upserts or tombstones, or a horizon advance
//! that materialized occurrence rows. The mail round reports every *chunk*
//! because its progress surface needs the heartbeat; the PIM scopes have no
//! per-chunk observer and no progress surface — a quiet delta is not news, and
//! a host that re-reads on every event would just re-render the same grid. The
//! mirror of the mail round's "an outbox nothing touched is not news" holds
//! exactly: a round that changed nothing emits nothing.
//!
//! # The window maintenance
//!
//! A sync expands only the objects its delta *changed*, over the window the
//! store already holds — so a delta with no changes materializes nothing, and
//! a host that widened its horizon would read a confidently empty week
//! forever. The round closes that itself: after a successful calendar sync
//! whose persisted window is narrower than the requested horizon, it runs
//! [`Engine::expand_horizon`] — the one call that re-expands every stored
//! event and moves the window — before it emits, so the change event the host
//! hears already covers the materialization. A window that already covers the
//! horizon (including one a previous round widened further) is left alone:
//! widening is maintenance, narrowing is data loss.
//!
//! # Failure semantics
//!
//! The PIM facade verbs are all-or-nothing per scope — there is no per-folder
//! report the mail round's partial failures ride — so the round propagates the
//! engine's own [`ApiError`] from the first verb that fails and stops there:
//! no drain of a scope whose sync did not land, and no contacts pass under a
//! failed calendar sync. The store keeps whatever committed before the fault,
//! and the next round is a plain retry. Outbox claims are scope-blind, so a
//! contact op the calendar drain happens to claim waits out one lease TTL
//! (the facade's documented skip cost) and is the next round's.

use engine_api::{
    AccountId, ApiError, CalendarSyncReport, ContactSyncReport, Engine, Horizon, HorizonExpansion,
    SyncApplied,
};
use engine_core::time::TimeZoneId;
use engine_provider::{ContactsProvider, Provider};
use engine_store::StoreRead as _;

use crate::events::{EngineEvent, EventSink};

/// What one PIM round did: both scopes' sync reports, and how many outbox ops
/// each drain drove to a recorded outcome.
#[derive(Debug)]
pub struct PimRoundReport {
    /// The calendar sync's per-scope report — which containers and events
    /// landed, and what the expander refused — returned whole, like the mail
    /// round's own `sync`.
    pub calendar: CalendarSyncReport,
    /// The contacts sync's report: discovery, cards, and the people rebuild.
    pub contacts: ContactSyncReport,
    /// How many calendar ops this round's drain drove to a recorded outcome,
    /// under the facade drain's own accounting.
    pub drained_cal: usize,
    /// How many contact ops this round's drain drove to a recorded outcome.
    pub drained_contacts: usize,
}

/// Drives one PIM round: calendar sync and window maintenance, calendar
/// drain, contacts sync, contacts drain.
///
/// The six steps, in order: `Engine::sync_calendar` over `(horizon,
/// host_zone)`; the window check — `Engine::expand_horizon` when the store's
/// persisted window for the synced event scope is narrower than `horizon`
/// (see the module docs); one `CalendarChanged` when the calendar's rows
/// moved; `Engine::drain_calendar_ops`, with one `OutboxChanged` at the depth
/// the drain left when it settled anything; then the identical pair over
/// contacts — `Engine::sync_contacts`, one `ContactsChanged` on change,
/// `Engine::drain_contact_ops` with its own depth event. No timers and no
/// loops; the sink is told everything exactly once, in emission order.
///
/// # Errors
///
/// Propagates the engine's own [`ApiError`] from the first verb that fails —
/// see the module docs' failure semantics.
pub async fn run_pim_round<P: ContactsProvider>(
    engine: &Engine,
    provider: &P,
    account: &AccountId,
    horizon: Horizon,
    host_zone: &TimeZoneId,
    sink: &dyn EventSink,
) -> Result<PimRoundReport, ApiError> {
    let name = account.as_str().to_owned();

    // Calendar half: sync, keep the window honest, emit, drain.
    let calendar = engine
        .sync_calendar(provider, account, horizon, host_zone)
        .await?;
    let expanded = widen_window(engine, provider, account, horizon, host_zone).await?;
    let calendar_moved = carries_changes(&calendar.calendars)
        || carries_changes(&calendar.events.applied)
        || expanded.is_some_and(|pass| pass.occurrences > 0);
    if calendar_moved {
        sink.emit(EngineEvent::CalendarChanged {
            account: name.clone(),
        });
    }
    let drained_cal = engine.drain_calendar_ops(provider, account).await?;
    report_drain(engine, account, &name, drained_cal, sink).await;

    // Contacts half: sync, emit, drain.
    let contacts = engine.sync_contacts(provider, account).await?;
    let contacts_moved = carries_changes(&contacts.address_books.applied)
        || carries_changes(&contacts.cards.applied);
    if contacts_moved {
        sink.emit(EngineEvent::ContactsChanged {
            account: name.clone(),
        });
    }
    let drained_contacts = engine.drain_contact_ops(provider, account).await?;
    report_drain(engine, account, &name, drained_contacts, sink).await;

    Ok(PimRoundReport {
        calendar,
        contacts,
        drained_cal,
        drained_contacts,
    })
}

/// Whether an apply count set says the scope's rows moved: an upsert or a
/// tombstone is a change a re-reading host must hear about. A reconciled
/// pending op moves no object rows — the object it confirms lands through the
/// upserts — and the people rebuild is derived from the cards, so neither
/// counts on its own.
fn carries_changes(applied: &SyncApplied) -> bool {
    applied.upserted > 0 || applied.tombstoned > 0
}

/// Advances the store's expansion window to `horizon` when it is narrower,
/// through the engine's own maintenance call.
///
/// The scope checked is the one this round's `sync_calendar` just seeded or
/// synced (`Provider::event_scope`); other providers' scopes under the same
/// account belong to their own rounds. A window that already covers the
/// horizon — including one a previous round widened further — is left
/// untouched: this is widening, never narrowing, and never a redundant
/// re-expansion of a window that already matches. `Ok(None)` means no pass
/// was needed.
async fn widen_window<P: Provider>(
    engine: &Engine,
    provider: &P,
    account: &AccountId,
    horizon: Horizon,
    host_zone: &TimeZoneId,
) -> Result<Option<HorizonExpansion>, ApiError> {
    let scope = provider.event_scope(account);
    let persisted = engine.host_store().expansion_window(&scope).await?;
    let narrower = persisted.as_ref().is_none_or(|window| {
        window.horizon.start() > horizon.start() || window.horizon.end() < horizon.end()
    });
    if !narrower {
        return Ok(None);
    }
    Ok(Some(
        engine.expand_horizon(account, horizon, host_zone).await?,
    ))
}

/// Emits one `OutboxChanged` at the depth a drain left, when the drain settled
/// anything — the mail round's rule that an outbox nothing touched is not
/// news, and a round that drained nothing stays change-events-only.
async fn report_drain(
    engine: &Engine,
    account: &AccountId,
    name: &str,
    drained: usize,
    sink: &dyn EventSink,
) {
    if drained == 0 {
        return;
    }
    let pending = crate::round::outbox_depth(engine, account).await;
    sink.emit(EngineEvent::OutboxChanged {
        account: name.to_owned(),
        pending,
    });
}

#[cfg(test)]
#[path = "pim_tests.rs"]
mod pim_tests;
