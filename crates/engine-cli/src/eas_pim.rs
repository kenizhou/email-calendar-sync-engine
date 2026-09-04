//! The PIM arms of `eas-sync`: `--kind calendar` and `--kind contacts`,
//! the P2 acceptance paths beside the mail arm in `eas_sync.rs`.
//!
//! The same posture as the mail arm: the command builds real
//! [`EasAdapter`]s and hands them to the ENGINE's own sync —
//! [`sync_calendar`](engine_sync::sync_calendar) and
//! [`sync_contacts`](engine_sync::sync_contacts), with their window
//! seeding, occurrence expansion, and people rebuild — against a real
//! [`SqliteStore`], so a verdict printed here is the verdict a host
//! driving `run_pim_round` would get. Discovery, negotiation, and the
//! rounds loop are the only CLI-owned parts.
//!
//! ## The fan-out shape
//!
//! EAS binds an event scope to one calendar collection and a card scope to
//! one contact folder, so the arms fan out over per-collection adapters
//! (`EasAdapter::calendar_adapter` / `contacts_adapter`) and drive one
//! engine sync per collection per round. The container half
//! (`sync_calendars` / `sync_address_books`) then runs once per collection
//! pass rather than once per round: each adapter's container pass rides
//! the store's shared container-scope cursor (a cold in-process hierarchy
//! ledger falls back to it), so the first pass snapshots and the rest read
//! as empty deltas — correct, self-healing, and bounded by the collection
//! count (the interleaved-adapter cost `adapter/hierarchy.rs` documents).
//! Round 2 against the same `--db` is the incremental pass by
//! construction, exactly as the mail arm's.
//!
//! ## The `--create` round-trip
//!
//! `--kind calendar --create` drives one write through the engine's
//! outbox (`engine_sync::create_calendar_event` — the facade verb's own
//! composition) after the sync rounds, then re-syncs once: the re-sync's
//! apply carries the SERVER's copy of the created event under the
//! ServerId the Sync Add ack returned, which is the backfill a host
//! re-reads after a write. The probe's uid and idempotency key are
//! deterministic in the account, so a second run against the same store
//! resolves as a duplicate rather than creating a second event.

use core::time::Duration;
use std::fmt::Write as _;

