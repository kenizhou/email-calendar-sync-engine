//! The calendar grid read over a seeded in-memory `Engine`: the
//! occurrence-to-master join a grid renders from, and the materialization flag
//! a paging host checks before it believes an empty week. No provider verbs
//! run inside the read — the seeding happens through `sync_calendar` before
//! any assertion, and the read itself touches only the store.

use core::num::NonZeroU32;

use engine_api::{AccountId, Engine, Horizon};
use engine_core::{
    calendar::{
        Calendar, Event, EventStatus, Frequency, Recurrence, RecurrenceBound, RecurrenceRule,
    },
    ids::{CalendarId, EventId, ProviderKey, Uid},
    membership::Memberships,
    sync::{SyncState, SyncUpdate},
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_provider::{Capabilities, ConnectionInfo, Provider, ProviderResult, ScopeSync};
use engine_store::OccurrenceRow;

use crate::grid::{CalendarGridPage, CalendarGridRead as _, GridOccurrence};

fn account() -> AccountId {
    AccountId::try_from("acct-1").expect("valid account")
}

fn zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").expect("valid zone")
}

/// March 2026, the window every seeding sync materializes.
fn horizon() -> Horizon {
    Horizon::new(
        "2026-03-01T00:00:00Z".parse().expect("valid instant"),
        "2026-04-01T00:00:00Z".parse().expect("valid instant"),
    )
    .expect("valid horizon")
}

fn at_utc(raw: &str) -> CalendarDateTime {
    let local: LocalDateTime = raw.parse().expect("valid time");
    CalendarDateTime::utc(local)
}

fn calendar() -> Calendar {
    Calendar::new(CalendarId::try_from("work").expect("valid id"), "Work")
}

fn in_work(id: &str, uid: &str, start: CalendarDateTime) -> Event {
    Event::new(
        EventId::try_from(id).expect("valid id"),
        Uid::new(uid).expect("valid uid"),
        Memberships::of_one(CalendarId::try_from("work").expect("valid id")),
        start,
    )
}

/// A single one-hour meeting on March 2nd.
fn single() -> Event {
    let mut event = in_work("evt-1", "uid-1@h", at_utc("2026-03-02T09:00:00"));
    event.title = "Sprint planning".to_owned();
    event.duration = "PT1H".parse().expect("valid duration");
    event
}

/// A weekly standup, three occurrences from March 2nd.
fn weekly() -> Event {
    let mut event = in_work("evt-2", "uid-2@h", at_utc("2026-03-02T10:00:00"));
    event.title = "Standup".to_owned();
    event.duration = "PT30M".parse().expect("valid duration");
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(3).expect("non-zero"));
    event.recurrence = Some(Recurrence::from_rule(rule));
    event
}

/// An all-day event on March 3rd — the `Date` form, which materializes as a
/// UTC-midnight-to-midnight row regardless of the host zone.
fn all_day() -> Event {
    let mut event = in_work(
        "evt-3",
        "uid-3@h",
        CalendarDateTime::Date(CalendarDate::new(2026, 3, 3).expect("valid date")),
    );
    event.title = "Conference".to_owned();
    event.duration = "P1D".parse().expect("valid duration");
    event
}

/// An unbounded weekly Monday standup starting March 2nd — the series the
/// expansion test materializes into May with.
fn weekly_unbounded() -> Event {
    let mut event = in_work("evt-u", "uid-u@h", at_utc("2026-03-02T10:00:00"));
    event.title = "Standup".to_owned();
    event.duration = "PT30M".parse().expect("valid duration");
    event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
        Frequency::Weekly,
    )));
    event
}

/// A standalone override-instance object for the standup's March 9th instance,
/// moved to 11:00 — its own event object (its own provider id) carrying a
/// `recurrence_id`, exactly as a CalDAV override arrives.
fn moved_instance() -> Event {
    let mut event = in_work("evt-2-1", "uid-2@h", at_utc("2026-03-09T11:00:00"));
    event.title = "Standup (moved)".to_owned();
    event.duration = "PT30M".parse().expect("valid duration");
    event.recurrence_id = Some(at_utc("2026-03-09T10:00:00"));
    event
}

/// A minimal in-memory calendar provider — snapshot on the first sync of each
/// scope, an empty delta once a cursor exists.
struct GridPim {
    calendars: Vec<Calendar>,
    events: Vec<Event>,
}

