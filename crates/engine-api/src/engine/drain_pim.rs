//! The PIM outbox drains on `Engine`: the calendar and contact counterparts of
//! [`drain_outbox`](Self::drain_outbox) — the fork's port of the owed
//! follow-up the engine's `FORKING.md` names (upstream's drainer is
//! mail-only; these replay the Contact*/Calendar* kinds through the same
//! targeted-claim queue, `engine-sync`'s `drain_pim`). A facade write resolves
//! the op it enqueues in the same call; these resolve the ops nobody finished
//! — an unstarted `Pending` op, or a crash orphan an expired lease left
//! `InFlight` — which is the recovery a host's PIM round runs so a faulted
//! write never strands.

use engine_core::ids::AccountId;
use engine_provider::{ContactsProvider, Provider};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Drains this account's queued **contact** ops from the durable outbox —
    /// the `create_contact`, `patch_contact`, and `delete_contact` intents
    /// that were recorded but never resolved (an unstarted op, or a crash
    /// orphan an expired lease left `InFlight`). One call replays every
    /// contact op the pass can claim, under the same targeted-claim contract
    /// and the same counting semantics as
    /// [`drain_outbox`](Self::drain_outbox): the returned count is the ops
    /// driven to a recorded outcome — succeeded, settled-failed, or parked on
    /// a retryable class for a later pass (the store owns the attempt bound).
    ///
    /// A replayed patch or delete re-reads its base card by id from the
    /// store, exactly as the contact execute half prescribes: a card already
    /// gone is a `Conflict` for a patch (corrected by the next contact sync,
    /// never retried into success) and a success for a delete.
    ///
    /// **Host scheduling.** The natural rhythm is one call per PIM round,
    /// after the contacts sync — the shape `run_pim_round` (engine-host) and
    /// the shell's multi-collection round both drive.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] when the claim, a mark, or a replay's store
    /// step fails. An execution failure is not an error: it arrives as the
    /// recorded outcome this call counts.
    pub async fn drain_contact_ops<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<usize, ApiError> {
        engine_sync::drain_contact_ops(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Drains this account's queued **calendar** ops from the durable outbox —
    /// the `create_calendar_event`, `patch_calendar_event`,
    /// `put_calendar_document`, `rsvp_calendar_event` (direct or
    /// from-invite), and `delete_calendar_event` intents that were recorded
    /// but never resolved — under the same claim/replay/settle discipline and
    /// counting semantics as [`drain_contact_ops`](Self::drain_contact_ops)
    /// (see its docs). Every calendar verb lives on [`Provider`] itself, so
    /// this drain needs no tighter provider bound than the mail one.
    ///
    /// A replayed patch, RSVP, or occurrence delete re-reads its base event by
    /// id from the store: a patch or RSVP whose event is gone is a `Conflict`
    /// (corrected by the next calendar sync, never retried into success), an
    /// occurrence delete whose event is gone is a success, and a series delete
    /// and a document replace need no base at all.
    ///
    /// # Errors
    ///
    /// As [`drain_contact_ops`](Self::drain_contact_ops).
    pub async fn drain_calendar_ops<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<usize, ApiError> {
        engine_sync::drain_calendar_ops(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }
}

#[cfg(test)]
mod tests {
    //! The drains through the facade, as a host calls them. Every facade write
    //! resolves its op inline, so these live in-crate, enqueue one unstarted
    //! op exactly as the inline drivers' enqueue half leaves it, and then
    //! drive and observe it through the public methods: the drain call for the
    //! count, `pending_op_state` for the outcome.

    use engine_core::{
        contact::{ContactCard, ContactDraft},
        ids::{AddressBookId, CalendarId, ContactId, EventId, Uid},
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime},
        write::{IdempotencyKey, PendingOp, PendingOpId, ResourceKey},
    };
    use engine_provider::{
        CalendarWrites, Capabilities, ConnectionInfo, ContactWriteReceipt, ContactsProvider,
        EventDraft, EventWriteReceipt, Provider, ProviderResult,
    };
    use engine_store::{PendingOpState, Store};

    use crate::{AccountId, Engine};

    fn account() -> AccountId {
        AccountId::try_from("acct-1").expect("valid account")
    }

    /// The unstarted op both tests drain — recorded durably, claimed by
    /// nobody: the state a crash between the enqueue and claim halves of any
    /// inline driver leaves behind.
    async fn unstarted_op(
        engine: &Engine,
        idempotency: &str,
        resource: &str,
        intent: engine_sync::OutboxIntent,
    ) -> PendingOpId {
        engine
            .store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new(idempotency).expect("valid idempotency key"),
                    intent.pending_op_kind(),
                    ResourceKey::new(resource).expect("valid resource key"),
                    serde_json::to_value(intent).expect("serializable intent"),
                ),
            )
            .await
            .expect("the op enqueues")
    }

    fn contact_draft() -> ContactDraft {
        let book = AddressBookId::try_from("personal").expect("valid book");
        ContactDraft {
            address_book: book.clone(),
            card: ContactCard::new(
                ContactId::try_from("card-1").expect("valid id"),
                Memberships::of_one(book),
            ),
        }
    }

    /// The contacts fake: a provider that can create a card.
    struct FakeContacts;

    impl CalendarWrites for FakeContacts {}

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
            Ok(ContactWriteReceipt::new(draft.card.id.clone()))
        }
    }

    /// The calendar fake: a provider that can create an event, minting the id
    /// the CalDAV shape does — from the caller's `UID`.
    struct FakeCalendar;

    #[async_trait::async_trait]
    impl Provider for FakeCalendar {
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(Capabilities::none().with_calendars())
        }
    }

    #[async_trait::async_trait]
    impl CalendarWrites for FakeCalendar {
        async fn create_event(
            &self,
            _account: &AccountId,
            draft: &EventDraft,
        ) -> ProviderResult<EventWriteReceipt> {
            Ok(EventWriteReceipt::new(
                EventId::try_from(format!("/cal/{}.ics", draft.uid.as_str()).as_str())
                    .expect("valid id"),
                draft.uid.clone(),
                engine_core::version::RevisionTokens::from_etag(engine_core::version::ETag::new(
                    "\"put-v1\"",
                )),
            ))
        }
    }

    #[tokio::test]
    async fn a_pending_contact_op_drains_to_succeeded_through_the_facade() {
        let engine = Engine::open_in_memory().expect("engine");
        let op = unstarted_op(
            &engine,
            "drain:contact:create",
            "contact-create:personal",
            engine_sync::OutboxIntent::CreateContact {
                draft: contact_draft(),
            },
        )
        .await;

        let drained = engine
            .drain_contact_ops(&FakeContacts, &account())
            .await
            .unwrap();

        assert_eq!(drained, 1);
        assert_eq!(
            engine.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Succeeded)
        );
        let again = engine
            .drain_contact_ops(&FakeContacts, &account())
            .await
            .unwrap();
        assert_eq!(again, 0, "a settled op leaves nothing runnable");
    }

    #[tokio::test]
    async fn a_pending_calendar_op_drains_to_succeeded_through_the_facade() {
        let engine = Engine::open_in_memory().expect("engine");
        let op = unstarted_op(
            &engine,
            "drain:calendar:create",
            "event:drain-9@test.local",
            engine_sync::OutboxIntent::CreateEvent {
                draft: calendar_draft("drain-9@test.local"),
            },
        )
        .await;

        let drained = engine
            .drain_calendar_ops(&FakeCalendar, &account())
            .await
            .unwrap();

        assert_eq!(drained, 1);
        assert_eq!(
            engine.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Succeeded)
        );
    }

    #[tokio::test]
    async fn a_poison_payload_settles_failed_through_the_facade() {
        // A queued row whose payload carries no tagged intent is settled
        // Failed by the drain — the same terminal poison the engine-sync
        // suite pins, driven here through the verb a host calls.
        let engine = Engine::open_in_memory().expect("engine");
        let op = engine
            .store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new("drain:calendar:poison").unwrap(),
                    engine_core::write::PendingOpKind::CalendarPatch,
                    ResourceKey::new("event:poison@test.local").unwrap(),
                    serde_json::json!({"verb": "not-a-real-verb"}),
                ),
            )
            .await
            .unwrap();

        let drained = engine
            .drain_calendar_ops(&FakeCalendar, &account())
            .await
            .unwrap();

        assert_eq!(drained, 1);
        assert_eq!(
            engine.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Failed)
        );
    }

    fn calendar_draft(uid: &str) -> EventDraft {
        EventDraft::new(
            CalendarId::try_from("/cal/default/").expect("valid calendar"),
            Uid::new(uid).expect("valid uid"),
            "Sprint planning",
            at(9),
            at(10),
            "2026-08-01T10:00:00Z".parse().expect("valid stamp"),
        )
    }

    fn at(hour: u8) -> CalendarDateTime {
        CalendarDateTime::utc(
            format!("2026-08-01T{hour:02}:00:00")
                .parse::<LocalDateTime>()
                .unwrap(),
        )
    }
}
