//! The round orchestration: one account, one bounded pass, every fact reported
//! as it lands.
//!
//! [`run_account_round`] is the single entry a host's scheduler calls — a
//! desktop timer or a mobile OS background hook alike — and it is deliberately
//! the *only* thing of that shape: composition, no policy. It brackets the
//! account's status ([`AccountState::Syncing`] … a terminal), folds every chunk
//! the engine's own `Engine::sync_mail` commits into one [`EngineEvent::Commit`]
//! on the sink, drains the durable outbox through `Engine::drain_mail_ops` with
//! one [`EngineEvent::SendResult`] per op that reached an outcome and one
//! [`EngineEvent::OutboxChanged`] for the depth the drain left, and hands the
//! whole story back as [`RoundReport`]. What it does **not** do is everything a
//! scheduler owns: no timing, no loop, no backoff, no product decisions — when
//! to run again is the caller's, and a deeper outbox backlog needs another
//! round, not an internal one (the facade's drain is one bounded batch by
//! design).
//!
//! # The per-chunk commit policy
//!
//! Every chunk the engine commits, the round reports — including a chunk whose
//! delta is empty. The observer fires per *durable commit*, and each carries
//! the running `fetched`/`total`, which is exactly the "downloaded Y of X"
//! heartbeat a progress surface needs between the chunks that move rows; a
//! policy of swallowing quiet chunks would freeze that surface for the whole
//! length of a slow, empty fetch. The complement holds too: a provider that
//! streams nothing commits nothing, and the adapter never fabricates a commit —
//! an empty round emits only its status bracket.
//!
//! # The drain's per-op reporting
//!
//! The facade's drain returns a count, not per-op outcomes, so the round
//! reconstructs them from the outbox's own rows, read through the host seam
//! before and after: a row that was runnable and now holds a terminal state is
//! one this drain drove. The event's `message_id` is the op's resource key —
//! the outbox's durable per-send identity, minted from the `Message-ID` on both
//! submit paths (`draft:{Message-ID}`) — because the intent payload inside the
//! row is a type the facade does not re-export, and the key needs no decoding to
//! stay stable. The failure's reason text is likewise not recoverable: Phase 1
//! persists only the op's state, so a `Failed` send carries no detail, while a
//! parked `NeedsConfirmation` carries that state's own name as its detail —
//! *parked, not dead* being the fact a host most needs before it retries
//! anything. `OutboxChanged` fires only when the drain actually settled
//! something: an outbox nothing touched is not news, and an empty round stays
//! exactly two events long.

use engine_api::{
    AccountId, Engine, MailSyncReport, Provider, StreamTuning, SyncCommit, SyncError, SyncObserver,
};
use engine_core::{error::FailureClass, sync::SyncScope};
use rusqlite::Connection;

use crate::events::{AccountState, EngineEvent, EventSink};

/// What one account round did: the sync's own report, the drain's count, and
/// the round's one-word verdict.
#[derive(Debug)]
pub struct RoundReport {
    /// The mail sync's per-scope report, returned whole — which folder was
    /// busy, which refused, which applied — because "the account failed" is not
    /// an answer a host can act on.
    pub sync: MailSyncReport,
    /// How many outbox ops this round's drain drove to a recorded outcome. An
    /// op skipped as another executor's scope is not counted (the facade's own
    /// accounting), and a drain call that itself faulted counts zero.
    pub drained: usize,
    /// Whether the round completed cleanly: every sync scope applied **and**
    /// the drain call itself did not fault. A drain fault leaves the account's
    /// sync standing untouched — the terminal status says `Idle` while `clean`
    /// says the round as a whole owes its caller another look.
    pub clean: bool,
}

