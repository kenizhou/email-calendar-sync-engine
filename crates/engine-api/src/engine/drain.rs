//! The outbox drains on `Engine`: the background counterpart of the write
//! methods in `writes` and `contacts`. A facade write resolves the op it
//! enqueues in the same call; a drain resolves the ops nobody finished — an
//! unstarted `Pending` op, or a crash orphan (`InFlight` under an expired
//! lease) — which is the recovery a host runs periodically (kylins P1 runs it
//! on a timer) so a crash never strands a recorded write.

use engine_core::ids::AccountId;
use engine_provider::{ContactsProvider, Provider};
use engine_sync::{drain_contact_ops, drain_mail_ops};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

/// How many runnable ops one facade drain claims and replays per call.
///
/// A facade counterpart of `engine-sync`'s claim bound, deliberately **not** a
/// share of it: the inline drivers' `CLAIM_LIMIT` bounds the window they scan to
/// *find their own just-enqueued op*, while this bounds a background batch, and
/// coupling them would let tuning one silently move the other. The same value
/// (16) is still chosen so a drain and a concurrent inline write contend for
/// claim windows symmetrically, and one pass stays bounded — at most 16 provider
/// round trips — while an ordinary backlog clears in one or two calls. A deeper
/// backlog needs another call, not a bigger constant; a host drains to zero
/// rather than raising this.
const DRAIN_LIMIT: usize = 16;

impl Engine {
    /// Drains this account's runnable **mail** ops from the durable outbox: the
    /// `submit_mail`/`submit_mail_source`, `edit_mail`, and `report_message`
    /// intents that were recorded but never resolved — an unstarted op, or one a
    /// crashed worker left `InFlight` under an expired lease. One call claims up
    /// to 16 runnable ops (the `DRAIN_LIMIT` bound) and replays each through the same
    /// execute half the inline path runs, so a replay carries exactly the inline
    /// semantics: a caller-rendered submission re-sends its bytes **verbatim**
    /// (never re-rendered — the host-crypto seam), and an ambiguous post-`DATA`
    /// send parks as `NeedsConfirmation` rather than being blind-retried.
    ///
    /// Returns how many ops this call drove to a recorded outcome — `Succeeded`,
    /// `Failed` (a provider failure, or the terminal `Failed` a poison payload
    /// that does not decode as a tagged intent is marked with — neither is ever
    /// re-claimed), or a parked `NeedsConfirmation` (also never re-driven: a
    /// parked op is not claimable). **Failed is terminal** and confirmation is a
    /// host decision, so the drain's work is done when an op holds any of the
    /// three. Not counted: an op skipped as out of scope — a contact or calendar
    /// verb this drain claimed only because claims are scope-blind, left unmarked
    /// so the right executor can take it after the lease expires (one lease TTL
    /// of unrunnability per skip) — and an op whose mark lost its
    /// lease to another worker (that worker owns the outcome).
    ///
    /// **Calendar verbs are excluded** — replaying a calendar write needs the
    /// base re-fetch and conflict recovery of a later phase — and a replayed
    /// submission's `SentCopy` fact (what became of the sender's own copy) is
    /// lost: Phase 1 records the op state only, and the host observes completion
    /// through [`pending_op_state`](Self::pending_op_state).
    ///
    /// **Host scheduling.** One call is one bounded batch, not a loop: call this
    /// periodically (a timer) and again while it returns non-zero to clear a
    /// backlog. Schedule it against [`drain_contact_ops`](Self::drain_contact_ops)
    /// so each drain gets a clean claim window rather than repeatedly burning the
    /// other's ops into lease-holds — the natural rhythm is both, once per sync
    /// pass.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] when the claim, a mark, or a replay's store
    /// step fails. An execution failure is not an error: it arrives as the
    /// `Failed` outcome this call records.
    pub async fn drain_mail_ops<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<usize, ApiError> {
        drain_mail_ops(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            DRAIN_LIMIT,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Drains this account's runnable **contact** ops from the durable outbox —
    /// the `create_contact`, `patch_contact`, and `delete_contact` intents that
    /// were recorded but never resolved — under the same claim/replay/settle
    /// discipline and the same counting semantics as
    /// [`drain_mail_ops`](Self::drain_mail_ops) (see its docs for the exact
    /// accounting, the exclusions, and the skip's TTL cost). A replayed patch or
    /// delete re-reads its base card by id from the store, exactly as the contact
    /// execute half prescribes: a card already gone is a `Conflict` for a patch
    /// (corrected by the next contact sync, never retried into success) and a
    /// success for a delete.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] when the claim, a mark, or a replay's store
    /// step (including the base-card re-read) fails. An execution failure is not
    /// an error: it arrives as the `Failed` outcome this call records.
    pub async fn drain_contact_ops<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<usize, ApiError> {
        drain_contact_ops(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            DRAIN_LIMIT,
        )
        .await
        .map_err(map_sync_error)
    }
}

#[cfg(test)]
mod tests {
    //! The drains through the facade, as a host calls them. The suites in
    //! `tests/` cannot build the drain's input state — every facade write
    //! resolves its op inline, and the store is `pub(crate)` — so these live
    //! in-crate, enqueue one unstarted op exactly as the inline drivers' enqueue
    //! half leaves it, and then drive and observe it through the public methods:
    //! the drain call for the count, `pending_op_state` for the outcome.

    use engine_core::{
        contact::{ContactCard, ContactDraft},
        ids::{AddressBookId, ContactId, MessageIdHeader, ProviderKey},
        mail::EmailAddress,
        membership::Memberships,
        write::{IdempotencyKey, PendingOp, PendingOpId, ResourceKey, SubmitPayload},
    };
    use engine_provider::{
        Capabilities, ConnectionInfo, ContactWriteReceipt, ContactsProvider, Draft, Provider,
        ProviderResult, SubmissionReceipt,
    };
    use engine_store::{PendingOpState, Store};
    use engine_sync::OutboxIntent;

    use crate::{AccountId, Engine};

    fn account() -> AccountId {
        AccountId::try_from("acct-1").expect("valid account")
    }

    /// The unstarted op both tests drain — recorded durably, claimed by nobody:
    /// the state a crash between the enqueue and claim halves of any inline
    /// driver leaves behind, built here directly because no facade write does.
    async fn unstarted_op(
        engine: &Engine,
        idempotency: &str,
        resource: &str,
        intent: OutboxIntent,
    ) -> PendingOpId {
        engine
            .store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new(idempotency).expect("valid idempotency key"),
                    ResourceKey::new(resource).expect("valid resource key"),
                    serde_json::to_value(intent).expect("serializable intent"),
                ),
            )
            .await
            .expect("the op enqueues")
    }

