// SPDX-License-Identifier: MPL-2.0
//! The trait half of the skeleton: what [`EasAdapter`] reports and which
//! scopes it names. No verb is overridden — the trait's rejecting defaults
//! are the honest behavior until each verb slice lands (the module docs in
//! `super` carry the ladder).

use engine_core::{ids::AccountId, sync::SyncScope};
use engine_provider::{ConnectionInfo, Provider};

use super::EasAdapter;

#[async_trait::async_trait]
impl Provider for EasAdapter {
    /// The verb-ladder capabilities plus the transport's negotiated HTTP
    /// version — composed per call, the Graph/JMAP precedent, because the
    /// version fact is live (most-recent observation) rather than latched.
    ///
    /// * **Capabilities are `none()` in this slice** — they follow the verbs that have landed, not
    ///   the server's OPTIONS answer; each verb slice flips its own bit as it implements its trait
    ///   method.
    /// * **`http_version`** is `None` until the [`EasAdapter::negotiate`] OPTIONS exchange (EAS's
    ///   session-discovery step — the JMAP/CalDAV connect-time precedent; Graph, which has no
    ///   discovery step, stays `None` until its first fetch), then whatever the transport most
    ///   recently observed.
    /// * **`tls_version`** is always `None`: reqwest exposes only the peer certificate, never the
    ///   negotiated protocol version (`docs/agent-guidance/tls.md`).
    /// * **`concurrent_fetches`** stays the `ConnectionInfo` default (1) until a measured
    ///   per-server EAS ceiling exists to justify a wider one — the Graph precedent set its 4 from
    ///   live throttling evidence.
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
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
}
