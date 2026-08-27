// SPDX-License-Identifier: MPL-2.0
//! The [`Provider`](engine_provider::Provider) adapter over the EAS client —
//! the skeleton slice: connection facts and the EAS scope overrides.
//!
//! ## Binding
//!
//! EAS item `Sync` carries one collection per request, so message sync is per
//! folder — like IMAP and Graph, an [`EasAdapter`] is bound to a single mail
//! folder (its [`email_scope`](engine_provider::Provider::email_scope) names
//! that folder) and the cross-folder fan-out is the orchestrator's job
//! (`docs/agent-guidance/eas.md`). The bound [`EasClient`] is cheap to clone
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
//! ## The verb ladder (capabilities stay honest)
//!
//! Capabilities follow the **verbs that have landed**, never the server's
//! OPTIONS answer: a bit turns on when the trait method honoring it is
//! implemented, because the trait's un-overridden defaults reject —
//! advertising a bit without its verb would point a capability-checking
//! caller straight at that rejection (`provider.rs`: "the default rejects,
//! so a capability-checking caller never relies on it"). This skeleton
//! lands connection + scopes only, so it advertises `Capabilities::none()`;
//! the mail verbs (FolderSync, Sync) and their bits land in later slices,
//! each flipping its bit here in the same move.
//!
//! ## Interior mutability is deliberately absent
//!
//! [`EasClient`]'s command methods take `&mut self` (the retry layers adopt
//! redirects and rotate policy keys in place), while the trait's verbs take
//! `&self`. This skeleton overrides no async verb, so it holds the client
//! plainly and [`EasAdapter::negotiate`] takes `&mut self` like any
//! connect-time step. The first verb slice (FolderSync) will introduce the
//! lock it actually needs — with the shape its calls demand — rather than a
//! speculative one here.

mod connection;

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
    /// The protocol client this adapter drives. Held plainly (see the module
    /// docs): the skeleton overrides no async verb, and `negotiate` is a
    /// connect-time `&mut self` step.
    client: EasClient,
    /// The bound folder — the `Sync` `CollectionId` (a folder ServerId) this
    /// adapter's email scope names, per the IMAP/Graph one-folder binding.
    folder: MailboxId,
    /// The verb ladder, read by
    /// [`connection_info`](engine_provider::Provider::connection_info):
    /// `none()` until a verb slice lands and flips its own bit (see the
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
            client,
            folder,
            // The honest ladder: this slice implements no verb, so nothing
            // is advertised. Verb slices flip their bits here as they land.
            capabilities: Capabilities::none(),
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
        let options = self.client.options().await?;
        let advertised = options.protocol_versions.join(", ");
        let Some(version) = pick_protocol_version(&advertised, &CLIENT_KNOWN_PROTOCOL_VERSIONS)
        else {
            return Err(EasError::Transport(format!(
                "OPTIONS advertised protocol versions [{advertised}] — none this client speaks ([{}])",
                CLIENT_KNOWN_PROTOCOL_VERSIONS.join(", ")
            )));
        };
        self.client.set_protocol_version(version.clone());
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