    fn draft(message_id: &str) -> Draft {
        Draft::new(
            MessageIdHeader::new(message_id).expect("valid id"),
            EmailAddress::new("alice@test.local"),
            vec![EmailAddress::new("bob@test.local")],
            "Drain me",
            "the body",
        )
    }

    fn contact_draft() -> ContactDraft {
        let personal = AddressBookId::try_from("personal").expect("valid book");
        ContactDraft {
            address_book: personal.clone(),
            card: ContactCard::new(
                ContactId::try_from("card-1").expect("valid id"),
                Memberships::of_one(personal),
            ),
        }
    }

    /// The submitting fake, pared from the integration suite's
    /// `SubmittingProvider` to the one verb the mail test replays; every other
    /// verb keeps the trait's rejecting default, which would surface as a
    /// recorded `Failed` rather than a silent pass.
    struct FakeMail;

    #[async_trait::async_trait]
    impl Provider for FakeMail {
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(Capabilities::none().with_mail())
        }

        async fn submit_email(
            &self,
            _account: &AccountId,
            draft: &Draft,
        ) -> ProviderResult<SubmissionReceipt> {
            Ok(SubmissionReceipt::filed(
                ProviderKey::new("sent-1").expect("valid key"),
                draft.message_id.clone(),
            ))
        }
    }

    /// The contacts counterpart: a provider that can create a card.
    struct FakeContacts;

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

    #[tokio::test]
    async fn a_pending_mail_op_drains_to_succeeded_through_the_facade() {
        // The founding case through the host's entry point: an unstarted submit
        // is replayed by one facade drain call, counted, and pollable as
        // Succeeded through the facade's own read. A second call finds nothing —
        // one call is one batch, and a settled op is not runnable again.
        let engine = Engine::open_in_memory().expect("engine");
        let op = unstarted_op(
            &engine,
            "drain:submit:1",
            "draft:drain-1@test.local",
            OutboxIntent::SubmitMail {
                payload: SubmitPayload::Draft(draft("drain-1@test.local")),
            },
        )
        .await;

        let drained = engine.drain_mail_ops(&FakeMail, &account()).await.unwrap();

        assert_eq!(drained, 1, "the unstarted op was driven to an outcome");
        assert_eq!(
            engine.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Succeeded)
        );
        let again = engine.drain_mail_ops(&FakeMail, &account()).await.unwrap();
        assert_eq!(again, 0, "a settled op leaves nothing runnable");
    }

    #[tokio::test]
    async fn a_pending_contact_op_drains_to_succeeded_through_the_facade() {
        // The contacts half through the same shape: the facade's contact drain
        // replays the create through the provider and commits the outcome the
        // host polls.
        let engine = Engine::open_in_memory().expect("engine");
        let op = unstarted_op(
            &engine,
            "drain:contact:create",
            "contact-create:personal",
            OutboxIntent::CreateContact {
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
    }
}
