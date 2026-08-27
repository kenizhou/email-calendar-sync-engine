// SPDX-License-Identifier: MPL-2.0
//! The trait half of the adapter: what [`EasAdapter`] reports, which scopes
//! it names, and the verbs that have landed (FolderSync). The un-overridden
//! defaults remain the honest behavior for every verb still to come (the
//! module docs in `super` carry the ladder).

use engine_core::{
    ids::AccountId,
    mail::Mailbox,
    sync::{SyncScope, SyncState},
};
use engine_provider::{ConnectionInfo, Provider, ProviderResult, ScopeSync};

use super::EasAdapter;

#[async_trait::async_trait]
impl Provider for EasAdapter {
    /// The verb-ladder capabilities plus the transport's negotiated HTTP
    /// version — composed per call, the Graph/JMAP precedent, because the
    /// version fact is live (most-recent observation) rather than latched.
    ///
    /// * **Capabilities are `none()` in this slice** — they follow the verbs that have landed, not
    ///   the server's OPTIONS answer. FolderSync alone does not flip `mail`: that bit names the
    ///   whole mail read domain (containers *and* messages), so it turns on with the message verbs.
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
}
