//! The warm tests: the sequential default's order and failure isolation, and the
//! warm pass over a really-synced store (seeded through `sync_mail`, exactly the
//! `threads_tests` discipline). The IMAP pipeline's own tests — the pure
//! UID-set/fan-out halves and the in-process TLS server — are the
//! `warm_imap_tests` sibling.

use engine_api::{AccountId, Engine, IgnoreCommits, StreamTuning};
use engine_core::{
    error::FailureClass,
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{Mailbox, MailboxRole, Message},
    membership::Memberships,
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EmailChunk, EmailStream, Provider, ProviderResult,
};
use engine_store::{MessageBodyStore, MessageSourceCache};

use super::*;

pub(super) fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// One message with provider key `key`, filed once — the minimal object a warm
/// batch addresses.
fn message(key: &str) -> Message {
    Message::new(
        MessageId::try_from(key).unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

/// The owned messages for a batch of `keys`; pair them with [`warm_pairs`].
pub(super) fn messages_for(keys: &[&str]) -> Vec<Message> {
    keys.iter().map(|key| message(key)).collect()
}

/// The `(&ProviderKey, &Message)` pairs the batch trait takes, borrowed from the
/// caller's owned messages.
pub(super) fn warm_pairs(messages: &[Message]) -> Vec<(&ProviderKey, &Message)> {
    messages.iter().map(|m| (m.id.key(), m)).collect()
}

/// A provider whose only verb is the single-message source fetch, recording every
/// key it is asked for — the call-order witness the sequential tests assert on.
struct CountingProvider {
    caps: Capabilities,
    calls: std::sync::Mutex<Vec<String>>,
    fail: Vec<String>,
}

impl CountingProvider {
    fn new(fail: &[&str]) -> Self {
        Self {
            caps: Capabilities::none().with_mail().with_message_source(),
            calls: std::sync::Mutex::new(Vec::new()),
            fail: fail.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Provider for CountingProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(self.caps)
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        let key = message.id.key().as_str().to_owned();
        self.calls.lock().unwrap().push(key.clone());
        if self.fail.contains(&key) {
            return Err(engine_provider::ProviderError::retryable(
                "scripted failure",
            ));
        }
        Ok(RawMime::new(
            format!("Content-Type: text/plain\r\n\r\nbody of {key}\r\n").into_bytes(),
        ))
    }
}

#[tokio::test]
async fn sequential_sources_fetches_each_item_once_in_batch_order() {
    let provider = CountingProvider::new(&[]);
    let owned = messages_for(&["k1", "k2", "k3"]);
    let batch = warm_pairs(&owned);

    let out = sequential_sources(&provider, &account(), &batch).await;

    // One provider call per item, in the batch's own order — N items, N calls.
    assert_eq!(provider.calls(), vec!["k1", "k2", "k3"]);
    // The returned keys align with the input order, each carrying its own body.
    assert_eq!(
        out.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        vec!["k1", "k2", "k3"]
    );
    for (key, result) in &out {
        let raw = result.as_ref().unwrap();
        assert!(
            String::from_utf8_lossy(raw.as_bytes()).contains(&format!("body of {}", key.as_str())),
            "key {} carries its own body",
            key.as_str()
        );
    }
}

#[tokio::test]
async fn sequential_sources_a_failed_item_does_not_block_the_rest() {
    let provider = CountingProvider::new(&["k2"]);
    let owned = messages_for(&["k1", "k2", "k3"]);
    let batch = warm_pairs(&owned);

    let out = sequential_sources(&provider, &account(), &batch).await;

    // The middle item failed; its neighbors still fetched — three calls, one Err.
    assert_eq!(provider.calls().len(), 3, "a failure stops nothing");
    assert!(out[0].1.is_ok() && out[2].1.is_ok());
    assert_eq!(
        out[1].1.as_ref().unwrap_err().class(),
        FailureClass::Retryable
    );
}

/// A `BatchSourceFetch` scripted per key, recording every batch it received — the
/// witness the warm tests assert call counts and batch shapes against.
struct ScriptedBatch {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    fail: Vec<String>,
}

impl ScriptedBatch {
    fn new(fail: &[&str]) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            fail: fail.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    /// The keys of each call, in call order.
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl BatchSourceFetch for ScriptedBatch {
    async fn fetch_message_sources(
        &self,
        _account: &AccountId,
        batch: &[(&ProviderKey, &Message)],
    ) -> Vec<(ProviderKey, Result<RawMime, engine_provider::ProviderError>)> {
        self.calls.lock().unwrap().push(
            batch
                .iter()
                .map(|(key, _)| key.as_str().to_owned())
                .collect(),
        );
        batch
            .iter()
            .map(|(key, _)| {
                let outcome = if self.fail.iter().any(|fail| fail == key.as_str()) {
                    Err(engine_provider::ProviderError::retryable(
                        "scripted failure",
                    ))
                } else {
                    Ok(RawMime::new(
                        format!(
                            "Content-Type: text/plain\r\n\r\nwarm body {}\r\n",
                            key.as_str()
                        )
                        .into_bytes(),
                    ))
                };
                ((*key).clone(), outcome)
            })
            .collect()
    }
}

/// The minimal JMAP-shaped snapshot provider from `threads_tests`: mailboxes plus
/// messages on the first pass, an empty delta after — what the warm tests need from
/// it is the *real* sync semantics that land metadata rows and nothing else.
struct SeededMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
}

impl SeededMail {
    fn new(keys: &[&str]) -> Self {
        let mut inbox = Mailbox::new(MailboxId::try_from("a").unwrap(), "Inbox");
        inbox.role = Some(MailboxRole::Inbox);
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes: vec![inbox],
            messages: keys.iter().map(|key| message(key)).collect(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for SeededMail {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(self.caps)
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<engine_provider::ScopeSync<Mailbox>> {
        if cursor.is_some() {
            return Ok(engine_provider::ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("mbox-2"),
            ));
        }
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(engine_provider::ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            SyncState::new("mbox-1"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let chunk = if cursor.is_some() {
            EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("email-2"))
        } else {
            let present: Vec<_> = self.messages.iter().map(|m| m.id.key().clone()).collect();
            EmailChunk::reconcile_last(
                self.messages.clone(),
                present,
                Some(self.messages.len()),
                SyncState::new("email-1"),
            )
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

/// The engine the warm tests run against: synced once, so the store holds real
/// derived rows landed by the engine's own sync — not rows written behind its back.
async fn synced_engine(keys: &[&str]) -> Engine {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SeededMail::new(keys);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await;
    engine
}

#[tokio::test]
async fn warm_drains_the_missing_list_and_caches_both_halves() {
    let engine = synced_engine(&["m1", "m2", "m3"]).await;

    // The documented post-sync fact: a sync lands metadata only, so every message
    // starts with neither cache half.
    let missing = engine
        .mail_missing_body(core::slice::from_ref(&account()), 50)
        .await
        .unwrap();
    assert_eq!(missing.len(), 3, "a fresh sync caches no bodies");

    let batch = ScriptedBatch::new(&[]);
    let report = warm_mail_bodies(&engine, &batch, &account(), 50)
        .await
        .unwrap();

    assert_eq!(
        (report.fetched, report.failed, report.remaining_hint),
        (3, 0, 0)
    );
    // One batch call carrying exactly the missing keys — not three per-item calls.
    let calls = batch.calls();
    assert_eq!(
        calls.len(),
        1,
        "the work list goes to the batch fetcher once"
    );
    let mut served = calls[0].clone();
    served.sort();
    assert_eq!(served, vec!["m1", "m2", "m3"]);

    // The engine's own work list is empty afterwards, and both cache halves hold
    // content for every message.
    assert!(
        engine
            .mail_missing_body(core::slice::from_ref(&account()), 50)
            .await
            .unwrap()
            .is_empty()
    );
    for key in ["m1", "m2", "m3"] {
        let key = ProviderKey::new(key).unwrap();
        let body = engine
            .host_store()
            .get_message_body(&account(), &key)
            .await
            .unwrap()
            .expect("body text cached");
        assert!(
            body.plain().unwrap().contains(&format!("warm body {key}")),
            "extracted text cached for {key}"
        );
        assert!(
            engine
                .host_store()
                .get_message_source(&account(), &key)
                .await
                .unwrap()
                .is_some(),
            "raw source cached for {key}"
        );
    }
}

#[tokio::test]
async fn warm_counts_a_failed_fetch_and_still_warms_the_rest() {
    let engine = synced_engine(&["m1", "m2", "m3"]).await;
    let batch = ScriptedBatch::new(&["m2"]);

    let report = warm_mail_bodies(&engine, &batch, &account(), 50)
        .await
        .unwrap();

    assert_eq!(
        (report.fetched, report.failed, report.remaining_hint),
        (2, 1, 1)
    );
    // Exactly the failed key stays on the work list; the warmed two left it.
    let still_missing = engine
        .mail_missing_body(core::slice::from_ref(&account()), 50)
        .await
        .unwrap();
    assert_eq!(still_missing.len(), 1);
    assert_eq!(still_missing[0].mail.key.as_str(), "m2");
}

#[tokio::test]
async fn warm_respects_the_budget_and_reports_what_is_left() {
    let engine = synced_engine(&["m1", "m2", "m3"]).await;
    let batch = ScriptedBatch::new(&[]);

    // A zero budget warms nothing — and its hint still names the whole backlog,
    // because the hint is the engine's answer, not the pass's.
    let none = warm_mail_bodies(&engine, &batch, &account(), 0)
        .await
        .unwrap();
    assert_eq!((none.fetched, none.failed, none.remaining_hint), (0, 0, 3));
    assert!(batch.calls().is_empty(), "no batch call for an empty pass");

    let report = warm_mail_bodies(&engine, &batch, &account(), 1)
        .await
        .unwrap();

    assert_eq!(
        report.fetched, 1,
        "one pass warms at most `budget` messages"
    );
    assert_eq!(
        report.remaining_hint, 2,
        "the hint names the rest, not the pass"
    );
    let calls = batch.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].len(),
        1,
        "the batch fetcher saw only the budgeted item"
    );
}
