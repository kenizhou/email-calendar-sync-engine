//! The threads read over a real synced store: an in-memory `Engine` seeded through
//! `sync_mail` with a fake `Provider` (the repo's established discipline — the
//! engine derives the thread ids, the store lands the rows, and the read is
//! asserted against both), covering the per-thread facts, the keyset pages, and
//! the label filter.

use engine_api::{AccountId, Engine, IgnoreCommits, StreamTuning};
use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader},
    mail::{EmailAddress, Keyword, Mailbox, MailboxRole, Message, SystemKeyword},
    membership::Memberships,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EmailChunk, EmailStream, Provider, ProviderResult, ScopeSync,
};

use super::*;

/// The epoch seconds of a UTC wall clock, so the tests assert real instants rather
/// than hand-arithmetic magic numbers. Days-from-civil (Hinnant): exact for the
/// proleptic Gregorian calendar, which is what `received_at` strings are.
fn epoch(date: &str) -> i64 {
    let year: i64 = date[0..4].parse().unwrap();
    let month: i64 = date[5..7].parse().unwrap();
    let day: i64 = date[8..10].parse().unwrap();
    let hour: i64 = date[11..13].parse().unwrap();
    let minute: i64 = date[14..16].parse().unwrap();
    let second: i64 = date[17..19].parse().unwrap();
    let (year, month) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month - 3) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days * 86_400 + hour * 3_600 + minute * 60 + second
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn mailbox(id: &str, name: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), name);
    mailbox.role = role;
    mailbox
}

/// A conversation member's threading facts: own `Message-ID`, the ids it
/// references, and its filing.
fn member(id: &str, mailbox: &str, own: &str, references: &[&str]) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.envelope.message_id = vec![MessageIdHeader::new(own).unwrap()];
    message.envelope.references = references
        .iter()
        .map(|value| MessageIdHeader::new(*value).unwrap())
        .collect();
    message
}

/// A member's display facts: the delivery date the read orders by, and the header
/// a summary shows.
fn header(message: &mut Message, received: &str, subject: &str, preview: &str, from: EmailAddress) {
    message.received_at = Some(received.parse().unwrap());
    message.envelope.subject = Some(subject.to_owned());
    message.preview = Some(preview.to_owned());
    message.envelope.from = vec![from];
}

/// A minimal in-memory JMAP-shaped provider: a full snapshot on the first sync of
/// a scope, an empty delta once a cursor exists — the same shape the facade's own
/// tests drive, because what these tests need from it is real sync semantics:
/// derive, index, thread, and land the rows the read then aggregates.
struct FakeMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    cursor: SyncState,
}

impl FakeMail {
    /// Two messages joined by `References` into one conversation plus a standalone
    /// one, spread over two mailboxes: the facts every assertion below reads.
    ///
    /// - `t1` — the conversation's root: seen, from Alice, delivered 2026-01-01.
    /// - `t2` — its reply: **unseen**, `$flagged`, with an attachment, from Bob, delivered
    ///   2026-01-02 — so the thread's newest member is the one carrying every mutable fact.
    /// - `t3` — standalone, seen, from Carol, delivered 2026-01-03, filed in the other mailbox —
    ///   the newest thread of all and the label filter's target.
    fn seeded() -> Self {
        let mut root = member("t1", "a", "a@h", &[]);
        header(
            &mut root,
            "2026-01-01T09:00:00Z",
            "Thread root",
            "root preview",
            EmailAddress::named("Alice", "alice@h"),
        );
        root.keywords.insert(Keyword::system(SystemKeyword::Seen));
        let mut reply = member("t2", "a", "b@h", &["a@h"]);
        header(
            &mut reply,
            "2026-01-02T10:00:00Z",
            "Re: Thread root",
            "reply preview",
            EmailAddress::named("Bob", "bob@h"),
        );
        reply
            .keywords
            .insert(Keyword::system(SystemKeyword::Flagged));
        reply.has_attachment = true;
        let mut solo = member("t3", "b", "c@h", &[]);
        header(
            &mut solo,
            "2026-01-03T08:00:00Z",
            "Standalone",
            "solo preview",
            EmailAddress::named("Carol", "carol@h"),
        );
        solo.keywords.insert(Keyword::system(SystemKeyword::Seen));
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes: vec![
                mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
                mailbox("b", "Archive", None),
            ],
            messages: vec![root, reply, solo],
            cursor: SyncState::new("email-1"),
        }
    }
}

#[async_trait::async_trait]
impl Provider for FakeMail {
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
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("mbox-2"),
            ));
        }
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
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
                self.cursor.clone(),
            )
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

