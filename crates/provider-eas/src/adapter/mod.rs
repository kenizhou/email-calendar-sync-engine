// SPDX-License-Identifier: MPL-2.0
//! The [`Provider`](engine_provider::Provider) adapter over the EAS client —
//! connection facts, the EAS scope overrides, and the read verbs
//! (FolderSync containers, Sync class Email messages).
//!
//! ## Binding
//!
//! EAS item `Sync` carries one collection per request, so message sync is per
//! folder — like IMAP and Graph, an [`EasAdapter`] is bound to a single mail
//! folder (its [`email_scope`](engine_provider::Provider::email_scope) names
//! that folder) and the cross-folder fan-out is the orchestrator's job
//! (`docs/agent-guidance/eas.md`). The bound
//! [`EasClient`](crate::client::EasClient) is cheap to clone
//! and clones share one pooled HTTP transport, so an account's folder
//! adapters may share a client.
//!
//! ## Connection time
//!
//! EAS has a session-discovery step — the HTTP `OPTIONS` exchange
//! ([MS-ASHTTP] §2.2.1.1) — so the JMAP/CalDAV `connect` precedent applies:
//! a host builds the adapter, calls [`EasAdapter::negotiate`] (the OPTIONS
//! first contact: the server's advertised versions negotiated down to one,
//! applied to the client, recorded adapter-side), and only then reads
//! [`connection_info`](engine_provider::Provider::connection_info) for
//! facts the exchange learned. Nothing goes out before that: reading
//! connection facts or scopes is free.
//!
//! ## The verb lock
//!
//! [`EasClient`](crate::client::EasClient)'s command methods take `&mut self` — the retry layers
//! adopt redirects and rotate policy keys in place, and FolderSync rotates the
//! cached hierarchy key — while the trait's verbs take `&self`. The verb
//! slice therefore holds the client behind a [`tokio::sync::Mutex`]: the
//! IMAP precedent (its session sits "behind an async `Mutex` — concurrent
//! `stream_email` calls serialize onto one connection"). For EAS the
//! serialized state is the client's *session* facts (hierarchy key, policy
//! key, adopted URL), not an exclusive socket. The one sync reader,
//! [`connection_info`](engine_provider::Provider::connection_info), must not
//! take that lock (an async lock from a sync method), so the adapter holds
//! the [`ObservedHttpVersion`](engine_provider::ObservedHttpVersion) funnel
//! directly — the same `Arc` the client
//! records every response into, taken as a handle at construction
//! ([`EasClient::http_version_handle`](crate::client::EasClient::http_version_handle)) — and reads
//! the live, most-recent-wins fact lock-free. An email stream holds the lock for its
//! whole pass, like IMAP's held connection guard.
//!
//! ## The verb ladder (capabilities stay honest)
//!
//! Capabilities follow the **verbs that have landed**, never the server's
//! OPTIONS answer: a bit turns on when the trait method honoring it is
//! implemented, because the trait's un-overridden defaults reject —
//! advertising a bit without its verb would point a capability-checking
//! caller straight at that rejection (`provider.rs`: "the default rejects,
//! so a capability-checking caller never relies on it").
//!
//! The `mail` bit is **on** in this slice: `sync_mailboxes` (the
//! containers) and `stream_email` (the messages) are both live, which is
//! the whole mail read domain the bit names — IMAP/Graph advertise it only
//! with every mail verb live, and this slice reaches that bar. The bits
//! that name domains beyond it stay off until their verbs land:
//! `mail_writes` (`edit_mail`), `message_source`
//! (`fetch_message_source`), `submission` (`submit_email`), and the
//! calendar/contacts families.

mod connection;
mod email;
mod error;
mod mailboxes;

use engine_core::ids::MailboxId;
use engine_provider::Capabilities;

use crate::client::{EasClient, EasError, pick_protocol_version};

/// The protocol versions this adapter can negotiate over OPTIONS: exactly
/// the versions whose feature gates the crate implements — `MeetingResponse`
/// `InstanceId` is 14.1+ ([MS-ASWBXML] §2.1.2.1.9), the calendar
/// `airsyncbase:Location` container is 16.0+ ([MS-ASWBXML] §2.1.2.1.5 note
/// 2), `GetChanges` omission is 16.1 ([MS-ASCMD] §2.2.3.29). Nothing older
/// has been gated for or exercised, so it is not claimed.
pub const CLIENT_KNOWN_PROTOCOL_VERSIONS: [&str; 3] = ["14.1", "16.0", "16.1"];