/// Drives one account round: status bracket, streamed sync, outbox drain,
/// terminal status.
///
/// The five steps, in order: `Syncing` to the sink; `Engine::sync_mail` under a
/// [`SyncObserver`] that folds each commit; `Engine::drain_mail_ops` through the
/// first provider (the transport the facade's drain takes — the same one
/// `sync_mail` itself uses for the account's folder list; no providers means no
/// drain, and `drained` stays zero); one `SendResult` per op the drain settled
/// plus one `OutboxChanged` when it settled any; then the terminal status from
/// the sync report alone — `Idle` when every scope applied, `RateLimited` with
/// the throttle's retry-after seconds when the first failure was a throttle,
/// `Error` otherwise. No timers and no loops: see the module docs.
///
/// The sink is told everything exactly once, in emission order, from whichever
/// task the engine runs the pass on — a sink implementation only needs to be
/// `Send + Sync` and cheap.
pub async fn run_account_round<P: Provider>(
    engine: &Engine,
    providers: &[P],
    account: &AccountId,
    tuning: StreamTuning,
    sink: &dyn EventSink,
) -> RoundReport {
    let name = account.as_str().to_owned();
    sink.emit(EngineEvent::AccountStatus {
        account: name.clone(),
        state: AccountState::Syncing,
        detail: None,
    });

    let observer = RoundObserver {
        account: name.clone(),
        sink,
    };
    let sync = engine
        .sync_mail(providers, account, tuning, &observer)
        .await;

    let before = outbox(engine, account).await;
    let (drained, drain_ok) = match providers.first() {
        Some(provider) => match engine.drain_mail_ops(provider, account).await {
            Ok(count) => (count, true),
            Err(_) => (0, false),
        },
        None => (0, true),
    };
    let after = outbox(engine, account).await;

    let settled = settled_between(&before, &after);
    for (resource, state) in &settled {
        sink.emit(EngineEvent::SendResult {
            account: name.clone(),
            message_id: resource.clone(),
            success: state.succeeded(),
            detail: state.detail().map(str::to_owned),
        });
    }
    if !settled.is_empty() {
        sink.emit(EngineEvent::OutboxChanged {
            account: name.clone(),
            pending: depth(&after),
        });
    }

    let (state, detail) = terminal(&sync);
    sink.emit(EngineEvent::AccountStatus {
        account: name,
        state,
        detail,
    });

    RoundReport {
        clean: sync.is_ok() && drain_ok,
        sync,
        drained,
    }
}

/// The `SyncObserver` the round hands `sync_mail`: every commit the engine
/// makes becomes one `Commit` event on the round's sink, with the scope's
/// folder, the provider keys that moved, and the running progress.
struct RoundObserver<'a> {
    /// The account, resolved once so every event carries the same text.
    account: String,
    /// Where the folded commits go.
    sink: &'a dyn EventSink,
}

impl SyncObserver for RoundObserver<'_> {
    fn committed(&self, commit: &SyncCommit<'_>) {
        self.sink.emit(EngineEvent::Commit {
            account: self.account.clone(),
            folder: folder_of(commit.scope),
            upserted: commit
                .upserted
                .iter()
                .map(|message| message.id.key().as_str().to_owned())
                .collect(),
            removed: commit
                .removed
                .iter()
                .map(|key| key.as_str().to_owned())
                .collect(),
            fetched: commit.fetched,
            total: commit.total,
        });
    }
}

/// The folder a committed chunk's scope names, as the event's `folder` string.
///
/// The per-folder protocols (IMAP, Graph, EAS) name the folder in the scope
/// itself; the account-wide scopes (a JMAP `Email` scope, Gmail's messages)
/// have no folder to name, so the event carries the scope's own designation —
/// one round, one string, no protocol leaking. The *id*, not the display name:
/// the name lives in the folder list, a store read a synchronous observer
/// cannot make, and the id is the stable identity a host correlates by anyway.
fn folder_of(scope: &SyncScope) -> String {
    match scope {
        SyncScope::ImapMailbox { mailbox, .. } => mailbox.as_str().to_owned(),
        SyncScope::GraphFolder { folder, .. } | SyncScope::EasFolder { folder, .. } => {
            folder.as_str().to_owned()
        }
        // A mail commit can only come from the Email data type; there is no
        // per-folder scope to name underneath it.
        SyncScope::JmapType { .. } => "Email".to_owned(),
        SyncScope::GmailMessages { .. } => "Messages".to_owned(),
        unreachable_scope => format!("{unreachable_scope:?}"),
    }
}

/// The outbox row states the round distinguishes, read back as the store's own
/// state spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpRow {
    /// Still awaiting an outcome — `Pending`, or `InFlight` under any lease,
    /// live or expired (an expired lease is runnable again, not settled).
    Runnable,
    /// Driven to `Succeeded`.
    Succeeded,
    /// Driven to `Failed`.
    Failed,
    /// Parked at `NeedsConfirmation` — deliberately not retried.
    NeedsConfirmation,
}

