//! The round orchestration driven end to end over a real in-memory `Engine`: a
//! fake per-folder `Provider` (the threads tests' established discipline) streams
//! real chunks through `sync_mail`, a seeded outbox op exercises `drain_mail_ops`,
//! and every assertion reads the event stream a `CollectingSink` heard — by
//! content, not by count.

use std::collections::BTreeSet;

use engine_api::{AccountId, Engine, StreamTuning};
use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Mailbox, MailboxRole, Message},
    membership::Memberships,
    sync::{SyncScope, SyncState, SyncUpdate},
    time::Duration,
    write::{IdempotencyKey, PendingOp, ResourceKey, SubmitPayload},
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, Provider, ProviderError,
    ProviderResult, ScopeSync, SubmissionReceipt,
};
use engine_store::Store as _;
use engine_sync::OutboxIntent;

use super::*;
use crate::events::{AccountState, CollectingSink, EngineEvent};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// Runs one round with the tests' standing tuning, so each scenario reads as
/// its inputs and its expectations.
async fn round<P: Provider>(
    engine: &Engine,
    providers: &[P],
    sink: &CollectingSink,
) -> RoundReport {
    run_account_round(engine, providers, &account(), StreamTuning::new(0, 0), sink).await
}

fn mailbox(id: &str, name: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), name);
    mailbox.role = role;
    mailbox
}

fn message(id: &str, mailbox: &str) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.received_at = Some("2026-01-01T09:00:00Z".parse().unwrap());
    message
}

/// How the fake's first (cursor-less) `stream_email` answers.
enum FirstFetch {
    /// Stream the folder's messages as one reconcile chunk.
    Messages,
    /// Yield no chunks at all — a provider with nothing to say.
    Nothing,
    /// Fail the pass with this fault.
    Fail(Fault),
}

