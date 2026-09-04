//! The PIM round driven end to end over a real in-memory `Engine`: the fake
//! JMAP-shaped provider and fixtures live beside this file (`fake.rs`), and
//! every assertion reads the event stream a `CollectingSink` heard — by
//! content, not by count.

// The sibling file this includes sits under `pim_tests/`, beside this file —
// an explicit path because a plain `mod fake;` inside a `#[path]`-included
// module resolves against the parent directory instead.
#[path = "pim_tests/fake.rs"]
mod fake;

use engine_api::{ApiError, Engine, Horizon, TimeZoneId};
use engine_core::sync::ObjectKind;
use engine_provider::ContactsProvider;
use engine_store::{PendingOpState, StoreRead as _};
use fake::{RoundPim, account, seed_calendar_create, seed_contact_create};

use crate::{
    events::{CollectingSink, EngineEvent},
    grid::CalendarGridRead,
    pim::{PimRoundReport, run_pim_round},
};

/// March 2026, the standing horizon of the round tests.
fn horizon() -> Horizon {
    spanning("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z")
}

fn spanning(start: &str, end: &str) -> Horizon {
    Horizon::new(
        start.parse().expect("valid instant"),
        end.parse().expect("valid instant"),
    )
    .expect("valid horizon")
}

/// Runs one PIM round with the tests' standing zone.
async fn round<P: ContactsProvider>(
    engine: &Engine,
    provider: &P,
    sink: &CollectingSink,
    horizon: Horizon,
) -> Result<PimRoundReport, ApiError> {
    let zone = TimeZoneId::iana("Europe/Amsterdam").expect("valid zone");
    run_pim_round(engine, provider, &account(), horizon, &zone, sink).await
}

/// The round's change events, as one-line expectations.
fn calendar_changed() -> EngineEvent {
    EngineEvent::CalendarChanged {
        account: "acct-1".to_owned(),
    }
}

fn contacts_changed() -> EngineEvent {
    EngineEvent::ContactsChanged {
        account: "acct-1".to_owned(),
    }
}

#[tokio::test]
async fn a_first_round_emits_one_change_event_per_scope_and_reports_both() {
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();

    let report = round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the round completes");

    // One change event per scope, in the round's order; nothing drained, so no
    // outbox event.
    assert_eq!(
        sink.events(),
        vec![calendar_changed(), contacts_changed()],
        "both scopes changed on the first snapshot"
    );
    assert_eq!(report.calendar.calendars.upserted, 1);
    assert_eq!(report.calendar.events.applied.upserted, 2);
    assert_eq!(report.contacts.address_books.applied.upserted, 1);
    assert_eq!(report.contacts.cards.applied.upserted, 2);
    assert_eq!(report.drained_cal, 0);
    assert_eq!(report.drained_contacts, 0);
}

#[tokio::test]
async fn a_quiet_round_emits_nothing() {
    // A delta that changes nothing is not news, and nothing runnable means no
    // outbox events either; the round still reports both scopes' zero counts.
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();
    round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the first round completes");
    sink.clear();

    let report = round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the second round completes");

    assert!(sink.events().is_empty(), "nothing changed, nothing drained");
    assert_eq!(report.calendar.events.applied.upserted, 0);
    assert_eq!(report.contacts.cards.applied.upserted, 0);
    assert_eq!(report.drained_cal, 0);
    assert_eq!(report.drained_contacts, 0);
}

#[tokio::test]
async fn a_drained_calendar_op_reports_the_outbox_after_the_calendar_drain() {
    let engine = Engine::open_in_memory().expect("engine");
    seed_calendar_create(&engine, "drain-9@test.local").await;
    let sink = CollectingSink::default();

    let report = round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the round completes");

    // The round's order, pinned: the calendar change, then the drain's outbox
    // depth, then the contacts change; nothing runnable was left for the
    // contact drain, so no second outbox event.
    assert_eq!(
        sink.events(),
        vec![
            calendar_changed(),
            EngineEvent::OutboxChanged {
                account: "acct-1".to_owned(),
                pending: 0,
            },
            contacts_changed(),
        ]
    );
    assert_eq!(report.drained_cal, 1, "the unstarted create was replayed");
    assert_eq!(report.drained_contacts, 0);
}

#[tokio::test]
async fn a_contact_op_the_calendar_drain_skipped_stays_lease_held() {
    // Claims are scope-blind and the round's order is fixed: the calendar
    // drain claims the account's runnable ops first, executes its own verbs,
    // and leaves a claimed contact op skipped-unmarked and lease-held — so
    // the contact drain in the SAME round finds nothing runnable.
    let engine = Engine::open_in_memory().expect("engine");
    let first = seed_contact_create(&engine, "card-9").await;
    let sink = CollectingSink::default();

    let report = round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the round completes");

    assert_eq!(
        sink.events(),
        vec![calendar_changed(), contacts_changed()],
        "the calendar drain settled nothing, so it reported no depth"
    );
    assert_eq!(report.drained_cal, 0);
    assert_eq!(
        report.drained_contacts, 0,
        "the op is lease-held, not runnable this round"
    );
    assert_eq!(
        engine.pending_op_state(first).await.expect("op state read"),
        Some(PendingOpState::InFlight),
        "the skip left the op lease-held, unmarked"
    );

    // The second round skips again — and this is the permanent starvation
    // under this round's fixed calendar-first order, not a one-TTL wait: the
    // fresh contact op is claimed and skipped by the calendar drain exactly
    // as the first was, and the first is still lease-held. When its lease
    // does expire, the next round's calendar drain re-claims it (claims look
    // at runnability only, limit 16) and skips it again — no round under
    // this ordering ever hands a runnable contact op to the contact drain.
    // This pin is what the intent-aware claims escalated as engine task T7b
    // will flip; until then, a host must run `drain_contact_ops` on its own
    // cadence between rounds to clear contact writes.
    sink.clear();
    let second = seed_contact_create(&engine, "card-10").await;
    let report = round(&engine, &RoundPim::full(), &sink, horizon())
        .await
        .expect("the second round completes");

    assert!(sink.events().is_empty(), "the second round is quiet");
    assert_eq!(
        report.drained_cal, 0,
        "a skip is not a settled op — the calendar drain reports nothing"
    );
    assert_eq!(
        report.drained_contacts, 0,
        "the second round's contact drain found nothing runnable either"
    );
    assert_eq!(
        engine
            .pending_op_state(second)
            .await
            .expect("op state read"),
        Some(PendingOpState::InFlight),
        "the calendar drain claimed and skipped the fresh op too"
    );
    assert_eq!(
        engine.pending_op_state(first).await.expect("op state read"),
        Some(PendingOpState::InFlight),
        "the first op is still lease-held, never run"
    );
}

