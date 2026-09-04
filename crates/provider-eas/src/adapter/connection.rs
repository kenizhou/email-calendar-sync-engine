// SPDX-License-Identifier: MPL-2.0
//! The trait half of the adapter: what [`EasAdapter`] reports, which scopes
//! it names, and the verbs that have landed (FolderSync containers for mail
//! and calendars, Sync class Email messages and class Calendar events,
//! ItemOperations message-source fetch). The un-overridden defaults remain
//! the honest behavior for every verb still to come (the module docs in
//! `super` carry the ladder).

use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncWindow},
};
use engine_provider::{ConnectionInfo, EmailStream, Provider, ProviderResult, ScopeSync};

use super::EasAdapter;

#[async_trait::async_trait]
impl Provider for EasAdapter {
    /// The verb-ladder capabilities plus the transport's negotiated HTTP
    /// version — composed per call, the Graph/JMAP precedent, because the
    /// version fact is live (most-recent observation) rather than latched.
    ///
    /// * **`mail` is on in this slice** — `sync_mailboxes` (containers) and `stream_email`
    ///   (messages) are both live, which is the whole mail read domain the bit names. The
    ///   write/submission bits stay off until their verbs land (the module docs' ladder).
    /// * **`http_version`** is `None` until the [`EasAdapter::negotiate`] OPTIONS exchange (EAS's
    ///   session-discovery step — the JMAP/CalDAV connect-time precedent; Graph, which has no
    ///   discovery step, stays `None` until its first fetch), then whatever the transport most
    ///   recently observed. Read from the funnel handle — the sync lock-free read side (see the
    ///   module docs' verb-lock section).
    /// * **`tls_version`** is always `None`: reqwest exposes only the peer certificate, never the
    ///   negotiated protocol version (`docs/agent-guidance/tls.md`).
    /// * **`concurrent_fetches`** stays the `ConnectionInfo` default (1) until a measured
    ///   per-server EAS ceiling exists to justify a wider one — the Graph precedent set its 4 from
    ///   live throttling evidence.
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.http.get(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    /// The FolderSync hierarchy is one per-account container scope — a real
    /// rotated cursor, unlike IMAP/Graph's re-discovered snapshot lists —
    /// claimed and applied before the per-folder email scopes it parents.
    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::EasFolderList {
            account: account.clone(),
        }
    }

