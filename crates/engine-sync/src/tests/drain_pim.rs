//! The PIM drain loops' own tests: contact and calendar ops replayed through
//! the upstream queue's targeted claims. The founding cases are the unstarted
//! op and the crash orphan (an expired `InFlight` the targeted claim reclaims);
//! the pins are the base re-read semantics — a patch replays against the
//! freshly stored base and settles `Conflict` when the event is gone, an
//! occurrence delete of a gone event completes — plus poison, the kind filter
//! (a mail op is never leased), and the store-owned park on a retryable
//! failure. The calendar cases live in the `calendar` submodule.

mod calendar;

use core::time::Duration;

use engine_core::{
    calendar::Calendar,
    contact::{ContactCard, ContactDraft},
    ids::{AddressBookId, ContactId, MessageIdHeader},
    mail::EmailAddress,
    membership::Memberships,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, ResourceKey, SubmitPayload},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactWriteReceipt, ContactsProvider, Draft, ProviderResult,
};
use engine_store::{LeaseRequest, ManualClock, PendingOpState, Store};

use super::*;
use crate::outbox::{OutboxIntent, drain_contact_ops};

/// The lease the tests arm — long enough to span a claim, short enough that a
/// two-minute advance expires it.
pub(super) fn ttl() -> Duration {
    Duration::from_mins(1)
}

/// Enqueues one unstarted (`Pending`) op — exactly the state the inline
/// drivers' enqueue half leaves behind.
pub(super) async fn enqueue_op(
    store: &SqliteStore<ManualClock>,
    kind: PendingOpKind,
    idempotency: &str,
    resource: &str,
    payload: serde_json::Value,
) -> PendingOpId {
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(idempotency).unwrap(),
                kind,
                ResourceKey::new(resource).unwrap(),
                payload,
            ),
        )
        .await
        .unwrap()
}

/// The drainer's founding case: an op an inline worker claimed and then died
/// holding, its lease long expired — the targeted claim reclaims it.
pub(super) async fn crash_orphan(
    store: &SqliteStore<ManualClock>,
    clock: &ManualClock,
    kind: PendingOpKind,
    idempotency: &str,
    resource: &str,
    payload: serde_json::Value,
) -> PendingOpId {
    let op = enqueue_op(store, kind, idempotency, resource, payload).await;
    let _ = store
        .claim_pending_op(account(), op, LeaseRequest::new(worker(), ttl()))
        .await
        .unwrap();
    clock.advance(Duration::from_mins(2));
    op
}

fn contact_draft() -> ContactDraft {
    let book = AddressBookId::try_from("personal").unwrap();
    ContactDraft {
        address_book: book.clone(),
        card: ContactCard::new(
            ContactId::try_from("card-1").unwrap(),
            Memberships::of_one(book),
        ),
    }
}

/// The contacts fake: a provider whose create succeeds, or refuses every
/// create as rate-limited while `throttle` is armed — the retryable class the
/// store's park rule answers. Every other verb keeps the trait's rejecting
/// default (which would surface as a recorded `Failed`, never a silent pass).
pub(super) struct FakeContacts {
    throttle: std::sync::atomic::AtomicBool,
}

