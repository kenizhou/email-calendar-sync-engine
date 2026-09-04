//! The PIM round tests' fake provider and fixtures: the JMAP-shaped in-memory
//! `RoundPim` (calendars, events, address books, cards — snapshot on the first
//! sync of each scope, an empty delta once a cursor exists), the event
//! fixtures built over one work calendar, and the unstarted-outbox-op seeds
//! the drain scenarios reconstruct a crash with.

use core::num::NonZeroU32;

use engine_api::{AccountId, Engine};
use engine_core::{
    calendar::{Calendar, Event, Frequency, Recurrence, RecurrenceBound, RecurrenceRule},
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactEmail, ContactProperty, ContactSourceClass,
        PropertyId,
    },
    ids::{AddressBookId, CalendarId, ContactId, EventId, Uid},
    membership::Memberships,
    sync::{SyncObject, SyncState, SyncUpdate},
    time::{CalendarDateTime, LocalDateTime},
    version::{ETag, RevisionTokens},
    write::{IdempotencyKey, PendingOp, PendingOpId, ResourceKey},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactSourceSync, ContactWriteReceipt, ContactsProvider,
    EventDraft, EventWriteReceipt, Provider, ProviderError, ProviderResult, ScopeSync,
};
use engine_store::Store as _;
use engine_sync::OutboxIntent;

pub(super) fn account() -> AccountId {
    AccountId::try_from("acct-1").expect("valid account")
}

fn calendar() -> Calendar {
    Calendar::new(CalendarId::try_from("work").expect("valid id"), "Work")
}

pub(super) fn at_utc(year: i32, month: u8, day: u8, hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(LocalDateTime::new(year, month, day, hour, 0, 0).expect("valid time"))
}

/// A floating (zoneless) wall-clock time — the form whose materialized
/// instants move with the zone the expansion resolved them through.
pub(super) fn floating(year: i32, month: u8, day: u8, hour: u8) -> CalendarDateTime {
    CalendarDateTime::Floating(
        LocalDateTime::new(year, month, day, hour, 0, 0).expect("valid time"),
    )
}

/// A titled, timed meeting in the work calendar.
pub(super) fn meeting(
    id: &str,
    uid: &str,
    start: CalendarDateTime,
    title: &str,
    duration: &str,
) -> Event {
    let mut event = Event::new(
        EventId::try_from(id).expect("valid id"),
        Uid::new(uid).expect("valid uid"),
        Memberships::of_one(CalendarId::try_from("work").expect("valid id")),
        start,
    );
    event.title = title.to_owned();
    event.duration = duration.parse().expect("valid duration");
    event
}

/// A weekly standup from `start`, bounded to `count` occurrences or unbounded
/// to the horizon when `None`.
pub(super) fn standup(id: &str, uid: &str, start: CalendarDateTime, count: Option<u32>) -> Event {
    let mut event = meeting(id, uid, start, "Standup", "PT30M");
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    if let Some(count) = count {
        rule.bound = RecurrenceBound::Count(NonZeroU32::new(count).expect("non-zero"));
    }
    event.recurrence = Some(Recurrence::from_rule(rule));
    event
}

fn book() -> AddressBook {
    let mut book = AddressBook::new(
        AddressBookId::try_from("personal").expect("valid id"),
        "Personal",
        ContactSourceClass::Personal,
    );
    book.is_writable = true;
    book
}

fn card(id: &str) -> ContactCard {
    let mut card = ContactCard::new(
        ContactId::try_from(id).expect("valid id"),
        Memberships::of_one(AddressBookId::try_from("personal").expect("valid id")),
    );
    card.source_class = ContactSourceClass::Personal;
    // The rebuild's display-name fallback: a card with neither a name nor an
    // address cannot persist a person.
    card.emails.insert(
        PropertyId::new("email").expect("valid property id"),
        ContactProperty::new(ContactEmail::new(format!("{id}@test.local"))),
    );
    card
}

/// A minimal in-memory PIM provider — JMAP-shaped (one account-wide calendar
/// scope, one address-book/card pair) — that snapshots its data on the first
/// sync of each scope and answers every later one with an empty delta, so a
/// second round is quiet by construction. `fail_calendars` fails the
/// calendar-container fetch, the failure path's stand-in.
pub(super) struct RoundPim {
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    books: Vec<AddressBook>,
    cards: Vec<ContactCard>,
    fail_calendars: bool,
}

