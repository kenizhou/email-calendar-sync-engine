// SPDX-License-Identifier: MPL-2.0
//! The trait half of the adapter: what [`EasAdapter`] reports, which scopes
//! it names, and the verbs that have landed (FolderSync, Sync class Email,
//! ItemOperations message-source fetch). The un-overridden defaults remain
//! the honest behavior for every verb still to come (the module docs in
//! `super` carry the ladder).

use engine_core::{
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncWindow},
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
        super::mailboxes::sync(&self.client, cursor).await
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
}