/// An EAS read/sync provider bound to one mail folder for email.
///
/// Construct with [`EasAdapter::new`] from a configured [`EasClient`] and
/// the folder to bind, then [`EasAdapter::negotiate`] at connection time.
/// The folder list syncs under the per-account
/// [`SyncScope::EasFolderList`](engine_core::sync::SyncScope::EasFolderList);
/// email syncs under the bound folder's
/// [`SyncScope::EasFolder`](engine_core::sync::SyncScope::EasFolder).
pub struct EasAdapter {
    /// The protocol client this adapter drives, behind the verb lock (see
    /// the module docs): command methods rotate session state in place, so
    /// verbs serialize onto one client — the IMAP connection-lock precedent.
    client: tokio::sync::Mutex<EasClient>,
    /// The transport-facts funnel, shared with the locked client — the
    /// lock-free read side of [`connection_info`](Provider::connection_info)
    /// (a sync method cannot take the async lock). Never sends anything.
    http: std::sync::Arc<engine_provider::ObservedHttpVersion>,
    /// The bound folder — the `Sync` `CollectionId` (a folder ServerId) this
    /// adapter's email scope names, per the IMAP/Graph one-folder binding.
    folder: MailboxId,
    /// The verb ladder, read by
    /// [`connection_info`](engine_provider::Provider::connection_info):
    /// `mail` since the read verbs (`sync_mailboxes` + `stream_email`) both
    /// landed; every other bit stays off until its verb does (see the
    /// module docs). Deliberately not a constructor parameter — a host must
    /// not be able to advertise a verb this adapter does not implement.
    capabilities: Capabilities,
    /// The OPTIONS-negotiated protocol version ("16.1"-shaped), or `None`
    /// before [`EasAdapter::negotiate`]. Adapter-held by design: a host must
    /// not branch on it (`docs/agent-guidance/providers.md`), so it never
    /// enters `ConnectionInfo`.
    protocol_version: Option<String>,
}

/// Delegates to `EasClient`'s redacting Debug (the config carries
/// credentials); the adapter adds nothing secret.
impl std::fmt::Debug for EasAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EasAdapter")
            .field("client", &self.client)
            // The funnel's Debug is its atomic's — the observed version or
            // None — no secret either; omitted fields would hide the
            // lock-free read side from panic inspection.
            .field("http", &self.http.get())
            .field("folder", &self.folder)
            .field("capabilities", &self.capabilities)
            .field("protocol_version", &self.protocol_version)
            .finish()
    }
}

impl EasAdapter {
    /// Binds a configured client to one mail folder for email sync — the
    /// `GraphProvider::new` shape. The account arrives per trait call (every
    /// verb and scope accessor receives it), so it is not stored: an unused
    /// field would be dead state.
    #[must_use]
    pub fn new(client: EasClient, folder: MailboxId) -> Self {
        Self {
            // The lock the FolderSync slice demanded (module docs): verbs
            // serialize onto the client's in-place session mutations, while
            // the funnel handle keeps connection_info lock-free.
            http: client.http_version_handle(),
            client: tokio::sync::Mutex::new(client),
            folder,
            // The honest ladder: `mail` names containers AND messages —
            // both read verbs are live in this slice, so the bit is on
            // (module docs).
            capabilities: Capabilities::none().with_mail(),
            protocol_version: None,
        }
    }

    /// The connection-time OPTIONS exchange ([MS-ASHTTP] §2.2.1.1): the
    /// server's advertised protocol versions negotiated against
    /// [`CLIENT_KNOWN_PROTOCOL_VERSIONS`] — the last client-known entry in
    /// the server's listed order ([`pick_protocol_version`]) — then applied
    /// to the client (every later command carries it as
    /// `MS-ASProtocolVersion`) and recorded adapter-side. The exchange is
    /// also the transport's first contact, so from here on
    /// [`connection_info`](engine_provider::Provider::connection_info)
    /// reports the HTTP version that response spoke.
    ///
    /// Still `&mut self` (the connect-time shape, like any dial): a host
    /// negotiates before spawning sync work, so the verb lock is
    /// uncontended by construction.
    ///
    /// A server sharing no version with the client is an explicit
    /// connect-time failure — never a silent fall back to the configured
    /// default version, which would only defer the mismatch to the first
    /// command.
    ///
    /// # Errors
    ///
    /// Returns [`EasError`] from the OPTIONS round-trip itself, or
    /// `EasError::Transport` when the intersection is empty.
    pub async fn negotiate(&mut self) -> Result<String, EasError> {
        let mut client = self.client.lock().await;
        let options = client.options().await?;
        let advertised = options.protocol_versions.join(", ");
        let Some(version) = pick_protocol_version(&advertised, &CLIENT_KNOWN_PROTOCOL_VERSIONS)
        else {
            return Err(EasError::Transport(format!(
                "OPTIONS advertised protocol versions [{advertised}] — none this client speaks ([{}])",
                CLIENT_KNOWN_PROTOCOL_VERSIONS.join(", ")
            )));
        };
        client.set_protocol_version(version.clone());
        drop(client);
        self.protocol_version = Some(version.clone());
        Ok(version)
    }

    /// The OPTIONS-negotiated protocol version, or `None` before
    /// [`EasAdapter::negotiate`]. Adapter-held by design — a host must not
    /// branch on it (`docs/agent-guidance/providers.md`), so it does not
    /// enter `ConnectionInfo`; the host persists it alongside the policy key
    /// if it wants it across restarts.
    #[must_use]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }
}