/// The engine the assertions read through: synced once, so the store holds real
/// derived, threaded, indexed rows — not rows a test wrote behind the engine's back.
async fn synced_engine() -> Engine {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeMail::seeded();
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

/// The thread id the engine itself derived for the message with provider key `key`,
/// read back through the facade — so the summaries' ids are asserted against the
/// engine's answer, not against a test's assumption of it.
async fn derived_thread_id(engine: &Engine, key: &str) -> String {
    engine
        .messages(&account())
        .await
        .unwrap()
        .iter()
        .find(|message| message.id.key().as_str() == key)
        .unwrap()
        .thread_id()
        .unwrap()
        .as_str()
        .to_owned()
}

#[tokio::test]
async fn one_summary_per_thread_folds_members_and_shows_the_newest_one() {
    let engine = synced_engine().await;
    let page = engine.threads(&account(), ThreadsOptions::default()).await;
    let page = page.unwrap();

    // The engine derived one conversation out of t1+t2 and left t3 standalone;
    // newest first: t3's 2026-01-03 beats the conversation's 2026-01-02 reply.
    let root_thread = derived_thread_id(&engine, "t1").await;
    let reply_thread = derived_thread_id(&engine, "t2").await;
    let solo_thread = derived_thread_id(&engine, "t3").await;
    assert_eq!(root_thread, reply_thread, "t2 joined t1's conversation");
    assert_eq!(
        page.threads
            .iter()
            .map(|t| t.thread_id.as_str())
            .collect::<Vec<_>>(),
        vec![solo_thread.as_str(), root_thread.as_str()],
        "one row per thread, newest first"
    );

    // The conversation: every mutable fact folded over both members, every header
    // fact from the newest one (the reply).
    let conversation = &page.threads[1];
    assert_eq!(conversation.total, 2);
    assert_eq!(
        conversation.unread, 1,
        "the seen root is not unread; the reply is"
    );
    assert!(conversation.starred, "the reply carries $flagged");
    assert!(
        conversation.has_attachments,
        "the reply carries an attachment"
    );
    assert_eq!(
        conversation.labels,
        vec![MailboxId::try_from("a").unwrap()],
        "both members are filed in the inbox"
    );
    assert_eq!(conversation.subject.as_deref(), Some("Re: Thread root"));
    assert_eq!(conversation.snippet.as_deref(), Some("reply preview"));
    assert_eq!(conversation.from_name.as_deref(), Some("Bob"));
    assert_eq!(conversation.from_address.as_deref(), Some("bob@h"));
    assert_eq!(conversation.last_date, Some(epoch("2026-01-02T10:00:00Z")));

    // The standalone conversation: no folding to do, its own header.
    let solo = &page.threads[0];
    assert_eq!(solo.total, 1);
    assert_eq!(solo.unread, 0, "seen");
    assert!(!solo.starred);
    assert!(!solo.has_attachments);
    assert_eq!(solo.labels, vec![MailboxId::try_from("b").unwrap()]);
    assert_eq!(solo.subject.as_deref(), Some("Standalone"));
    assert_eq!(solo.last_date, Some(epoch("2026-01-03T08:00:00Z")));

    // Two of fifty rows is a short page: the end of the list by construction.
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn keyset_pages_continue_the_list_and_stop_at_the_end() {
    let engine = synced_engine().await;
    let solo_thread = derived_thread_id(&engine, "t3").await;
    let root_thread = derived_thread_id(&engine, "t1").await;

    // Page one (size one): the newest thread, full, so it carries a cursor that
    // names exactly where it stopped.
    let first = engine
        .threads(
            &account(),
            ThreadsOptions {
                limit: 1,
                ..ThreadsOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.threads.len(), 1);
    assert_eq!(first.threads[0].thread_id, solo_thread);
    let cursor = first.next_cursor.expect("a full page carries a cursor");
    assert_eq!(cursor.date, epoch("2026-01-03T08:00:00Z"));
    assert_eq!(cursor.id, solo_thread);

    // Page two: the conversation — no row repeated, none skipped — full again, so
    // it too carries a cursor.
    let second = engine
        .threads(
            &account(),
            ThreadsOptions {
                limit: 1,
                cursor: Some(cursor),
                ..ThreadsOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(second.threads.len(), 1);
    assert_eq!(second.threads[0].thread_id, root_thread);
    assert!(
        second.next_cursor.is_some(),
        "a full page carries a cursor, even the last full one"
    );

    // Page three: empty, and short, so no cursor to follow.
    let third = engine
        .threads(
            &account(),
            ThreadsOptions {
                cursor: second.next_cursor,
                ..ThreadsOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(third.threads.is_empty());
    assert!(third.next_cursor.is_none());
}

#[tokio::test]
async fn a_label_filter_keeps_only_threads_with_a_member_in_that_mailbox() {
    let engine = synced_engine().await;
    let solo_thread = derived_thread_id(&engine, "t3").await;
    let root_thread = derived_thread_id(&engine, "t1").await;

    // The archive holds only the standalone message's thread; the inbox only the
    // conversation; a mailbox nothing is filed in matches nothing.
    let filtered = |mailbox: &str| ThreadsOptions {
        label: Some(MailboxId::try_from(mailbox).unwrap()),
        ..ThreadsOptions::default()
    };
    let archive = engine.threads(&account(), filtered("b")).await.unwrap();
    assert_eq!(
        archive
            .threads
            .iter()
            .map(|t| t.thread_id.as_str())
            .collect::<Vec<_>>(),
        vec![solo_thread.as_str()]
    );
    let inbox = engine.threads(&account(), filtered("a")).await.unwrap();
    assert_eq!(
        inbox
            .threads
            .iter()
            .map(|t| t.thread_id.as_str())
            .collect::<Vec<_>>(),
        vec![root_thread.as_str()]
    );
    let none = engine.threads(&account(), filtered("nope")).await.unwrap();
    assert!(none.threads.is_empty());
    assert!(none.next_cursor.is_none());
}

#[tokio::test]
async fn a_nonpositive_limit_is_rejected_rather_than_read_as_unlimited() {
    let engine = synced_engine().await;
    let err = engine
        .threads(
            &account(),
            ThreadsOptions {
                limit: 0,
                ..ThreadsOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("limit"),
        "the error names what was wrong: {err}"
    );
}