use engine_core::{
    ids::{AccountId, AddressBookId, CalendarId, MailboxId, Uid},
    sync::SyncUpdate,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{ContactSourceSync, ContactsProvider, EventDraft, Provider as _, ScopeSync};
use engine_recurrence::Horizon;
use engine_store::{StoreRead as _, SyncApplied};
use engine_sync::{SyncError, create_calendar_event, sync_calendar, sync_contacts};
use provider_eas::{EasAdapter, client::EasClient};
use store_sqlite::SqliteStore;

use crate::{
    CliError, WORKER,
    eas_sync::{EasTarget, LEASE_TTL, configured_client, negotiate},
};

/// The bound folder every discovery adapter uses as its inert mail binding
/// (the container verbs never read it — the mail arm's placeholder).
const DISCOVERY_PLACEHOLDER: &str = "0";

/// How far inside the horizon the `--create` probe event starts, and how
/// long it runs.
const PROBE_OFFSET: Duration = Duration::from_hours(1);
const PROBE_LENGTH: Duration = Duration::from_hours(1);

/// Runs the account's calendar sync `rounds` times against `store`.
///
/// `calendars` selects a subset of the discovered calendar folders by
/// ServerId (empty = all). `roundtrip` adds the `--create` probe after the
/// rounds. The report ends with the occurrence summary: what materialized,
/// over which persisted window, in which zone — the facts a grid read
/// would see.
// One argument past the lint's taste, and they do not fold: (target,
// rounds) belong to the command, (calendars, horizon, zone) to the sync's
// scope, and `roundtrip` is the probe switch — the mail arm carries its
// own parallel fan-out parameters the same way.
#[allow(
    clippy::too_many_arguments,
    reason = "the command's target plus the sync's scope"
)]
pub(crate) async fn eas_sync_calendar(
    store: &SqliteStore<crate::ManualClock>,
    account: &AccountId,
    target: &EasTarget,
    calendars: &[CalendarId],
    rounds: usize,
    horizon: Horizon,
    zone: &TimeZoneId,
    roundtrip: bool,
) -> Result<String, CliError> {
    let mut client = configured_client(target)?;
    let version = negotiate(&mut client).await?;

    // Discovery through the adapter's own verb: one bootstrap FolderSync,
    // unapplied — the applied container pass rides each round's sync.
    let ScopeSync { update, .. } = discovery_adapter(&client)
        .sync_calendars(account, None)
        .await?;
    let discovered: Vec<CalendarId> = match update {
        SyncUpdate::Snapshot { objects, .. } => objects.into_iter().map(|c| c.id).collect(),
        SyncUpdate::Delta { changed, .. } => changed.into_iter().map(|c| c.id).collect(),
    };
    let selected = selected(
        &discovered,
        calendars,
        |c| c.as_str().to_owned(),
        "--calendar",
    )?;

    let mut out = format!(
        "eas-sync {} (calendar): {} calendar(s), protocol {version}\n",
        account.as_str(),
        selected.len()
    );
    // The adapters live across the rounds (and the round-trip) so each
    // collection's SyncKey ledger stays warm in-process.
    let adapters: Vec<EasAdapter> = selected
        .iter()
        .map(|cal| EasAdapter::calendar_adapter(client.clone(), cal.clone()))
        .collect();
    for round in 1..=rounds {
        let _ = writeln!(out, "round {round}/{rounds}");
        let mut containers = SyncApplied::default();
        let mut reports = Vec::new();
        for adapter in &adapters {
            let report = sync_calendar(adapter, store, account, worker(), LEASE_TTL, horizon, zone)
                .await
                .map_err(pim_error)?;
            containers = merge(&containers, &report.calendars);
            reports.push(report);
        }
        let _ = writeln!(
            out,
            "  containers  +{} -{}",
            containers.upserted, containers.tombstoned
        );
        for (cal, report) in selected.iter().zip(&reports) {
            let _ = writeln!(
                out,
                "  {:<10} +{} -{}",
                cal.as_str(),
                report.events.applied.upserted,
                report.events.applied.tombstoned
            );
        }
    }

    if roundtrip {
        let cal = &selected[0];
        let adapter = &adapters[0];
        let draft = probe_draft(account, cal, &horizon)?;
        let outcome = create_calendar_event(
            adapter,
            store,
            account,
            worker(),
            LEASE_TTL,
            &format!("engine-cli-create-{}", account.as_str()),
            &draft,
        )
        .await
        .map_err(pim_error)?;
        let _ = writeln!(
            out,
            "created {} (uid {}) as op {}",
            outcome.event.as_str(),
            outcome.uid.as_str(),
            outcome.op.get()
        );
        // The backfill read: the delta now carries the server's copy under
        // the acked ServerId — the row a host re-reads after the write.
        let backfill = sync_calendar(adapter, store, account, worker(), LEASE_TTL, horizon, zone)
            .await
            .map_err(pim_error)?;
        let _ = writeln!(
            out,
            "  {:<10} +{} -{}",
            cal.as_str(),
            backfill.events.applied.upserted,
            backfill.events.applied.tombstoned
        );
    }

    occurrence_summary(&mut out, store, account, &selected, &adapters).await?;
    Ok(out)
}

/// Runs the account's contacts sync `rounds` times against `store`.
///
/// `books` selects a subset of the discovered address books by ServerId
/// (empty = all). The report ends with the people count the rebuild
/// derived from the cards.
pub(crate) async fn eas_sync_contacts(
    store: &SqliteStore<crate::ManualClock>,
    account: &AccountId,
    target: &EasTarget,
    books: &[AddressBookId],
    rounds: usize,
) -> Result<String, CliError> {
    let mut client = configured_client(target)?;
    let version = negotiate(&mut client).await?;

    // Discovery through the adapter's own verb: one bootstrap FolderSync,
    // unapplied.
    let found = discovery_adapter(&client)
        .sync_address_books(account, None)
        .await?;
    let ContactSourceSync::Available { sync, .. } = found else {
        return Err(CliError::Eas(
            "the server reports the account's contacts source unavailable".to_owned(),
        ));
    };
    let ScopeSync { update, .. } = sync;
    let discovered: Vec<AddressBookId> = match update {
        SyncUpdate::Snapshot { objects, .. } => objects.into_iter().map(|b| b.id).collect(),
        SyncUpdate::Delta { changed, .. } => changed.into_iter().map(|b| b.id).collect(),
    };
    let selected = selected(&discovered, books, |b| b.as_str().to_owned(), "--book")?;

    let mut out = format!(
        "eas-sync {} (contacts): {} address book(s), protocol {version}\n",
        account.as_str(),
        selected.len()
    );
    let adapters: Vec<EasAdapter> = selected
        .iter()
        .map(|book| EasAdapter::contacts_adapter(client.clone(), book.clone()))
        .collect();
    let mut people = 0;
    for round in 1..=rounds {
        let _ = writeln!(out, "round {round}/{rounds}");
        let mut containers = SyncApplied::default();
        let mut reports = Vec::new();
        for adapter in &adapters {
            let report = sync_contacts(adapter, store, account, worker(), LEASE_TTL)
                .await
                .map_err(pim_error)?;
            containers = merge(&containers, &report.address_books.applied);
            people = report.people.people;
            reports.push(report);
        }
        let _ = writeln!(
            out,
            "  books       +{} -{}",
            containers.upserted, containers.tombstoned
        );
        for (book, report) in selected.iter().zip(&reports) {
            let _ = writeln!(
                out,
                "  {:<13} +{} -{}",
                book.as_str(),
                report.cards.applied.upserted,
                report.cards.applied.tombstoned
            );
        }
    }
    let _ = writeln!(out, "people {people}");
    Ok(out)
}

