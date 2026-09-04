//! The calendar grid: occurrence rows joined to their masters, one flat page
//! a grid renders from.
//!
//! A grid does not show events; it shows *occurrences* — the rows recurrence
//! materialized — and each row needs its master's facts beside it: the title,
//! the calendar, whether it is all-day or cancelled. Forcing that through the
//! facade's object reads would deserialize every event of the account; the
//! join here resolves only the masters whose occurrences are in the window
//! ([`Engine::occurrences_in`] → [`Engine::events_by_keys`]), so a page costs
//! the page. Both reads are store reads: the grid never touches the network,
//! which is exactly why it answers [`CalendarGridPage::is_materialized`] — a
//! window nothing expanded returns *nothing*, not wrong data, and the flag is
//! how a paging host tells "empty week" from "not yet materialized week"
//! before it calls [`Engine::expand_horizon`] (or the PIM round's window
//! maintenance) and reads again.
//!
//! The join is mechanical and None-safe: a row whose master cannot be
//! resolved (moved, tombstoned, raced) still renders, with the master-derived
//! fields absent rather than guessed. What it deliberately does **not** do is
//! reconcile a recurring master against its sibling override objects —
//! `expand` is a single-event function and cross-object deduplication is the
//! sync layer's job (`engine-recurrence`'s module docs) — so a moved instance
//! renders beside the series' own row for that instant until sync reconciles
//! them.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use engine_api::{AccountId, Engine, Horizon};
use engine_core::{calendar::Event, ids::ProviderKey, sync::ObjectKind, time::UtcDateTime};
use engine_store::{OccurrenceRow, StoreRead as _};
use serde::{Deserialize, Serialize};

/// One occurrence as a grid cell renders it: the materialized instant span,
/// plus the master's facts joined on.
///
/// Pure data — engine-shaped values in, plain serde out — so it crosses the
/// host boundary as a value, exactly like the thread summary rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridOccurrence {
    /// The master event's provider key, as text.
    pub event: String,
    /// The master's first calendar membership, as text; `None` when the
    /// master could not be resolved.
    pub calendar: Option<String>,
    /// The master's title; `None` when the master could not be resolved.
    pub title: Option<String>,
    /// The occurrence start instant.
    pub start: UtcDateTime,
    /// The occurrence end instant (exclusive).
    pub end: UtcDateTime,
    /// Whether the master is an all-day (date-only) event.
    pub all_day: bool,
    /// The `RECURRENCE-ID` instant when this row is an overridden instance.
    pub recurrence_id: Option<UtcDateTime>,
    /// Whether the master is cancelled.
    pub cancelled: bool,
}

/// One page of the grid: the joined occurrences over the requested window,
/// and whether that window is materialized at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarGridPage {
    /// The occurrences, ascending by the store's own occurrence order.
    pub occurrences: Vec<GridOccurrence>,
    /// Whether the requested window lies within the store's persisted
    /// expansion window for the account's event scopes — the flag that says
    /// "an empty page is really empty" rather than "never expanded".
    pub is_materialized: bool,
}

/// The calendar-grid read over an engine's store.
///
/// Implemented for `engine_api::Engine` here rather than in the facade
/// because the grid is a *host* read model, the [`ThreadsRead`](crate::ThreadsRead)
/// precedent: the facade stays the engine's own object-level surface, and the
/// orphan rule would keep a foreign trait off its type anyway.
#[async_trait]
pub trait CalendarGridRead {
    /// Reads the account's grid over `window`: occurrence rows joined to
    /// their masters, no network.
    ///
    /// # Errors
    ///
    /// Returns the backend's message when a store read fails.
    async fn calendar_grid(
        &self,
        account: &AccountId,
        window: Horizon,
    ) -> Result<CalendarGridPage, String>;
}

#[async_trait]
impl CalendarGridRead for Engine {
    async fn calendar_grid(
        &self,
        account: &AccountId,
        window: Horizon,
    ) -> Result<CalendarGridPage, String> {
        // The rows first — they bound the masters needed, never the reverse.
        let rows = self
            .occurrences_in(account, window)
            .await
            .map_err(|err| err.to_string())?;
        let wanted: HashSet<ProviderKey> = rows.iter().map(|row| row.event.clone()).collect();
        let masters: HashMap<ProviderKey, Event> = self
            .events_by_keys(account, &wanted.into_iter().collect::<Vec<_>>())
            .await
            .map_err(|err| err.to_string())?
            .into_iter()
            .map(|event| (event.id.key().clone(), event))
            .collect();
        let occurrences = rows
            .iter()
            .map(|row| joined(row, masters.get(&row.event)))
            .collect();
        let is_materialized = materialized(self, account, window).await?;
        Ok(CalendarGridPage {
            occurrences,
            is_materialized,
        })
    }
}

/// Whether `window` lies within every event scope's persisted expansion
/// horizon for the account, read through the host store seam.
///
/// The store owns this fact (the `sync_scope` row's persisted
/// [`ExpansionWindow`](engine_core::time::ExpansionWindow) horizon); the
/// zone it was expanded under is deliberately not compared — a zone change
/// is a re-expansion decision, not a coverage fact. An account with no
/// event scopes has never synced its calendars, which is the plain `false`
/// of "nothing is materialized".
async fn materialized(
    engine: &Engine,
    account: &AccountId,
    window: Horizon,
) -> Result<bool, String> {
    let scopes = engine
        .host_store()
        .account_scopes(account.clone())
        .await
        .map_err(|err| err.to_string())?;
    let mut seen = false;
    for scope in scopes
        .iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Event))
    {
        seen = true;
        let Some(persisted) = engine
            .host_store()
            .expansion_window(scope)
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(false);
        };
        if persisted.horizon.start() > window.start() || persisted.horizon.end() < window.end() {
            return Ok(false);
        }
    }
    Ok(seen)
}

/// Joins one occurrence row to its master's display facts.
///
/// `None`-safe by construction: every master-derived field falls back to its
/// absent form when the master did not resolve, so a row is never dropped or
/// guessed at. The calendar is the master's *first* membership (memberships
/// are an ordered set, so "first" is stable); an empty-string title is still
/// `Some` — an untitled event is a fact about the event, distinct from an
/// unresolvable one.
fn joined(row: &OccurrenceRow, master: Option<&Event>) -> GridOccurrence {
    GridOccurrence {
        event: row.event.as_str().to_owned(),
        calendar: master
            .and_then(|event| event.calendars.iter().next())
            .map(|calendar| calendar.as_str().to_owned()),
        title: master.map(|event| event.title.clone()),
        start: row.start,
        end: row.end,
        all_day: master.is_some_and(Event::is_all_day),
        recurrence_id: row.recurrence_id,
        cancelled: master.is_some_and(Event::is_cancelled),
    }
}

#[cfg(test)]
#[path = "grid_tests.rs"]
mod grid_tests;