#[async_trait::async_trait]
impl Provider for GridPim {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_calendars())
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("cal-2"),
            ));
        }
        let present = self.calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.calendars.clone(), present),
            SyncState::new("cal-1"),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("events-2"),
            ));
        }
        let present = self.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.events.clone(), present),
            SyncState::new("events-1"),
        ))
    }
}

/// Seeds the full fixture set: one calendar holding the single, the weekly
/// series, the all-day event, and the moved instance.
async fn seeded(engine: &Engine) {
    seed_with(
        engine,
        vec![single(), weekly(), all_day(), moved_instance()],
    )
    .await;
}

/// Seeds one calendar holding exactly `events` over the standing horizon.
async fn seed_with(engine: &Engine, events: Vec<Event>) {
    let provider = GridPim {
        calendars: vec![calendar()],
        events,
    };
    engine
        .sync_calendar(&provider, &account(), horizon(), &zone())
        .await
        .expect("the seed sync completes");
}

/// The page's rows for one event key, in the page's order.
fn rows_for<'a>(page: &'a CalendarGridPage, event: &str) -> Vec<&'a GridOccurrence> {
    page.occurrences
        .iter()
        .filter(|row| row.event == event)
        .collect()
}

#[tokio::test]
async fn a_grid_joins_each_occurrence_to_its_master() {
    let engine = Engine::open_in_memory().expect("engine");
    seeded(&engine).await;

    let page = engine
        .calendar_grid(&account(), horizon())
        .await
        .expect("the grid reads");

    assert!(
        page.is_materialized,
        "the requested window is the one the sync seeded"
    );
    // Single + three series occurrences + the all-day + the moved instance.
    assert_eq!(page.occurrences.len(), 6);

    // The single meeting: the master's title, calendar, and window.
    let rows = rows_for(&page, "evt-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("Sprint planning"));
    assert_eq!(rows[0].calendar.as_deref(), Some("work"));
    assert_eq!(
        rows[0].start,
        "2026-03-02T09:00:00Z".parse().expect("instant")
    );
    assert_eq!(
        rows[0].end,
        "2026-03-02T10:00:00Z".parse().expect("instant")
    );
    assert!(!rows[0].all_day);
    assert!(!rows[0].cancelled);
    assert_eq!(rows[0].recurrence_id, None);

    // The series: three occurrences, each joined back to the one master.
    let rows = rows_for(&page, "evt-2");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|row| row.title.as_deref() == Some("Standup"))
    );
    assert!(rows.iter().all(|row| row.recurrence_id.is_none()));

    // The all-day event: flagged by its master's `Date` start, materialized as
    // the UTC-midnight span the store expands date-only values to.
    let rows = rows_for(&page, "evt-3");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].all_day);
    assert_eq!(rows[0].title.as_deref(), Some("Conference"));
    assert_eq!(
        rows[0].start,
        "2026-03-03T00:00:00Z".parse().expect("instant")
    );
    assert_eq!(
        rows[0].end,
        "2026-03-04T00:00:00Z".parse().expect("instant")
    );

    // The moved instance: its own object is the master of its row, and the row
    // carries the `RECURRENCE-ID` it overrides. (The master's own March 9th row
    // still stands beside it — master/override reconciliation is the sync
    // layer's job, not the read's.)
    let rows = rows_for(&page, "evt-2-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("Standup (moved)"));
    assert_eq!(
        rows[0].recurrence_id,
        Some("2026-03-09T10:00:00Z".parse().expect("instant"))
    );
    assert_eq!(
        rows[0].start,
        "2026-03-09T11:00:00Z".parse().expect("instant")
    );
}

#[tokio::test]
async fn a_window_past_the_materialized_horizon_flags_itself_unmaterialized() {
    let engine = Engine::open_in_memory().expect("engine");
    seeded(&engine).await;

    // The seeded window answers materialized.
    let inside = engine
        .calendar_grid(&account(), horizon())
        .await
        .expect("the grid reads");
    assert!(inside.is_materialized);

    // April alone extends past the seeded window's end: unmaterialized, and
    // empty — not wrong, just not yet expanded.
    let april = Horizon::new(
        "2026-04-01T00:00:00Z".parse().expect("valid instant"),
        "2026-05-01T00:00:00Z".parse().expect("valid instant"),
    )
    .expect("valid horizon");
    let outside = engine
        .calendar_grid(&account(), april)
        .await
        .expect("the grid reads");
    assert!(!outside.is_materialized);
    assert!(
        outside.occurrences.is_empty(),
        "nothing was materialized into April"
    );
}

