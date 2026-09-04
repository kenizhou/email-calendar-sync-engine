// SPDX-License-Identifier: MPL-2.0
//! The [`Provider`](engine_provider::Provider) adapter over the EAS client —
//! connection facts, the EAS scope overrides, the read verbs (FolderSync
//! containers for mail AND calendars, Sync class Email messages and class
//! Calendar events, ItemOperations message-source fetch), and the write
//! verbs (SendMail submission from a `Draft` or caller-rendered bytes;
//! keyword edits, moves, and the documented per-verb refusals of
//! `edit_mail`).
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
//! with every mail verb live, and this slice reaches that bar. The
//! `message_source` bit is **on** too: `fetch_message_source` (the
//! ItemOperations MIME fetch with range reassembly) landed. The write bits
//! are **on** with this slice: `mail_writes` (`edit_mail` — keyword edits
//! and moves; `Delete` is refused per the protocol, see `mutate`) and
//! `submission` + `scheduling_submission` (`submit_email` and
//! `submit_email_source` — the raw-MIME send carries its own scheduling
//! parameters). The **calendar family is on with its binding**
//! ([`EasAdapter::with_calendar`]): the read verbs (`sync_calendars` +
//! `sync_events`, `adapter/calendar.rs`), the write verbs
//! (`create_event`/`patch_event`/`delete_event` — Sync
//! Add/Change/Delete-upsync, `adapter/calendar_write.rs`; `put_event` is
//! refused, EAS's update verb is a field-level Change, not a document PUT)
//! flip `calendars` + `calendar_writes` together — event addressing is per
//! collection and an unbound adapter cannot name one. `calendar_rsvp` is
//! on with the binding too (`rsvp_event_from_invite` over `MeetingResponse`
//! — the controls composed per call from the negotiated version, see
//! [`EasAdapter::rsvp_controls`]). The **contacts family is on with its
//! binding** ([`EasAdapter::with_contacts`], P2 Task 5): the read verbs
//! (`sync_address_books` + `sync_contacts`, `adapter/contacts.rs`) and the
//! write verbs (`create_contact`/`patch_contact`/`delete_contact` — Sync
//! Add/Change/Delete-upsync) flip `contacts` + `contact_writes` together,
//! exactly the calendar shape. `contact_photos` stays off — see
//! [`EasAdapter::with_contacts`] for the honest refusal.
//!
//! ## The calendar binding
//!
//! EAS item `Sync` carries one collection per request, so calendar event
//! sync is per calendar folder — like email per mail folder, and exactly the
//! [`GraphCalendarProvider`](provider_graph::GraphCalendarProvider) /
//! `CalDavProvider` shape. A host builds its calendar adapters from the
//! container sync's discovery: any adapter can list the calendars
//! (`sync_calendars` is per-account FolderSync), then each event-syncing
//! adapter is bound to one calendar folder with
//! [`EasAdapter::with_calendar`] (or [`EasAdapter::calendar_adapter`] — one
//! ServerId serving as both bindings for a calendar-only host). The
//! cross-calendar fan-out is the orchestrator's job, exactly as for mail.
//!
//! ## The collection-key ledger
//!
//! An EAS `SyncKey` is per-collection server state the client must thread
//! through every command — and the trait's write seam (`edit_mail`) carries
//! no cursor. The adapter therefore owns a one-key ledger for the bound
//! folder: a completed `stream_email` pass records its final key (the same
//! value the engine persists as its cursor — the cursor stays the
//! authority for what has been delivered), and a `SetKeywords` edit rides
//! the ledger's key and records its rotation. Resuming a pass from a
//! rotation is lossless because the upsync request sends no `GetChanges`
//! ([MS-ASCMD]: invalid in 16.1) — a rotation carries no server rows. A
//! cold ledger (a fresh adapter that has not yet observed a pass) refuses
//! `NeedsResync` rather than guessing: the orchestrator re-syncs, the pass
//! re-seeds, the outbox retries the op. See `mutate` for the write-side
//! discipline and `email` for the pass-side rule.

mod calendar;
mod calendar_write;
mod connection;
mod contacts;
mod contacts_write;
mod email;
mod error;
mod hierarchy;
mod ledger;
mod mailboxes;
mod mutate;
mod source;
mod submit;
mod watch;

use engine_core::ids::{AddressBookId, CalendarId, MailboxId};
use engine_provider::{Capabilities, OverrideSurvival, RsvpControls, WriteGuard};
pub use watch::EasPingWatcher;