    /// EAS item `Sync` is per collection, so email sync is per folder —
    /// [`SyncScope::EasFolder`] keyed by the bound folder's ServerId, the
    /// IMAP `ImapMailbox` / Graph `GraphFolder` binding precedent.
    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::EasFolder {
            account: account.clone(),
            folder: self.folder.clone(),
        }
    }

    /// FolderSync ([MS-ASFolderSync]): the hierarchy SyncKey is the cursor —
    /// `None` bootstraps from `"0"` (a snapshot of the full hierarchy), a
    /// `Some(key)` round returns the wire's Add/Update/Delete delta, and a
    /// status-9 invalidation recovers as a re-bootstrapped snapshot inside
    /// this one call. Non-mail classes (calendar/contacts/tasks/notes) are
    /// filtered out — `Mailbox` is the mail container type and those folders
    /// belong to the calendar/contacts scopes. `super::mailboxes` owns the
    /// mapping and its contract.
    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        super::mailboxes::sync(&self.client, &self.hierarchy, cursor).await
    }

    /// Sync class "Email" over the bound folder ([MS-ASSYNC]): the
    /// collection SyncKey is the cursor (`None`/empty → bootstrap `"0"`),
    /// `MoreAvailable` pages the pass, and each completed round's chunk
    /// carries that round's rotated key as `advance_to` — Additive with a
    /// checkpoint per round. A SyncKey invalidation (collection status
    /// 3/12) recovers inside the stream as a Reconcile pass re-bootstrapped
    /// from `"0"`; everything else surfaces through the Sync family
    /// classifier. `fetch_batch` is the wire `WindowSize` (`0` = the
    /// drain-loop cap); `chunk_size` splits a round's items for incremental
    /// commit. `super::email` owns the mapping and its contract, including
    /// the depth-window note (no wire filter; the bound holds at apply).
    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        super::email::stream(
            &self.client,
            &self.folder,
            &self.collection_key,
            cursor,
            fetch_batch,
            chunk_size,
        )
    }

    /// ItemOperations Fetch with `MIMESupport`=2 + BodyPreference Type 4
    /// ([MS-ASCMD] §4.10.2.1) — the whole RFC 5322 bytes of one message.
    /// Addressing is the T4 identity mapping: the bound folder IS the
    /// `CollectionId`, the `MessageId` IS the `ServerId`. A truncated answer
    /// (Truncated flag / Total shortfall) is reassembled from authoritative
    /// server ranges; a vanished item answers a per-item status classified
    /// `Conflict` (re-sync, then retry). `super::source` owns the mapping
    /// and its contract.
    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        super::source::fetch_source(&self.client, &self.folder, message).await
    }

    /// SendMail ([MS-ASCMD] §2.2.1.13): the draft assembled through
    /// `engine-rfc5322` (the filed variant — SendMail routes recipients
    /// from the bytes, so the `Bcc` header must stay in them), sent as an
    /// OPAQUE `<Mime>` with `<SaveInSentItems/>` and a
    /// `Message-ID`-derived deterministic `<ClientId>` (Exchange's dedup
    /// key for a lost-response retry). The empty-body success carries no
    /// server id, so the receipt's key is the `sent:<Message-ID>`
    /// placeholder (the Graph/IMAP no-id precedent) that reconciles by
    /// `Message-ID` when Sent Items next syncs. `super::submit` owns the
    /// mapping and its contract.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &engine_provider::Draft,
    ) -> ProviderResult<engine_provider::SubmissionReceipt> {
        super::submit::submit(&self.client, draft).await
    }

    /// SendMail over the caller's own rendered bytes — sent **verbatim**,
    /// never re-rendered. The seam's shape contract is validated first (a
    /// `Message-ID`, a `From`, a terminated body), and a non-empty
    /// `recipients` envelope is honored only when it names exactly the
    /// bytes' own To/Cc/Bcc addr-specs — SendMail has no separate envelope,
    /// so a list the bytes cannot deliver is refused permanently rather
    /// than silently mis-delivered. `super::submit` owns the contract.
    async fn submit_email_source(
        &self,
        _account: &AccountId,
        source: &[u8],
        recipients: &[String],
    ) -> ProviderResult<engine_provider::SubmissionReceipt> {
        super::submit::submit_source(&self.client, source, recipients).await
    }

    /// The three mutations over their EAS commands: keyword edits ride a
    /// Sync Commands `Change` keyed by the adapter's collection-key ledger
    /// (the trait's write seam carries no cursor — see the module docs'
    /// ledger section; a cold ledger refuses `NeedsResync` and the outbox
    /// retries after the next pass re-seeds it), moves ride `MoveItems`
    /// with the bound folder as the source collection and record the SOURCE
    /// key (the moved copy is a new ServerId that reconciles next sync),
    /// and `Delete` is refused `InvalidState` — EAS has no per-item hard
    /// delete; the documented policy is `MoveTo` the deleted-items folder.
    /// `super::mutate` owns the mapping and its contract.
    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &engine_provider::MailEdit,
    ) -> ProviderResult<engine_provider::MailEditReceipt> {
        super::mutate::edit(&self.client, &self.folder, &self.collection_key, edit).await
    }

    /// The same FolderSync hierarchy as the mail container scope, split into
    /// its own per-account container scope — the calendar folders (class
    /// `Calendar` / folder Type 8) are claimed and applied before the
    /// per-calendar event scopes they parent.
    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::EasCalendarList {
            account: account.clone(),
        }
    }

    /// EAS item `Sync` is per collection, so event sync is per calendar
    /// folder — [`SyncScope::EasCalendar`] keyed by the bound calendar's
    /// ServerId, the Graph `GraphCalendar` / CalDAV `DavCollection` binding
    /// precedent. Without a binding ([`EasAdapter::with_calendar`]) the
    /// default JMAP shape stands — never consulted, since an unbound
    /// adapter's capabilities do not advertise the calendar family.
    fn event_scope(&self, account: &AccountId) -> SyncScope {
        match &self.calendar {
            Some(calendar) => SyncScope::EasCalendar {
                account: account.clone(),
                calendar: calendar.clone(),
            },
            None => SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::CalendarEvent,
            },
        }
    }

    /// FolderSync filtered to the Calendar class ([MS-ASFD] folder Type 8):
    /// the hierarchy SyncKey is the cursor (`None` bootstraps from `"0"` as
    /// a snapshot, `Some(key)` returns the wire's Add/Update/Delete delta),
    /// and a status-9 invalidation recovers inside the call as a
    /// re-bootstrapped snapshot — the `sync_mailboxes` recovery shape.
    /// `super::calendar` owns the mapping and its contract.
    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        super::calendar::sync_calendars(&self.client, &self.hierarchy, cursor).await
    }

    /// Sync class "Calendar" over the bound calendar folder ([MS-ASSYNC]):
    /// the collection SyncKey is the cursor (`None`/empty → bootstrap `"0"`
    /// → snapshot), `MoreAvailable` pages the pass inside the call, and a
    /// SyncKey invalidation (collection status 3/12) recovers inside the
    /// call by re-bootstrapping once as a snapshot — the mail stream's
    /// recovery adapted to the whole-scope verb. Items convert through the
    /// read-side seam (`calendar::calendar_event_from_props`); a malformed
    /// item is skipped, never failing the pass. Requires the calendar
    /// binding ([`EasAdapter::with_calendar`]) — an unbound adapter refuses
    /// `InvalidState`, and its capabilities never advertise the family.
    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        match &self.calendar {
            Some(calendar) => {
                super::calendar::sync_events(&self.client, calendar, &self.calendar_key, cursor)
                    .await
            }
            None => Err(super::calendar::unbound_calendar()),
        }
    }

    /// Sync `Add` with a synthesized `ClientId` — the only id-reveal point:
    /// the receipt keys the `ServerId` the server's `Responses` ack assigns
    /// ([MS-ASCMD] §2.2.3.7.2; an ack-less success keys the ClientId
    /// placeholder, reconciled away by `uid` on the next events pass). The
    /// draft converts through `calendar::convert_write::write_from_draft`
    /// (fixed-offset TZI fold; a named-DST zone refuses). The Add rides the
    /// adapter's calendar collection-key ledger. Requires the calendar
    /// binding. `super::calendar_write` owns the mapping.
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &engine_provider::EventDraft,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        match &self.calendar {
            Some(calendar) => {
                super::calendar_write::create(&self.client, calendar, &self.calendar_key, draft)
                    .await
            }
            None => Err(super::calendar::unbound_calendar()),
        }
    }

    /// Sync `Change` (Replace) of the master: a `Series` target rebuilds the
    /// complete document from the base + patch; an `Instance` target
    /// rebuilds the master carrying that occurrence as a modified exception
    /// (the master's other overrides ride untouched — the
    /// `OverrideSurvival::kept()` construction). An empty patch is a no-op
    /// receipt. Requires the calendar binding. `super::calendar_write`.
    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &engine_provider::EventEdit,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        match &self.calendar {
            Some(calendar) => {
                super::calendar_write::patch(&self.client, calendar, &self.calendar_key, base, edit)
                    .await
            }
            None => Err(super::calendar::unbound_calendar()),
        }
    }

    /// The documented rejecting default: EAS's update verb is a field-level
    /// Sync `Change`, not a document PUT, and there is no iCalendar document
    /// on an EAS server — [`Provider::patch_event`](Provider::patch_event)
    /// is the supported path. The trait explicitly allows an adapter
    /// advertising `calendar_writes` to leave this at the refusal.
    async fn put_event(
        &self,
        _account: &AccountId,
        write: &engine_provider::EventWrite,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        let _ = write;
        Err(super::calendar_write::put_refusal())
    }

    /// Sync `Delete` of the ServerId for the series; an occurrence delete
    /// is a `Change` of the master carrying the deleted-marker exception
    /// (the EAS EXDATE form, [MS-ASCAL] §2.2.2.16) — which is why an
    /// occurrence delete needs `base`. Already-gone is success (a per-item
    /// 8, or no item status at all — [MS-ASCMD] §2.2.3.154). Requires the
    /// calendar binding. `super::calendar_write`.
    async fn delete_event(
        &self,
        _account: &AccountId,
        base: Option<&Event>,
        deletion: &engine_provider::EventDeletion,
    ) -> ProviderResult<()> {
        match &self.calendar {
            Some(calendar) => {
                super::calendar_write::delete(
                    &self.client,
                    calendar,
                    &self.calendar_key,
                    base,
                    deletion,
                )
                .await
            }
            None => Err(super::calendar::unbound_calendar()),
        }
    }
}
