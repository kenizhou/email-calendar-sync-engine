//! The multi-collection round (`run_pim_round_many`) driven end to end over
//! the same in-memory `Engine`: collection-bound fakes — per-calendar event
//! scopes, per-book card scopes, one shared account-wide container scope each
//! — the shape CalDAV (and Google/Graph after it) presents when a host binds
//! one adapter per collection. Split from the single-round tests at the
//! 500-line cap; the fixtures and helpers are the parent module's.

use engine_api::{ApiError, Engine, Horizon, TimeZoneId};
use engine_provider::{ContactsProvider, Provider};

use super::{
    account, calendar_changed, contacts_changed,
    fake::{RoundPim, at_utc, card, meeting, seed_calendar_create, seed_contact_create, standup},
    horizon,
};
use crate::{
    events::{CollectingSink, EngineEvent},
    pim::{PimSetRoundReport, run_pim_round_many},
};

/// Runs one multi-collection PIM round with the tests' standing zone.
async fn many_round<P: Provider, K: ContactsProvider>(
    engine: &Engine,
    calendars: &[P],
    contacts: &[K],
    sink: &CollectingSink,
    horizon: Horizon,
) -> PimSetRoundReport {
    let zone = TimeZoneId::iana("Europe/Amsterdam").expect("valid zone");
    run_pim_round_many(
        engine,
        calendars,
        contacts,
        &account(),
        horizon,
        &zone,
        sink,
    )
    .await
}

/// The outbox-depth event at `pending`, spelled once for the pins below.
fn outbox(pending: i64) -> EngineEvent {
    EngineEvent::OutboxChanged {
        account: "acct-1".to_owned(),
        pending,
    }
}

#[tokio::test]
async fn many_round_syncs_every_calendar_and_book() {
    let engine = Engine::open_in_memory().expect("engine");
    // One runnable op per scope, so the drains have something to count.
    seed_calendar_create(&engine, "drain-many@test.local").await;
    seed_contact_create(&engine, "card-many").await;
    let sink = CollectingSink::default();

    let first = RoundPim::collection(
        "cal-a",
        vec![meeting(
            "evt-a",
            "uid-a@h",
            at_utc(2026, 3, 2, 9),
            "Sprint planning",
            "PT1H",
        )],
        Vec::new(),
    );
    let second = RoundPim::collection(
        "cal-b",
        vec![standup("evt-b", "uid-b@h", at_utc(2026, 3, 3, 10), Some(3))],
        Vec::new(),
    );
    let book_a = RoundPim::collection("book-a", Vec::new(), vec![card("one")]);
    let book_b = RoundPim::collection("book-b", Vec::new(), vec![card("two")]);

    let report = many_round(
        &engine,
        &[first, second],
        &[book_a, book_b],
        &sink,
        horizon(),
    )
    .await;

    // Every collection's pass ran, in slice order: one report per calendar
    // and per book. The shared container list lands once — the first
    // calendar's container sync snapshots it, the second answers the same
    // list with an empty delta — exactly what bound adapters present.
    assert_eq!(report.calendars.len(), 2);
    assert_eq!(report.contacts.len(), 2);
    assert_eq!(report.calendars[0].calendars.upserted, 1);
    assert_eq!(report.calendars[1].calendars.upserted, 0);
    assert_eq!(report.calendars[0].events.applied.upserted, 1);
    assert_eq!(report.calendars[1].events.applied.upserted, 1);
    assert_eq!(report.contacts[0].cards.applied.upserted, 1);
    assert_eq!(report.contacts[1].cards.applied.upserted, 1);
    assert_eq!(
        report.drained_cal, 1,
        "the first calendar's drain drove the create"
    );
    assert_eq!(
        report.drained_contacts, 1,
        "the first book's drain drove the create"
    );
    assert!(report.failures.is_empty());

    // The emission order, pinned: each calendar that moved with the outbox
    // depth after a drain that settled something; the discovery's own
    // `ContactsChanged` (the book list is contact rows too); then each book
    // that moved, with its drain's depth.
    assert_eq!(
        sink.events(),
        vec![
            calendar_changed(),
            outbox(1),
            calendar_changed(),
            contacts_changed(),
            contacts_changed(),
            outbox(0),
            contacts_changed(),
        ]
    );
}