/// The discovery adapter: an unbound binding whose container verbs are
/// per-account (its mail folder is inert).
fn discovery_adapter(client: &EasClient) -> EasAdapter {
    EasAdapter::new(
        client.clone(),
        MailboxId::try_from(DISCOVERY_PLACEHOLDER).expect("a non-empty discovery placeholder"),
    )
}

/// The collections to sync: all discovered, or the requested subset — a
/// requested id the server never named is an error, not a silent skip
/// (the mail arm's `selected_folders` rule, restated per id type; `show`
/// renders an id for the error, `flag` names the flag that carried it).
fn selected<T: Clone + PartialEq>(
    discovered: &[T],
    requested: &[T],
    show: impl Fn(&T) -> String,
    flag: &str,
) -> Result<Vec<T>, CliError> {
    if requested.is_empty() {
        return Ok(discovered.to_vec());
    }
    for id in requested {
        if !discovered.contains(id) {
            return Err(CliError::Usage(format!(
                "{flag} {} is not one of the server's",
                show(id)
            )));
        }
    }
    Ok(requested.to_vec())
}

/// Sums two apply-count sets (the per-collection container lines aggregate
/// into one round-level line).
fn merge(a: &SyncApplied, b: &SyncApplied) -> SyncApplied {
    SyncApplied {
        upserted: a.upserted + b.upserted,
        tombstoned: a.tombstoned + b.tombstoned,
        reconciled: a.reconciled + b.reconciled,
    }
}

/// Renders the occurrence summary: for every synced event scope, how many
/// occurrence rows materialized over the window the store persisted, and
/// the window itself — the facts a grid read would answer with.
async fn occurrence_summary(
    out: &mut String,
    store: &SqliteStore<crate::ManualClock>,
    account: &AccountId,
    selected: &[CalendarId],
    adapters: &[EasAdapter],
) -> Result<(), CliError> {
    for (cal, adapter) in selected.iter().zip(adapters) {
        let scope = adapter.event_scope(account);
        let Some(window) = store.expansion_window(&scope).await? else {
            continue;
        };
        let occurrences = store.scope_occurrences(&scope, window.horizon).await?;
        let _ = writeln!(
            out,
            "occurrences {} in {} over {}..{} ({})",
            occurrences.len(),
            cal.as_str(),
            window.horizon.start(),
            window.horizon.end(),
            window.zone
        );
    }
    Ok(())
}

/// The `--create` probe event: a one-hour UTC meeting one hour inside the
/// horizon's start, deterministic in the account so a repeated run
/// against the same store resolves as a duplicate create instead of
/// stacking events.
fn probe_draft(
    account: &AccountId,
    calendar: &CalendarId,
    horizon: &Horizon,
) -> Result<EventDraft, CliError> {
    let placement = |span: Duration| {
        horizon.start().checked_add(span).ok_or_else(|| {
            CliError::Usage("--create cannot place an event inside this horizon".to_owned())
        })
    };
    let start = placement(PROBE_OFFSET)?;
    let end = start.checked_add(PROBE_LENGTH).ok_or_else(|| {
        CliError::Usage("--create cannot place an event inside this horizon".to_owned())
    })?;
    Ok(EventDraft::new(
        calendar.clone(),
        Uid::new(format!("engine-cli-{}", account.as_str())).expect("an account id keys a uid"),
        "engine-cli probe",
        CalendarDateTime::utc(wall_clock(start)),
        CalendarDateTime::utc(wall_clock(end)),
        start,
    ))
}

/// The wall-clock view of a UTC instant (the UTC wall clock, by identity).
fn wall_clock(instant: UtcDateTime) -> LocalDateTime {
    LocalDateTime::new(
        instant.year(),
        instant.month(),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second(),
    )
    .expect("a valid wall clock decomposes back into one")
}

/// The worker identity the PIM arms stamp on leases (the mail arm's).
fn worker() -> engine_store::WorkerId {
    engine_store::WorkerId::new(WORKER)
}

/// Maps the engine's sync error onto the CLI's taxonomy: the provider and
/// store legs keep their dedicated variants; the rest render as the EAS
/// report error the mail arm uses.
fn pim_error(err: SyncError) -> CliError {
    match err {
        SyncError::Provider(e) => CliError::Provider(e),
        SyncError::Store(e) => CliError::Store(e),
        other => CliError::Eas(other.to_string()),
    }
}