use crate::client::{EasClient, EasError, pick_protocol_version};

pub(super) use ledger::{CollectionKey, current_key, record_rotation};

/// The protocol versions this adapter can negotiate over OPTIONS: exactly
/// the versions whose feature gates the crate implements — `MeetingResponse`
/// `InstanceId` is 14.1+ ([MS-ASWBXML] §2.1.2.1.9), the calendar
/// `airsyncbase:Location` container is 16.0+ ([MS-ASWBXML] §2.1.2.1.5 note
/// 2), `GetChanges` omission is 16.1 ([MS-ASCMD] §2.2.3.29). Nothing older
/// has been gated for or exercised, so it is not claimed.
pub const CLIENT_KNOWN_PROTOCOL_VERSIONS: [&str; 3] = ["14.1", "16.0", "16.1"];

/// An EAS read/sync provider bound to one mail folder for email and,
/// optionally, one calendar folder for events and one contact folder for
/// cards (see the module docs' binding sections).
///
/// Construct with [`EasAdapter::new`] from a configured [`EasClient`] and
/// the folder to bind, then [`EasAdapter::negotiate`] at connection time.
/// The folder list syncs under the per-account
/// [`SyncScope::EasFolderList`](engine_core::sync::SyncScope::EasFolderList);
/// email syncs under the bound folder's
/// [`SyncScope::EasFolder`](engine_core::sync::SyncScope::EasFolder). With a
/// calendar binding ([`EasAdapter::with_calendar`]) the calendar containers
/// sync under [`SyncScope::EasCalendarList`](engine_core::sync::SyncScope::EasCalendarList)
/// and events under the bound calendar's
/// [`SyncScope::EasCalendar`](engine_core::sync::SyncScope::EasCalendar).
/// With a contacts binding ([`EasAdapter::with_contacts`]) the contact
/// folders sync under
/// [`SyncScope::EasContactList`](engine_core::sync::SyncScope::EasContactList)
/// and cards under the bound folder's
/// [`SyncScope::EasContact`](engine_core::sync::SyncScope::EasContact).
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
    /// The bound calendar folder — the class-`Calendar` `CollectionId` this
    /// adapter's event scope names, when the adapter serves the calendar
    /// family (`None` until [`EasAdapter::with_calendar`]; its capabilities
    /// then never advertise the family).
    calendar: Option<CalendarId>,
    /// The bound contact folder — the class-`Contacts` `CollectionId` this
    /// adapter's card scope names, when the adapter serves the contacts
    /// family (`None` until [`EasAdapter::with_contacts`]; its capabilities
    /// then never advertise the family).
    address_book: Option<AddressBookId>,
    /// The verb ladder, read by
    /// [`connection_info`](Provider::connection_info):
    /// `mail` since the read verbs (`sync_mailboxes` + `stream_email`) both
    /// landed; `mail_writes`/`submission` since the write verbs landed;
    /// `calendars` since the calendar read verbs landed WITH their binding
    /// ([`EasAdapter::with_calendar`] sets both together); every other bit
    /// stays off until its verb does (see the module docs). Deliberately not
    /// a constructor parameter — a host must not be able to advertise a verb
    /// this adapter does not implement.
    capabilities: Capabilities,
    /// The bound folder's collection-SyncKey ledger — the write path's key
    /// source (the trait's `edit_mail` carries no cursor). Seeded by a
    /// completed `stream_email` pass, consumed and rotated by a
    /// `SetKeywords` edit; `None` until the first pass completes (see the
    /// module docs' ledger section).
    collection_key: CollectionKey,
    /// The bound calendar folder's collection-SyncKey ledger — the calendar
    /// write verbs' key source (the trait's write seam carries no cursor,
    /// exactly as `edit_mail`). Seeded by a completed `sync_events` pass,
    /// consumed and rotated by each calendar write; `None` until the first
    /// pass completes (the same cold-ledger `NeedsResync` refusal).
    calendar_key: CollectionKey,
    /// The bound contact folder's collection-SyncKey ledger — the contacts
    /// write verbs' key source (the same discipline as `calendar_key`).
    /// Seeded by a completed `sync_contacts` pass, consumed and rotated by
    /// each contacts write; `None` until the first pass completes.
    contacts_key: CollectionKey,
    /// The shared account-level hierarchy-SyncKey ledger — one server
    /// FolderSync cursor serving both container scopes (see `hierarchy`'s
    /// module docs: the key the server last handed this adapter, plus the
    /// rows a riding scope missed).
    hierarchy: hierarchy::HierarchyLedger,
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
            .field("calendar", &self.calendar)
            .field("address_book", &self.address_book)
            .field("capabilities", &self.capabilities)
            .field("collection_key", &self.collection_key)
            .field("calendar_key", &self.calendar_key)
            .field("contacts_key", &self.contacts_key)
            .field("hierarchy", &self.hierarchy)
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
            // The calendar binding arrives only through `with_calendar`
            // (which flips the `calendars` bit with it — the honest ladder);
            // the contacts binding only through `with_contacts`.
            calendar: None,
            address_book: None,
            // The honest ladder: `mail` names containers AND messages —
            // both read verbs are live, so the bit is on; ditto
            // `message_source`; and the write verbs (edit_mail,
            // submit_email/submit_email_source) turn their bits on with
            // this slice (module docs).
            capabilities: Capabilities::none()
                .with_mail()
                .with_message_source()
                .with_mail_writes()
                .with_submission()
                .with_scheduling_submission(),
            // Cold by construction — the first completed pass seeds it.
            collection_key: CollectionKey::default(),
            calendar_key: CollectionKey::default(),
            contacts_key: CollectionKey::default(),
            hierarchy: hierarchy::HierarchyLedger::default(),
            protocol_version: None,
        }
    }

    /// Binds the calendar family: names the calendar folder whose events
    /// this adapter syncs ([`event_scope`](Provider::event_scope) →
    /// [`SyncScope::EasCalendar`](engine_core::sync::SyncScope::EasCalendar))
    /// and turns the `calendars` capability bit on with it — the bit and
    /// the binding land together because event sync is per collection: an
    /// unbound adapter cannot name one, so advertising the family without a
    /// binding would point a capability-checking caller straight at the
    /// `InvalidState` refusal.
    ///
    /// The binding is the calendar folder's ServerId from the container sync
    /// (`sync_calendars` — itself per-account and callable on any adapter).
    ///
    /// The **write verbs land with the binding too** (P2 Task 3:
    /// `create_event`/`patch_event`/`delete_event` over Sync
    /// Add/Change/Delete): the bit and the binding turn on together because
    /// every write verb addresses the bound collection, exactly as the read
    /// bit did. The guard is honest: EAS Sync Change carries **no server
    /// revision tokens** ([MS-ASSYNC] has no per-object precondition — the
    /// request names the item and nothing else), so the last write silently
    /// wins and the guard is [`WriteGuard::Absent`]. The override-survival
    /// claim is `kept()` **by construction**: a series Replace rebuilds the
    /// whole `Exceptions` container from the base the caller read
    /// (`calendar/convert_write.rs`), so every per-occurrence change rides
    /// the write — the CalDAV structural-patcher argument; a stale base
    /// losing a newer override is the `Absent` guard's documented
    /// last-write-wins, not a survival failure. The RSVP bit lands with the
    /// binding as well (P2 Task 4: `rsvp_event_from_invite` over
    /// `MeetingResponse`), composed per call from the negotiated version —
    /// see [`EasAdapter::rsvp_controls`].
    #[must_use]
    pub fn with_calendar(mut self, calendar: CalendarId) -> Self {
        self.calendar = Some(calendar);
        self.capabilities = self
            .capabilities
            .with_calendars()
            .with_calendar_writes(WriteGuard::Absent, OverrideSurvival::kept());
        self
    }

    /// The calendar-role constructor: one calendar folder ServerId serving
    /// as both bindings, for a host that syncs calendars only through this
    /// adapter (`EasAdapter::new(client, folder).with_calendar(calendar)`
    /// collapsed — the folder binding holds the same ServerId under its mail
    /// id type, unused for mail by a calendar-role host).
    #[must_use]
    pub fn calendar_adapter(client: EasClient, calendar: CalendarId) -> Self {
        let folder = MailboxId::try_from(calendar.as_str()).unwrap_or_else(|e| {
            unreachable!("a ServerId that keys a CalendarId keys a MailboxId too: {e}")
        });
        Self::new(client, folder).with_calendar(calendar)
    }

    /// Binds the contacts family: names the contact folder whose cards
    /// this adapter syncs ([`contact_scope`](engine_provider::ContactsProvider::contact_scope)
    /// → [`SyncScope::EasContact`](engine_core::sync::SyncScope::EasContact))
    /// and turns the `contacts` capability bit on with it — the bit and
    /// the binding land together because card sync is per collection: an
    /// unbound adapter cannot name one, so advertising the family without
    /// a binding would point a capability-checking caller straight at the
    /// `InvalidState` refusal (the `with_calendar` precedent).
    ///
    /// The binding is the contact folder's ServerId from the container
    /// sync (`sync_address_books` — itself per-account and callable on
    /// any adapter).
    ///
    /// The **write verbs land with the binding too** (P2 Task 5:
    /// `create_contact`/`patch_contact`/`delete_contact` over Sync
    /// Add/Change/Delete): the `contact_writes` bit turns on with the
    /// binding because every write verb addresses the bound collection.
    /// The guard is honest: EAS Sync Change carries **no server revision
    /// tokens** ([MS-ASSYNC] has no per-object precondition — the
    /// request names the item and nothing else), so the last write
    /// silently wins and the guard is [`WriteGuard::Absent`] (the
    /// `with_calendar` ruling verbatim). `contact_photos` stays OFF: the
    /// EAS `Picture` is inline payload dropped at parse time (v1 ruling,
    /// pinned by the picture tests) and no fetchable URI survives to
    /// address an ItemOperations round — `fetch_contact_photo` keeps its
    /// rejecting default rather than claiming a verb it cannot honor.
    /// `contact_groups` stays off too (distribution lists are a separate,
    /// unmodeled container).
    #[must_use]
    pub fn with_contacts(mut self, address_book: AddressBookId) -> Self {
        self.address_book = Some(address_book);
        self.capabilities = self
            .capabilities
            .with_contacts()
            .with_contact_writes(WriteGuard::Absent);
        self
    }

    /// The contacts-role constructor: one contact folder ServerId serving
    /// as both bindings, for a host that syncs contacts only through this
    /// adapter (`EasAdapter::new(client, folder).with_contacts(book)`
    /// collapsed — the folder binding holds the same ServerId under its
    /// mail id type, unused for mail by a contacts-role host).
    #[must_use]
    pub fn contacts_adapter(client: EasClient, address_book: AddressBookId) -> Self {
        let folder = MailboxId::try_from(address_book.as_str()).unwrap_or_else(|e| {
            unreachable!("a ServerId that keys an AddressBookId keys a MailboxId too: {e}")
        });
        Self::new(client, folder).with_contacts(address_book)
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

    /// The RSVP controls the **negotiated** version can honour — the facts
    /// [`connection_info`](Provider::connection_info) composes into the
    /// `calendar_rsvp` capability and the write path consults before the wire,
    /// so the two can never disagree:
    ///
    /// - **comment: false** — `MeetingResponse` carries no note element on any protocol version
    ///   ([MS-ASWBXML] page 8 has nowhere to put one), so a note is refused rather than silently
    ///   dropped.
    /// - **suppress_notification** — `true` only where the `SendResponse` token exists (16.0/16.1,
    ///   [MS-ASWBXML] §2.1.2.1.9): there, presence asks the server to email the organizer and
    ///   absence keeps it out. On 14.1 the token is unregistered and the server emails per its own
    ///   default, so silence cannot be promised. Pre-negotiation (`None`) the conservative shape
    ///   stands: no version is known, so no client choice is claimed.
    /// - **guard: [`WriteGuard::Absent`]** — `MeetingResponse` names the email and nothing else;
    ///   there is no revision token to guard on.
    fn rsvp_controls(&self) -> RsvpControls {
        RsvpControls {
            comment: false,
            suppress_notification: matches!(
                self.protocol_version.as_deref(),
                Some("16.0" | "16.1")
            ),
            guard: WriteGuard::Absent,
        }
    }

    /// Builds a [`Watch`](engine_provider::Watch) session — an
    /// [`EasPingWatcher`] long-polling `Ping` for the bound folder. The
    /// concrete-type handout (the trait has no watch accessor; the IMAP
    /// dedicated-connection precedent, recorded as the optional fork in
    /// `eas.md`): the watcher OWNS a clone of the client, taken under the
    /// verb lock here and released — its long holds never contend the
    /// adapter's verbs. Build it after [`EasAdapter::negotiate`] so the
    /// clone carries the negotiated protocol version; the session's
    /// heartbeat tuning survives restarts via
    /// [`EasPingWatcher::heartbeat_secs`] /
    /// [`EasPingWatcher::set_heartbeat_secs`].
    pub async fn watcher(&self) -> EasPingWatcher {
        EasPingWatcher::new(self.client.lock().await.clone(), self.folder.clone())
    }
}

// ============================================================================
// The shared collection-key ledger discipline (mutate + calendar_write +
// contacts) — lives in `ledger`, re-exported above.
