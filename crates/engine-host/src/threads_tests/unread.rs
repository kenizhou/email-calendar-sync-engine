//! The unread aggregates: [`ThreadsRead::unread_counts_by_label`] and
//! [`ThreadsRead::unread_thread_total`] over the same fake-provider discipline
//! as the parent's page tests. The load-bearing assertion is the equivalence
//! pin — the aggregate must equal folding [`ThreadsRead::threads`] pages the
//! way a host that had no aggregate used to — because that fold is exactly the
//! semantics the page read established: a member is unread when it carries
//! neither `$seen` nor `$draft`, and a thread's labels are every mailbox any
//! member is filed in.

use std::collections::HashMap;

use super::*;

/// The unread-facts fixture: five threads over three mailboxes whose read /
/// unread / draft mix exercises every rule the aggregates fold.
///
/// - **`X` — the cross-label conversation.** Three members joined by `References`: two seen (one
///   filed in the inbox `a`, one in the archive `b`) plus one **unseen** reply in the inbox — a
///   thread with one unread member among read ones, whose labels span both mailboxes, so it must
///   count once for `a` and once for `b`.
/// - **`Y` — the draft-only thread.** A lone `$draft` member in the drafts mailbox `c`: never
///   unread, never counted.
/// - **`Z` — the draft-with-unread thread.** A `$draft` member and its **unseen** reply, both in
///   `c`: the thread is unread through the reply alone, and counts once for `c`.
/// - **`W` — the all-read thread.** A lone seen member in the inbox: counted nowhere.
/// - **`V` — the standalone unread thread.** A lone unseen member in the archive: counts once for
///   `b`.
async fn unread_engine() -> Engine {
    let mut x_root = member("x1", "a", "x1@h", &[]);
    header(
        &mut x_root,
        "2026-03-01T09:00:00Z",
        "Cross-label root",
        "root preview",
        EmailAddress::named("Alice", "alice@h"),
    );
    x_root.keywords.insert(Keyword::system(SystemKeyword::Seen));
    let mut x_archived = member("x2", "b", "x2@h", &["x1@h"]);
    header(
        &mut x_archived,
        "2026-03-02T09:00:00Z",
        "Re: Cross-label root",
        "archived preview",
        EmailAddress::named("Bob", "bob@h"),
    );
    x_archived
        .keywords
        .insert(Keyword::system(SystemKeyword::Seen));
    let mut x_reply = member("x3", "a", "x3@h", &["x1@h"]);
    header(
        &mut x_reply,
        "2026-03-03T09:00:00Z",
        "Re: Cross-label root",
        "reply preview",
        EmailAddress::named("Carol", "carol@h"),
    );
    let mut y_draft = member("y1", "c", "y1@h", &[]);
    header(
        &mut y_draft,
        "2026-03-04T09:00:00Z",
        "Draft only",
        "draft preview",
        EmailAddress::named("Dave", "dave@h"),
    );
    y_draft
        .keywords
        .insert(Keyword::system(SystemKeyword::Draft));
    let mut z_draft = member("z1", "c", "z1@h", &[]);
    header(
        &mut z_draft,
        "2026-03-05T09:00:00Z",
        "Draft with reply",
        "draft preview",
        EmailAddress::named("Erin", "erin@h"),
    );
    z_draft
        .keywords
        .insert(Keyword::system(SystemKeyword::Draft));
    let mut z_reply = member("z2", "c", "z2@h", &["z1@h"]);
    header(
        &mut z_reply,
        "2026-03-06T09:00:00Z",
        "Re: Draft with reply",
        "reply preview",
        EmailAddress::named("Faythe", "faythe@h"),
    );
    let mut w_read = member("w1", "a", "w1@h", &[]);
    header(
        &mut w_read,
        "2026-03-07T09:00:00Z",
        "All read",
        "read preview",
        EmailAddress::named("Grace", "grace@h"),
    );
    w_read.keywords.insert(Keyword::system(SystemKeyword::Seen));
    let mut v_solo = member("v1", "b", "v1@h", &[]);
    header(
        &mut v_solo,
        "2026-03-08T09:00:00Z",
        "Standalone unread",
        "solo preview",
        EmailAddress::named("Heidi", "heidi@h"),
    );

    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeMail::fixture(
        vec![
            mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("b", "Archive", None),
            mailbox("c", "Drafts", Some(MailboxRole::Drafts)),
        ],
        vec![
            x_root, x_archived, x_reply, y_draft, z_draft, z_reply, w_read, v_solo,
        ],
    );
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

/// The fold a host without the aggregates used to run — and the shell's exact
/// old shape: walk every [`ThreadsRead::threads`] page of the account (page
/// size one, so the keyset cursor genuinely continues the walk) and count, per
/// label, the threads whose `unread` is positive; the account total counts
/// such threads once each.
async fn paged_fold(engine: &Engine) -> (HashMap<String, i64>, i64) {
    let mut labels: HashMap<String, i64> = HashMap::new();
    let mut total = 0i64;
    let mut cursor: Option<ThreadCursor> = None;
    loop {
        let page = engine
            .threads(
                &account(),
                ThreadsOptions {
                    label: None,
                    limit: 1,
                    cursor,
                },
            )
            .await
            .unwrap();
        for summary in &page.threads {
            if summary.unread > 0 {
                total += 1;
                for label in &summary.labels {
                    *labels.entry(label.as_str().to_owned()).or_insert(0) += 1;
                }
            }
        }
        cursor = match page.next_cursor {
            Some(next) => Some(next),
            None => break,
        };
    }
    (labels, total)
}

#[tokio::test]
async fn unread_counts_by_label_equals_the_paged_threads_fold() {
    let engine = unread_engine().await;
    let (folded_labels, folded_total) = paged_fold(&engine).await;

    let aggregate: HashMap<String, i64> = engine
        .unread_counts_by_label(&account())
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        aggregate, folded_labels,
        "the aggregate answers exactly what walking the pages did"
    );
    assert_eq!(
        engine.unread_thread_total(&account()).await.unwrap(),
        folded_total,
        "the total answers exactly what walking the pages did"
    );
}