impl RoundPim {
    /// The standing fixture: one calendar with a single meeting and a bounded
    /// standup series, one address book with two cards.
    pub(super) fn full() -> Self {
        Self::with_events(vec![
            meeting(
                "evt-1",
                "uid-1@h",
                at_utc(2026, 3, 2, 9),
                "Sprint planning",
                "PT1H",
            ),
            standup("evt-2", "uid-2@h", at_utc(2026, 3, 2, 10), Some(3)),
        ])
    }

    /// One calendar holding exactly `events`, the same address book and cards.
    pub(super) fn with_events(events: Vec<Event>) -> Self {
        Self {
            calendars: vec![calendar()],
            events,
            books: vec![book()],
            cards: vec![card("one"), card("two")],
            fail_calendars: false,
        }
    }

    pub(super) fn failing() -> Self {
        Self {
            fail_calendars: true,
            ..Self::with_events(Vec::new())
        }
    }
}

/// The fake's one shape: a snapshot of `objects` on the first sync of a scope,
/// an empty delta once a cursor exists.
fn snapshot_or_delta<T: SyncObject + Clone>(
    cursor: Option<&SyncState>,
    objects: &[T],
    first: &str,
    later: &str,
) -> ScopeSync<T> {
    if cursor.is_some() {
        return ScopeSync::new(
            SyncUpdate::delta(Vec::new(), Vec::new()),
            SyncState::new(later),
        );
    }
    let present = objects
        .iter()
        .map(|object| object.provider_key().clone())
        .collect();
    ScopeSync::new(
        SyncUpdate::snapshot(objects.to_vec(), present),
        SyncState::new(first),
    )
}

#[async_trait::async_trait]
impl Provider for RoundPim {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_calendars().with_contacts())
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if self.fail_calendars {
            return Err(ProviderError::retryable("calendar list unreachable"));
        }
        Ok(snapshot_or_delta(cursor, &self.calendars, "cal-1", "cal-2"))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Ok(snapshot_or_delta(
            cursor,
            &self.events,
            "events-1",
            "events-2",
        ))
    }

    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        // Mints the id the way a CalDAV adapter does — from the caller's UID.
        let id = format!("/cal/{}.ics", draft.uid.as_str());
        Ok(EventWriteReceipt::new(
            EventId::try_from(id.as_str()).expect("valid id"),
            draft.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }
}

#[async_trait::async_trait]
impl ContactsProvider for RoundPim {
    async fn sync_address_books(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        Ok(ContactSourceSync::Available {
            sync: snapshot_or_delta(cursor, &self.books, "books-1", "books-2"),
            cursor_recovered: false,
        })
    }

    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        Ok(ContactSourceSync::Available {
            sync: snapshot_or_delta(cursor, &self.cards, "cards-1", "cards-2"),
            cursor_recovered: false,
        })
    }

    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        Ok(ContactWriteReceipt::new(draft.card.id.clone()))
    }
}

/// Seeds one unstarted op — the state a crash between the enqueue and claim
/// halves of any inline write driver leaves behind, built exactly the way the
/// facade's own drain tests build it — and returns its id for state polling.
async fn seed_unstarted(
    engine: &Engine,
    idempotency: String,
    resource: String,
    intent: OutboxIntent,
) -> PendingOpId {
    engine
        .host_store()
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(idempotency).expect("valid key"),
                ResourceKey::new(resource).expect("valid key"),
                serde_json::to_value(intent).expect("serializable intent"),
            ),
        )
        .await
        .expect("the op enqueues")
}

/// One unstarted calendar create, the calendar drain's replay.
pub(super) async fn seed_calendar_create(engine: &Engine, uid: &str) {
    let draft = EventDraft::new(
        CalendarId::try_from("/cal/default/").expect("valid calendar"),
        Uid::new(uid).expect("valid uid"),
        "Drain me",
        at_utc(2026, 3, 5, 9),
        at_utc(2026, 3, 5, 10),
        "2026-03-05T10:00:00Z".parse().expect("valid stamp"),
    );
    seed_unstarted(
        engine,
        format!("pim:event:create:{uid}"),
        format!("event:{uid}"),
        OutboxIntent::CreateEvent { draft },
    )
    .await;
}

/// One unstarted contact create, the contact drain's replay — its id returned
/// so a test can pin the op's lifecycle state across rounds.
pub(super) async fn seed_contact_create(engine: &Engine, id: &str) -> PendingOpId {
    let draft = ContactDraft {
        address_book: AddressBookId::try_from("personal").expect("valid book"),
        card: card(id),
    };
    seed_unstarted(
        engine,
        format!("pim:contact:create:{id}"),
        format!("contact-create:personal:{id}"),
        OutboxIntent::CreateContact { draft },
    )
    .await
}
