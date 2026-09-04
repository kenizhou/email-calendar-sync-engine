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
use engine_store::StoreRead as _;
use fake::{RoundPim, account, seed_calendar_create, seed_contact_create};

use crate::{
    events::{CollectingSink, EngineEvent},
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
async fn a_contact_op_the_calendar_drain_skipped_waits_for_a_later_round() {
    // Claims are scope-blind: the calendar drain claims the account's runnable
    // ops, executes its own verbs, and leaves a claimed contact op lease-held
    // and unmarked for one TTL (the facade's documented skip cost) — so the
    // contact drain in the SAME round finds nothing runnable.
    let engine = Engine::open_in_memory().expect("engine");
    seed_contact_create(&engine, "card-9").await;
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