#[tokio::test]
async fn many_round_isolates_a_failing_calendar() {
    let engine = Engine::open_in_memory().expect("engine");
    seed_calendar_create(&engine, "drain-x@test.local").await;
    seed_contact_create(&engine, "card-x").await;
    let sink = CollectingSink::default();

    let healthy = RoundPim::collection(
        "cal-a",
        vec![meeting(
            "evt-x",
            "uid-x@h",
            at_utc(2026, 3, 2, 9),
            "Sprint planning",
            "PT1H",
        )],
        Vec::new(),
    );
    let failing = RoundPim::failing_collection("cal-b");
    let book_a = RoundPim::collection("book-a", Vec::new(), vec![card("one")]);
    let book_b = RoundPim::collection("book-b", Vec::new(), vec![card("two")]);

    let report = many_round(
        &engine,
        &[healthy, failing],
        &[book_a, book_b],
        &sink,
        horizon(),
    )
    .await;

    // The failed calendar lands in `failures` under its slice index, its
    // widen/emit/drain never ran (no drain for a provider whose sync did not
    // land), and every later collection — the books included — ran in full.
    assert_eq!(report.calendars.len(), 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].0, 1, "the second calendar's slice index");
    assert!(
        matches!(report.failures[0].1, ApiError::Sync(_)),
        "got {:?}",
        report.failures[0].1
    );
    assert_eq!(report.contacts.len(), 2, "the books still synced");
    assert_eq!(
        report.drained_cal, 1,
        "the healthy calendar's drain drove the create; the failing one never drained"
    );
    assert_eq!(report.drained_contacts, 1);

    // Exactly one calendar's worth of news before the contacts pass — the
    // failing calendar contributed no event and no outbox depth.
    assert_eq!(
        sink.events(),
        vec![
            calendar_changed(),
            outbox(1),
            contacts_changed(),
            contacts_changed(),
            outbox(0),
            contacts_changed(),
        ]
    );
}

#[tokio::test]
async fn many_round_runs_discovery_once_over_the_first_book() {
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();
    let books = [
        RoundPim::collection("book-a", Vec::new(), vec![card("one")]),
        RoundPim::collection("book-b", Vec::new(), vec![card("two")]),
    ];
    let no_calendars: [RoundPim; 0] = [];

    let report = many_round(&engine, &no_calendars, &books, &sink, horizon()).await;

    // Discovery is one account-wide scope every bound adapter answers
    // identically, so the round asks it of exactly one provider — the first
    // book's — and every book still syncs its own cards through its own scope.
    assert_eq!(books[0].discovery_calls(), 1);
    assert_eq!(books[1].discovery_calls(), 0);
    assert_eq!(report.contacts.len(), 2);
    assert!(report.failures.is_empty());
}

#[tokio::test]
async fn many_round_with_empty_slices_is_a_no_op_report() {
    let engine = Engine::open_in_memory().expect("engine");
    let sink = CollectingSink::default();
    let no_calendars: [RoundPim; 0] = [];
    let no_books: [RoundPim; 0] = [];

    let report = many_round(&engine, &no_calendars, &no_books, &sink, horizon()).await;

    // Nothing was asked of any provider, so nothing is news: all-zero report,
    // no events.
    assert!(report.calendars.is_empty());
    assert!(report.contacts.is_empty());
    assert_eq!(report.drained_cal, 0);
    assert_eq!(report.drained_contacts, 0);
    assert!(report.failures.is_empty());
    assert!(sink.events().is_empty());
}