impl FakeContacts {
    pub(super) fn new() -> Self {
        Self {
            throttle: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Arms the rate-limit refusal.
    pub(super) fn throttled(self) -> Self {
        self.throttle
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Lifts the rate limit: the provider comes back.
    pub(super) fn recovered(&self) {
        self.throttle
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl engine_provider::CalendarWrites for FakeContacts {}

#[async_trait::async_trait]
impl Provider for FakeContacts {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_contacts())
    }
}

#[async_trait::async_trait]
impl ContactsProvider for FakeContacts {
    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        if self.throttle.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(engine_provider::ProviderError::rate_limited(
                "slow down",
                None,
            ));
        }
        Ok(ContactWriteReceipt::new(draft.card.id.clone()))
    }
}

async fn drain_contacts(
    provider: &FakeContacts,
    store: &SqliteStore<ManualClock>,
) -> Result<usize, crate::SyncError> {
    drain_contact_ops(provider, store, &account(), worker(), ttl()).await
}

/// Seeds the store with the one stored event the base-dependent replays
/// target: a full calendar sync of the fake's snapshot, so the event's object
/// payload — the base a replay re-reads — is exactly what sync recorded.
pub(super) async fn seed_stored_event(store: &SqliteStore<ManualClock>) -> FakeMail {
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![Calendar::new(
            CalendarId::try_from("/cal/default/").unwrap(),
            "Default",
        )],
        vec![super::calendar_write::stored(
            "/cal/default/evt-1.ics",
            "evt-1@test.local",
        )],
    );
    sync_calendar(
        &provider,
        store,
        &account(),
        worker(),
        ttl(),
        Horizon::new(
            "2026-01-01T00:00:00Z".parse().unwrap(),
            "2026-12-31T00:00:00Z".parse().unwrap(),
        )
        .unwrap(),
        &TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    )
    .await
    .unwrap();
    provider
}

#[tokio::test]
async fn an_unstarted_contact_create_drains_to_succeeded() {
    let provider = FakeContacts::new();
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        PendingOpKind::ContactCreate,
        "drain:contact:create",
        "contact-create:personal",
        serde_json::to_value(OutboxIntent::CreateContact {
            draft: contact_draft(),
        })
        .unwrap(),
    )
    .await;

    let drained = drain_contacts(&provider, &store).await.unwrap();

    assert_eq!(drained, 1, "the unstarted op was driven to an outcome");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    let again = drain_contacts(&provider, &store).await.unwrap();
    assert_eq!(again, 0, "a settled op leaves nothing runnable");
}

#[tokio::test]
async fn a_mail_op_is_never_leased_by_the_pim_drains() {
    // The kind filter is the "can dispatch" test: a mail op stays untouched —
    // the mail drainer's, at its own cadence.
    let provider = FakeMail::new(vec![], vec![]);
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        PendingOpKind::MailSubmit,
        "drain:pim:foreign",
        "draft:send-1@test.local",
        serde_json::to_value(OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(Draft::new(
                MessageIdHeader::new("send-1@test.local").unwrap(),
                EmailAddress::new("alice@test.local"),
                vec![EmailAddress::new("bob@test.local")],
                "Not mine",
                "the body",
            )),
        })
        .unwrap(),
    )
    .await;

    let cal = calendar::drain_calendar(&provider, &store).await.unwrap();
    let contacts = drain_contacts(&FakeContacts::new(), &store).await.unwrap();

    assert_eq!(cal, 0);
    assert_eq!(contacts, 0);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Pending),
        "a foreign op is left exactly as it was"
    );
}

#[tokio::test]
async fn a_rate_limited_create_parks_and_a_later_pass_re_drives_it() {
    // The store owns park-vs-settle: a rate-limited create parks with its
    // attempt counted and a backoff set; the drain before the backoff elapses
    // leaves it alone, and a later one — provider recovered — re-drives it.
    let provider = FakeContacts::new().throttled();
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let op = enqueue_op(
        &store,
        PendingOpKind::ContactCreate,
        "drain:contact:throttled",
        "contact-create:personal",
        serde_json::to_value(OutboxIntent::CreateContact {
            draft: contact_draft(),
        })
        .unwrap(),
    )
    .await;

    let first = drain_contacts(&provider, &store).await.unwrap();
    assert_eq!(first, 1, "parking is an outcome — counted");
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Pending),
        "a retryable class parks, it does not settle"
    );

    // Still inside the backoff window: the claim refuses, the pass defers.
    let immediate = drain_contacts(&provider, &store).await.unwrap();
    assert_eq!(immediate, 0, "an op still backing off is not claimable");

    // The provider recovers and the backoff elapses: the next pass succeeds.
    provider.recovered();
    clock.advance(Duration::from_mins(2));
    let recovered = drain_contacts(&provider, &store).await.unwrap();
    assert_eq!(recovered, 1);
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}