/// A provider fault, as the engine's error taxonomy classifies them.
enum Fault {
    /// A permanent failure (the folder never succeeds unchanged).
    Permanent(&'static str),
    /// A throttle, with the retry-after seconds the provider named.
    RateLimited(&'static str, u64),
}

impl Fault {
    fn error(&self) -> ProviderError {
        match self {
            Fault::Permanent(detail) => ProviderError::permanent(*detail),
            Fault::RateLimited(detail, after) => ProviderError::rate_limited(
                *detail,
                Some(Duration::from_parts(0, 0, 0, 0, *after, 0).unwrap()),
            ),
        }
    }
}

/// A minimal in-memory per-folder provider — IMAP-shaped (one provider per
/// mailbox), so two of them cover two folders without contending for one scope.
/// What it does on each verb is fixed by the fields, so each test names the
/// scenario it builds.
struct RoundMail {
    /// The folder this provider serves; its role drives the Inbox-first order.
    folder: Mailbox,
    /// The folder's messages, snapshotted on the first sync.
    messages: Vec<Message>,
    /// How the first fetch answers; a later (delta) fetch always answers with
    /// one empty chunk, so a second round still emits a heartbeat per chunk.
    first: FirstFetch,
    /// Message ids whose submits park ambiguous (the post-`DATA` loss) instead
    /// of sending — the drain test's failing op.
    parks: Vec<&'static str>,
}

impl RoundMail {
    fn quiet(folder: &str) -> Self {
        Self {
            folder: mailbox(folder, folder, None),
            messages: Vec::new(),
            first: FirstFetch::Nothing,
            parks: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for RoundMail {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailboxList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: self.folder.id.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("mbox-2"),
            ));
        }
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(
                vec![self.folder.clone()],
                BTreeSet::from([self.folder.id.key().clone()]),
            ),
            SyncState::new("mbox-1"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: engine_core::sync::SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let items: Vec<Result<EmailChunk, ProviderError>> = match (&self.first, cursor.is_some()) {
            (_, true) => vec![Ok(EmailChunk::additive(
                Vec::new(),
                Vec::new(),
                None,
                SyncState::new("email-2"),
            ))],
            (FirstFetch::Fail(fault), false) => vec![Err(fault.error())],
            (FirstFetch::Nothing, false) => Vec::new(),
            (FirstFetch::Messages, false) => {
                let present: Vec<_> = self.messages.iter().map(|m| m.id.key().clone()).collect();
                vec![Ok(EmailChunk::reconcile_last(
                    self.messages.clone(),
                    present,
                    Some(self.messages.len()),
                    SyncState::new("email-1"),
                ))]
            }
        };
        Box::pin(futures_util::stream::iter(items))
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.parks.contains(&draft.message_id.as_str()) {
            return Err(ProviderError::needs_confirmation(
                "post-DATA acknowledgement lost",
            ));
        }
        Ok(SubmissionReceipt::filed(
            ProviderKey::new("sent-1").unwrap(),
            draft.message_id.clone(),
        ))
    }
}

/// One `AccountStatus` event, as a one-line expectation.
fn status(state: AccountState, detail: Option<i64>) -> EngineEvent {
    EngineEvent::AccountStatus {
        account: "acct-1".to_owned(),
        state,
        detail,
    }
}

/// One `Commit` event, as a one-line expectation.
fn commit(folder: &str, upserted: &[&str], fetched: usize, total: Option<usize>) -> EngineEvent {
    EngineEvent::Commit {
        account: "acct-1".to_owned(),
        folder: folder.to_owned(),
        upserted: upserted.iter().map(|key| (*key).to_owned()).collect(),
        removed: Vec::new(),
        fetched,
        total,
    }
}

/// The folder a `Commit` event names — for sorting concurrent folders' commits
/// into one deterministic order before comparing them.
fn named_folder(event: &EngineEvent) -> &str {
    match event {
        EngineEvent::Commit { folder, .. } => folder,
        other => panic!("not a commit: {other:?}"),
    }
}

/// The commit events, sorted by folder — the folders of a pass stream
/// concurrently, so the order they finish in is the scheduler's, not the
/// contract's.
fn commits_in_folder_order(sink: &CollectingSink) -> Vec<EngineEvent> {
    let mut commits: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| matches!(event, EngineEvent::Commit { .. }))
        .collect();
    commits.sort_by_key(|event| named_folder(event).to_owned());
    commits
}

/// Seeds one unstarted submit op — the state a crash between the enqueue and
/// claim halves of the inline submit driver leaves behind, built exactly the way
/// the facade's own drain tests build it. No facade write can leave one: every
/// write resolves its op in the same call, so the only source of a runnable
/// backlog is a crash, and this is its reconstruction.
async fn seed_unstarted_submit(engine: &Engine, idempotency: &str, message_id: &str) {
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Drain me",
        "the body",
    );
    engine
        .host_store()
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(idempotency).unwrap(),
                ResourceKey::new(format!("draft:{message_id}")).unwrap(),
                serde_json::to_value(OutboxIntent::SubmitMail {
                    payload: SubmitPayload::Draft(draft),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_clean_round_reports_each_folders_commit_and_ends_idle() {
    let engine = Engine::open_in_memory().unwrap();
    let inbox = RoundMail {
        folder: mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
        messages: vec![message("t1", "a"), message("t2", "a")],
        first: FirstFetch::Messages,
        parks: Vec::new(),
    };
    let archive = RoundMail {
        folder: mailbox("b", "Archive", None),
        messages: vec![message("t3", "b")],
        first: FirstFetch::Messages,
        parks: Vec::new(),
    };
    let sink = CollectingSink::default();

    let report = round(&engine, &[inbox, archive], &sink).await;

    // The bracket: syncing first, idle last.
    let events = sink.events();
    assert_eq!(events.first(), Some(&status(AccountState::Syncing, None)));
    assert_eq!(events.last(), Some(&status(AccountState::Idle, None)));

    // One commit per folder, each carrying the folder its scope names, the
    // provider keys it upserted, and the running fetched/total the pass knew.
    assert_eq!(
        commits_in_folder_order(&sink),
        vec![
            commit("a", &["t1", "t2"], 2, Some(2)),
            commit("b", &["t3"], 1, Some(1)),
        ]
    );

    // An empty outbox is not news: nothing drained, so no outbox events fire.
    assert!(events.iter().all(|event| !matches!(
        event,
        EngineEvent::OutboxChanged { .. } | EngineEvent::SendResult { .. }
    )));

    assert!(report.clean, "every scope applied");
    assert_eq!(report.drained, 0);
    assert_eq!(report.sync.upserted(), 3);
}

#[tokio::test]
async fn a_delta_round_with_an_empty_chunk_still_heartbeats_one_commit_per_chunk() {
    // The adapter's per-chunk policy, pinned: a delta chunk that changes
    // nothing still committed, so it still reports — the running
    // fetched/total is the "still here" heartbeat a progress surface needs.
    let engine = Engine::open_in_memory().unwrap();
    let providers = [RoundMail {
        folder: mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
        messages: vec![message("t1", "a")],
        first: FirstFetch::Messages,
        parks: Vec::new(),
    }];
    let sink = CollectingSink::default();
    round(&engine, &providers, &sink).await;
    sink.clear();

    let report = round(&engine, &providers, &sink).await;

    assert_eq!(
        commits_in_folder_order(&sink),
        vec![commit("a", &[], 0, None)],
        "the empty delta chunk reports as a heartbeat, not silence"
    );
    let events = sink.events();
    assert_eq!(events.first(), Some(&status(AccountState::Syncing, None)));
    assert_eq!(events.last(), Some(&status(AccountState::Idle, None)));
    assert!(report.clean);
}

#[tokio::test]
async fn a_folder_failure_ends_the_round_in_error_without_clean() {
    let engine = Engine::open_in_memory().unwrap();
    let good = RoundMail {
        folder: mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
        messages: vec![message("t1", "a")],
        first: FirstFetch::Messages,
        parks: Vec::new(),
    };
    let bad = RoundMail {
        first: FirstFetch::Fail(Fault::Permanent("folder b exploded")),
        ..RoundMail::quiet("b")
    };
    let sink = CollectingSink::default();

    let report = round(&engine, &[good, bad], &sink).await;

    // The failing folder's provider error is not a number the engine codes, so
    // the error status carries no detail; the failure itself stays in the
    // report, per scope.
    let events = sink.events();
    assert_eq!(events.first(), Some(&status(AccountState::Syncing, None)));
    assert_eq!(
        events.last(),
        Some(&status(AccountState::Error, None)),
        "any failed scope fails the round"
    );
    assert_eq!(
        commits_in_folder_order(&sink),
        vec![commit("a", &["t1"], 1, Some(1))],
        "the healthy folder still committed"
    );

    assert!(!report.clean);
    assert_eq!(report.sync.folders.len(), 2);
    assert_eq!(report.sync.folders_synced(), 1);
}

#[tokio::test]
async fn an_empty_round_emits_only_the_status_bracket() {
    // A provider that streams nothing: no chunk, so no commit — the adapter
    // reports commits, it never fabricates them.
    let engine = Engine::open_in_memory().unwrap();
    let providers = [RoundMail::quiet("q")];
    let sink = CollectingSink::default();

    let report = round(&engine, &providers, &sink).await;

    assert_eq!(
        sink.events(),
        vec![
            status(AccountState::Syncing, None),
            status(AccountState::Idle, None),
        ]
    );
    assert!(report.clean, "nothing was asked and nothing failed");
    assert_eq!(report.drained, 0);
}

#[tokio::test]
async fn a_rate_limited_scope_failure_carries_the_retry_after_seconds() {
    let engine = Engine::open_in_memory().unwrap();
    let providers = [RoundMail {
        first: FirstFetch::Fail(Fault::RateLimited("slow down", 37)),
        ..RoundMail::quiet("a")
    }];
    let sink = CollectingSink::default();

    let report = round(&engine, &providers, &sink).await;

    let events = sink.events();
    assert_eq!(
        events.last(),
        Some(&status(AccountState::RateLimited, Some(37))),
        "the throttle's retry-after rides the terminal status as seconds"
    );
    assert!(!report.clean);
}

#[tokio::test]
async fn the_drain_step_reports_each_settled_op_and_the_outbox_depth() {
    let engine = Engine::open_in_memory().unwrap();
    let providers = [RoundMail {
        parks: vec!["park-1@test.local"],
        ..RoundMail::quiet("a")
    }];
    seed_unstarted_submit(&engine, "round:submit:ok", "ok-1@test.local").await;
    seed_unstarted_submit(&engine, "round:submit:park", "park-1@test.local").await;
    let sink = CollectingSink::default();

    let report = round(&engine, &providers, &sink).await;

    // One send result per op the drain drove to an outcome, in claim order:
    // the send that succeeded, and the one whose post-`DATA` acknowledgement
    // was lost — parked for confirmation, which is the detail it carries.
    let sends: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| matches!(event, EngineEvent::SendResult { .. }))
        .collect();
    assert_eq!(
        sends,
        vec![
            EngineEvent::SendResult {
                account: "acct-1".to_owned(),
                message_id: "draft:ok-1@test.local".to_owned(),
                success: true,
                detail: None,
            },
            EngineEvent::SendResult {
                account: "acct-1".to_owned(),
                message_id: "draft:park-1@test.local".to_owned(),
                success: false,
                detail: Some("needs_confirmation".to_owned()),
            },
        ]
    );

    // Both ops left the outbox, so the depth after the drain is zero —
    // reported once, because something actually drained.
    assert!(sink.events().contains(&EngineEvent::OutboxChanged {
        account: "acct-1".to_owned(),
        pending: 0,
    }));
    assert_eq!(report.drained, 2, "both ops were driven to an outcome");
    assert!(report.clean, "the sync itself was clean");
    let events = sink.events();
    assert_eq!(events.last(), Some(&status(AccountState::Idle, None)));
}