#[tokio::test]
async fn unread_counts_pin_the_thread_level_semantics() {
    let engine = unread_engine().await;

    // Label order is the read's own (sorted), so the pin is an exact list: the
    // cross-label conversation counts once per label it carries, the standalone
    // unread thread once for its own, and the drafts mailbox only through the
    // thread whose unread member is real.
    let counts = engine.unread_counts_by_label(&account()).await.unwrap();
    assert_eq!(
        counts,
        vec![
            ("a".to_owned(), 1),
            ("b".to_owned(), 2),
            ("c".to_owned(), 1),
        ],
        "cross-label thread once per label; draft-only thread nowhere"
    );
    assert_eq!(
        engine.unread_thread_total(&account()).await.unwrap(),
        3,
        "threads, not labels: X, Z and V once each — a thread in two labels does not count twice"
    );
}

#[tokio::test]
async fn an_account_with_no_unread_answers_empty_and_zero() {
    let mut read = member("r1", "a", "r1@h", &[]);
    header(
        &mut read,
        "2026-03-01T09:00:00Z",
        "All read",
        "read preview",
        EmailAddress::named("Alice", "alice@h"),
    );
    read.keywords.insert(Keyword::system(SystemKeyword::Seen));
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeMail::fixture(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![read],
    );
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await;

    assert!(
        engine
            .unread_counts_by_label(&account())
            .await
            .unwrap()
            .is_empty(),
        "no unread thread means no label row, not an error"
    );
    assert_eq!(
        engine.unread_thread_total(&account()).await.unwrap(),
        0,
        "no unread thread means zero, not an error"
    );
}
