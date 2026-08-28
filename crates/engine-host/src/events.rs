//! The event contract between an engine's sync work and the shell hosting it.
//!
//! A host does not watch the engine work; the engine *tells* it what changed, as
//! plain data. [`EngineEvent`] is that vocabulary — what a committed folder round
//! landed, where an account's sync stands, that the outbox grew or drained, how a
//! send ended — and [`EventSink`] is the one-method ear a host implements to hear
//! it: the engine's round orchestration emits, the Kylins shell forwards each
//! event to Tauri as the JSON below, and the frontend types it back as a
//! discriminated union. The payloads are pure data — no behavior, no engine types,
//! ids as text — precisely so they can cross every boundary as a value: collected
//! by a test sink, serialized to the shell, logged. Both enums are
//! `#[non_exhaustive]`: the vocabulary grows with the slices P1 adds, and it must
//! grow without breaking a listener's match.
//!
//! # Wire shape
//!
//! serde's default externally tagged representation is the contract, under two
//! pinned spellings. `#[serde(rename_all = "snake_case")]` on each enum renames
//! the *variant tags* (`AccountStatus` becomes `"account_status"`); every field
//! name is already snake_case, so fields ride as declared. And every `Option`
//! payload field is `skip_serializing_if = "Option::is_none"`: a `None` is
//! **omitted** from the object — never sent as `null` — and its absence
//! deserializes back to `None`. So each variant is one JSON object whose only key
//! is the tag, the payload nested under it. The exact compact JSON, which the
//! frontend mirror (P1 T8) copies verbatim and the tests beside this module pin
//! to the byte:
//!
//! ```json
//! {"commit":{"account":"acct-1","folder":"INBOX","upserted":["m-1","m-2"],"removed":["m-9"],"fetched":3,"total":42}}
//! ```
//!
//! `total`, the round's whole size, appears only when the provider named one —
//! an unbounded fetch omits it:
//!
//! ```json
//! {"commit":{"account":"acct-1","folder":"Drafts","upserted":[],"removed":[],"fetched":0}}
//! ```
//!
//! `state` is one of `"idle"`, `"syncing"`, `"error"`, `"rate_limited"`; `detail`
//! — `error`'s code, `rate_limited`'s retry-after seconds — appears only when
//! `Some`:
//!
//! ```json
//! {"account_status":{"account":"acct-1","state":"syncing"}}
//! {"account_status":{"account":"acct-1","state":"rate_limited","detail":37}}
//! ```
//!
//! ```json
//! {"outbox_changed":{"account":"acct-1","pending":2}}
//! ```
//!
//! `detail`, the failure's reason, appears only on a failed send that has one:
//!
//! ```json
//! {"send_result":{"account":"acct-1","message_id":"m-1","success":true}}
//! {"send_result":{"account":"acct-1","message_id":"m-2","success":false,"detail":"554 rejected"}}
//! ```

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Where an account's sync work stands, as the one word a status surface shows.
///
/// Travels as itself on the wire — `"idle"`, `"syncing"`, `"error"`,
/// `"rate_limited"`, see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccountState {
    /// No round is running for the account.
    Idle,
    /// A round is running for the account.
    Syncing,
    /// The last round failed; the carrying event's `detail` says why.
    Error,
    /// The provider throttled the account; `detail` is the retry-after seconds.
    RateLimited,
}

/// What the engine tells its host as work lands: one pure-data value per fact.
///
/// The serde wire shape — externally tagged, snake_case tags and fields, `None`
/// options omitted — is the contract with the shell and its TypeScript mirror;
/// the module docs pin one exact JSON example per variant, and the tests pin
/// them to the byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// The enum-level rename applies to the variant *tags* (`AccountStatus` ->
// "account_status"); every field name is already snake_case and rides as
// declared.
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineEvent {
    /// A folder's round committed: these ids landed, these left.
    Commit {
        /// The account the folder belongs to, as text.
        account: String,
        /// The folder the round synced, by name.
        folder: String,
        /// The provider ids the commit upserted.
        upserted: Vec<String>,
        /// The provider ids the commit removed.
        removed: Vec<String>,
        /// How many messages the round fetched.
        fetched: usize,
        /// The round's whole size where the provider named one; absent when the
        /// fetch is unbounded.
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
    },
    /// Where an account's sync stands; emitted on every transition.
    AccountStatus {
        /// The account, as text.
        account: String,
        /// The account's state.
        state: AccountState,
        /// The state's payload: `error`'s code, `rate_limited`'s retry-after
        /// seconds. Omitted on the wire when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<i64>,
    },
    /// The account's outbox changed: a message left it or arrived in it.
    OutboxChanged {
        /// The account, as text.
        account: String,
        /// How many messages the outbox holds pending.
        pending: i64,
    },
    /// A send ended — in success, or with the failure's reason.
    SendResult {
        /// The account, as text.
        account: String,
        /// The sent message's id.
        message_id: String,
        /// Whether the send succeeded.
        success: bool,
        /// The failure's reason. Omitted on the wire when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// The ear a host lends the engine: one method, called once per event.
///
/// `Send + Sync` so the same sink hears from whichever task a round runs on,
/// object-safe so emitting code holds it as `Arc<dyn EventSink>` or a plain
/// `&dyn EventSink` without naming the host's concrete type.
pub trait EventSink: Send + Sync {
    /// Records one event. Called once per event, from whichever task emits it;
    /// implementations choose their own buffering.
    fn emit(&self, event: EngineEvent);
}

/// A sink that keeps every event it hears, in order — the observer of record for
/// tests around emitting code (this crate's and the round orchestration's).
#[derive(Debug, Default)]
pub struct CollectingSink(Mutex<Vec<EngineEvent>>);

impl CollectingSink {
    /// A snapshot of the events heard so far, in emission order.
    pub fn events(&self) -> Vec<EngineEvent> {
        self.0
            .lock()
            .expect("the lock is poisoned only if an emit panicked mid-push")
            .clone()
    }

    /// Forgets every event heard so far.
    pub fn clear(&self) {
        self.0
            .lock()
            .expect("the lock is poisoned only if an emit panicked mid-push")
            .clear();
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: EngineEvent) {
        self.0
            .lock()
            .expect("the lock is poisoned only if an emit panicked mid-push")
            .push(event);
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod events_tests;