#[tokio::test]
async fn a_failed_calendar_sync_fails_the_round_before_contacts_runs() {
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();

    let err = round(&engine, &RoundPim::failing(), &sink, horizon())
        .await
        .expect_err("the failed calendar sync fails the round");

    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
    assert!(
        sink.events().is_empty(),
        "nothing was emitted before the failure"
    );
    // Contacts never ran: no contact scope exists for the account.
    let scopes = engine
        .host_store()
        .account_scopes(account())
        .await
        .expect("scopes read");
    assert!(
        scopes
            .iter()
            .all(|scope| scope.object_kind() != Some(ObjectKind::ContactCard)),
        "the contacts pass did not run"
    );
}

#[tokio::test]
async fn a_round_whose_horizon_widened_materializes_and_reports_the_calendar() {
    // The trap `expand_horizon` exists to close: the first round seeds the
    // store's window to its narrow January horizon; the second round's quiet
    // delta derives nothing — so the round itself must advance the window, and
    // the materialization it writes is a calendar change the host hears.
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();
    let january = spanning("2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z");
    let series = fake::standup("evt-w", "uid-w@h", fake::at_utc(2026, 1, 5, 10), None);
    let provider = RoundPim::with_events(vec![series]);

    round(&engine, &provider, &sink, january)
        .await
        .expect("the first round completes");
    let january_rows = engine
        .occurrences_in(&account(), january)
        .await
        .expect("occurrences read")
        .len();
    assert_eq!(january_rows, 4, "January materialized four Mondays");
    sink.clear();

    let year = spanning("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z");
    let report = round(&engine, &provider, &sink, year)
        .await
        .expect("the second round completes");

    assert_eq!(report.calendar.events.applied.upserted, 0);
    assert_eq!(
        sink.events(),
        vec![calendar_changed()],
        "the materialization is the round's one calendar change"
    );
    let year_rows = engine
        .occurrences_in(&account(), year)
        .await
        .expect("occurrences read")
        .len();
    assert!(
        year_rows > 45,
        "the year is materialized: {year_rows} occurrences"
    );
}

#[tokio::test]
async fn a_round_in_a_changed_zone_re_expands_and_reports_the_calendar() {
    // The second reason `expand_horizon` exists: the host's zone changed. A
    // floating event's stored instant is only correct for the zone it was
    // expanded under, and the persisted window's horizon already covers the
    // request — so a zone-blind window check would leave every floating
    // occurrence resolved through the old zone, silently shifted by the zone
    // offset, exactly at the instants a grid renders them.
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();
    let amsterdam = TimeZoneId::iana("Europe/Amsterdam").expect("valid zone");
    let new_york = TimeZoneId::iana("America/New_York").expect("valid zone");
    let provider = RoundPim::with_events(vec![fake::meeting(
        "evt-z",
        "uid-z@h",
        fake::floating(2026, 3, 2, 10),
        "Floating coffee",
        "PT30M",
    )]);

    run_pim_round(&engine, &provider, &account(), horizon(), &amsterdam, &sink)
        .await
        .expect("the first round completes");
    let amsterdam_page = engine
        .calendar_grid(&account(), horizon())
        .await
        .expect("grid read");
    assert_eq!(amsterdam_page.occurrences.len(), 1);
    assert_eq!(
        amsterdam_page.occurrences[0].start.hour(),
        9,
        "10:00 Amsterdam is 09:00Z"
    );
    assert!(amsterdam_page.is_materialized);
    sink.clear();

    // Same horizon, different zone, quiet delta: only the re-expansion can
    // move the row — and it is a calendar change the host hears.
    let report = run_pim_round(&engine, &provider, &account(), horizon(), &new_york, &sink)
        .await
        .expect("the second round completes");

    assert_eq!(report.calendar.events.applied.upserted, 0);
    assert_eq!(
        sink.events(),
        vec![calendar_changed()],
        "the re-materialization is the round's one calendar change"
    );
    let new_york_page = engine
        .calendar_grid(&account(), horizon())
        .await
        .expect("grid read");
    assert_eq!(new_york_page.occurrences.len(), 1);
    assert_eq!(
        new_york_page.occurrences[0].start.hour(),
        15,
        "10:00 New York is 15:00Z"
    );
    assert!(
        new_york_page.is_materialized,
        "the window still covers the request after the re-expansion"
    );

    // The zone unchanged: the window now matches `(horizon, zone)` exactly,
    // so a third round re-expands nothing and emits nothing.
    sink.clear();
    run_pim_round(&engine, &provider, &account(), horizon(), &new_york, &sink)
        .await
        .expect("the third round completes");
    assert!(sink.events().is_empty(), "no zone drift, no widening");
}