#[tokio::test]
async fn expanding_the_horizon_rematerializes_and_flips_the_flag_back() {
    let engine = Engine::open_in_memory().expect("engine");
    // The unbounded series: it keeps recurring past the seeded window, so the
    // expansion is visible as rows, not only as the flag.
    seed_with(&engine, vec![weekly_unbounded()]).await;

    let may = Horizon::new(
        "2026-05-01T00:00:00Z".parse().expect("valid instant"),
        "2026-06-01T00:00:00Z".parse().expect("valid instant"),
    )
    .expect("valid horizon");
    let before = engine
        .calendar_grid(&account(), may)
        .await
        .expect("the grid reads");
    assert!(!before.is_materialized);
    assert!(before.occurrences.is_empty());

    // The maintenance call the round (or a paging host) makes before reading
    // past the window.
    engine
        .expand_horizon(
            &account(),
            Horizon::new(
                "2026-03-01T00:00:00Z".parse().expect("valid instant"),
                "2026-06-01T00:00:00Z".parse().expect("valid instant"),
            )
            .expect("valid horizon"),
            &zone(),
        )
        .await
        .expect("the horizon expands");

    let after = engine
        .calendar_grid(&account(), may)
        .await
        .expect("the grid reads");
    assert!(after.is_materialized);
    assert_eq!(after.occurrences.len(), 4, "May's four Mondays appear");
    assert!(
        after
            .occurrences
            .iter()
            .all(|row| row.title.as_deref() == Some("Standup")),
        "the joined master facts ride on every materialized row"
    );
}

#[test]
fn a_row_whose_master_is_absent_joins_to_nothing_optional() {
    // The defensive join: an occurrence row whose master cannot be resolved
    // (a key the payload read did not answer) still renders — with no title,
    // no calendar, and none of the master-derived flags guessed at.
    let row = OccurrenceRow {
        event: ProviderKey::new("evt-gone").expect("valid key"),
        start: "2026-03-02T09:00:00Z".parse().expect("instant"),
        end: "2026-03-02T10:00:00Z".parse().expect("instant"),
        recurrence_id: None,
        tzdata_version: engine_store::TzdataVersion::new("2025b"),
    };
    let joined = crate::grid::joined(&row, None);
    assert_eq!(joined.event, "evt-gone");
    assert_eq!(joined.title, None);
    assert_eq!(joined.calendar, None);
    assert!(!joined.all_day);
    assert!(!joined.cancelled);
}

#[test]
fn a_row_whose_master_is_cancelled_marks_itself_cancelled() {
    // A cancelled master is a tombstone the expander materializes no rows for,
    // so this shape cannot arise from a clean sync — but a row that does point
    // at a cancelled master (a stale row, a raced read) must render struck
    // through rather than confirmed.
    let mut master = single();
    master.status = EventStatus::Cancelled;
    let row = OccurrenceRow {
        event: master.id.key().clone(),
        start: "2026-03-02T09:00:00Z".parse().expect("instant"),
        end: "2026-03-02T10:00:00Z".parse().expect("instant"),
        recurrence_id: None,
        tzdata_version: engine_store::TzdataVersion::new("2025b"),
    };
    let joined = crate::grid::joined(&row, Some(&master));
    assert!(joined.cancelled);
    assert_eq!(joined.title.as_deref(), Some("Sprint planning"));
}

#[test]
fn a_grid_page_round_trips_through_serde() {
    let page = CalendarGridPage {
        occurrences: vec![GridOccurrence {
            event: "evt-1".to_owned(),
            calendar: Some("work".to_owned()),
            title: Some("Sprint planning".to_owned()),
            start: "2026-03-02T09:00:00Z".parse().expect("instant"),
            end: "2026-03-02T10:00:00Z".parse().expect("instant"),
            all_day: false,
            recurrence_id: None,
            cancelled: false,
        }],
        is_materialized: true,
    };
    let json = serde_json::to_string(&page).expect("serializes");
    assert_eq!(
        serde_json::from_str::<CalendarGridPage>(&json).expect("deserializes"),
        page
    );
}