impl OpRow {
    /// Parses the store's state text; `None` for a spelling this round does not
    /// know, so an unrecognized row is skipped rather than miscounted.
    fn parse(state: &str) -> Option<Self> {
        match state {
            "Pending" | "InFlight" => Some(Self::Runnable),
            "Succeeded" => Some(Self::Succeeded),
            "Failed" => Some(Self::Failed),
            "NeedsConfirmation" => Some(Self::NeedsConfirmation),
            _ => None,
        }
    }

    /// Whether this is a terminal state a drain could have driven the op to.
    fn settled(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::NeedsConfirmation
        )
    }

    /// Whether the op's send succeeded.
    fn succeeded(self) -> bool {
        matches!(self, Self::Succeeded)
    }

    /// The failure's reason where one is recoverable: only the parked state
    /// says more than "failed" (the store records no failure text).
    fn detail(self) -> Option<&'static str> {
        match self {
            Self::NeedsConfirmation => Some("needs_confirmation"),
            _ => None,
        }
    }
}

/// The account's outbox rows — resource key and state — read through the host
/// seam in id order, the same order the drain claims in.
///
/// A read that fails yields nothing: the round's per-op events and its depth
/// event are skipped, never guessed at, while the sync report and the drain's
/// own count still carry the round. The engine's store just ran a sync for this
/// round, so a reader fault here is a backend emergency the caller owns.
async fn outbox(engine: &Engine, account: &AccountId) -> Vec<(String, OpRow)> {
    let account = account.as_str().to_owned();
    engine
        .host_store()
        .read(move |conn| read_outbox(conn, &account))
        .await
        .unwrap_or_default()
}

/// The outbox scan behind [`outbox`]: one statement over the engine's
/// `pending_op` table, no lock held past it.
fn read_outbox(conn: &Connection, account: &str) -> Result<Vec<(String, OpRow)>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT resource_key, state FROM pending_op WHERE account = ?1 ORDER BY id")
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?;
    let mut outbox = Vec::new();
    for row in rows {
        let (resource, state) = row.map_err(|err| err.to_string())?;
        if let Some(state) = OpRow::parse(&state) {
            outbox.push((resource, state));
        }
    }
    Ok(outbox)
}

/// The ops the drain between the two snapshots drove to an outcome: rows that
/// were runnable before and hold a terminal state after. A resource key is
/// unique per op, so matching keys is matching ops.
fn settled_between(before: &[(String, OpRow)], after: &[(String, OpRow)]) -> Vec<(String, OpRow)> {
    after
        .iter()
        .filter(|(resource, state)| {
            state.settled()
                && before
                    .iter()
                    .any(|(was, was_state)| was == resource && *was_state == OpRow::Runnable)
        })
        .cloned()
        .collect()
}

/// How many ops the outbox still holds pending — runnable, or in flight — as
/// the one number a badge shows.
fn depth(after: &[(String, OpRow)]) -> i64 {
    let pending = after
        .iter()
        .filter(|(_, state)| *state == OpRow::Runnable)
        .count();
    i64::try_from(pending).unwrap_or(i64::MAX)
}

/// The terminal status, read off the sync report alone.
///
/// `Idle` when every scope applied. `RateLimited` — with the throttle's
/// retry-after seconds — when the first failure the report holds is a provider
/// throttle; the engine classifies per failure, so the round can too. `Error`
/// otherwise, with no detail: the engine codes failures by class, not number,
/// and the class plus the message already ride the report's own `SyncError`,
/// which is where a caller that needs them reads them.
fn terminal(report: &MailSyncReport) -> (AccountState, Option<i64>) {
    if report.is_ok() {
        return (AccountState::Idle, None);
    }
    match report.first_error() {
        Some(SyncError::Provider(err)) if err.class() == FailureClass::RateLimited => (
            AccountState::RateLimited,
            err.retry_after()
                .and_then(|after| i64::try_from(after.seconds()).ok()),
        ),
        _ => (AccountState::Error, None),
    }
}

#[cfg(test)]
#[path = "round_tests.rs"]
mod round_tests;
