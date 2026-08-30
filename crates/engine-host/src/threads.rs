//! The conversation list: one summary row per thread, paged newest-first.
//!
//! A list pane does not show messages; it shows *conversations* — the count, the
//! unread/flagged state, the labels, and the newest member's header — and it shows
//! a page of them at a time. [`ThreadsRead::threads`] answers exactly that with one
//! grouped statement over the engine's `message` table, so a page costs the page
//! and not the account, and the same store rows the engine's own list read uses
//! stay the single source every surface agrees on.

use std::fmt::Write as _;

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId};
use rusqlite::{Connection, types::Value};

/// How a threads page is asked for: an optional label (mailbox) filter, a page
/// size, and the keyset cursor the previous page returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadsOptions {
    /// Only threads with at least one member filed in this mailbox. `None` is every
    /// thread of the account.
    pub label: Option<MailboxId>,
    /// The page size. Values below 1 are rejected by the read.
    pub limit: i64,
    /// Where the previous page stopped; `None` starts from the newest thread.
    pub cursor: Option<ThreadCursor>,
}

impl Default for ThreadsOptions {
    /// The list pane's standing page: no label filter, fifty rows, first page.
    fn default() -> Self {
        Self {
            label: None,
            limit: 50,
            cursor: None,
        }
    }
}

/// The position a page stopped at: the last thread's date and id.
///
/// The pair is the exact ordering key of the read — newest date first, ties broken
/// by thread id — so handing it back picks up with no row repeated and none
/// skipped, whatever sync moved underneath the page break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadCursor {
    /// The last page row's `last_date`: whole UTC seconds.
    pub date: i64,
    /// The last page row's thread id.
    pub id: String,
}

/// One conversation as a list row: everything a summary shows, nothing that needs
/// a member opened.
///
/// The conversational facts (`unread`, `starred`, `has_attachments`, `labels`) fold
/// over *all* members; the header facts (`subject`, `snippet`, `from_*`,
/// `last_date`) come from the **newest** member, so the row a user reads is the
/// message that made the conversation current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    /// The engine-derived thread id (the conversation's smallest owned
    /// `Message-ID`, or the provider's own id where it assigns one).
    pub thread_id: String,
    /// The newest member's subject, if any member carried one.
    pub subject: Option<String>,
    /// The newest member's list snippet (the `preview` column).
    pub snippet: Option<String>,
    /// Members that are unread — neither `$seen` nor `$draft` (RFC 8621 §2).
    pub unread: i64,
    /// Every member of the thread, dated or not.
    pub total: i64,
    /// The distinct mailboxes/labels any member is filed in.
    pub labels: Vec<MailboxId>,
    /// The newest member's date as whole UTC seconds; `None` when no member is
    /// dated (such a thread sorts last).
    pub last_date: Option<i64>,
    /// Whether any member carries `$flagged`.
    pub starred: bool,
    /// Whether any member has a non-inline attachment.
    pub has_attachments: bool,
    /// The newest member's first sender display name, if the header carried one.
    pub from_name: Option<String>,
    /// The newest member's first sender address, as the header spelled it.
    pub from_address: Option<String>,
}

/// One page of [`ThreadSummary`] rows plus the cursor that continues the list.
///
/// `next_cursor` is `Some` only when the page came back full — a short page is the
/// end of the list by construction — and is absent when even a full page's last
/// row has no date to key on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadsPage {
    /// The summaries, newest thread first.
    pub threads: Vec<ThreadSummary>,
    /// The cursor to pass as [`ThreadsOptions::cursor`] for the next page.
    pub next_cursor: Option<ThreadCursor>,
}

/// The conversation-list read over an engine's store.
///
/// Implemented for `engine_api::Engine` here rather than in the facade because the
/// aggregate is a *host* read model (P1): the facade stays the engine's own
/// object-level surface, and the orphan rule would keep a foreign trait off its
/// type anyway.
#[async_trait]
pub trait ThreadsRead {
    /// Reads one page of thread summaries for `account`.
    ///
    /// # Errors
    ///
    /// Returns the backend's message when the query fails, a stored label fails to
    /// parse as a mailbox id, or `opts.limit` is below 1.
    async fn threads(
        &self,
        account: &AccountId,
        opts: ThreadsOptions,
    ) -> Result<ThreadsPage, String>;
}

/// The system-keyword bits the aggregates fold over, as the `message.flags`
/// bitfield stores them (`engine_core::mail::MailFlags`, whose positions are
/// persisted and append-only).
const SEEN: i64 = 1; // 1 << 0
const FLAGGED: i64 = 2; // 1 << 1
const DRAFT: i64 = 4; // 1 << 2

#[async_trait]
impl ThreadsRead for engine_api::Engine {
    async fn threads(
        &self,
        account: &AccountId,
        opts: ThreadsOptions,
    ) -> Result<ThreadsPage, String> {
        if opts.limit < 1 {
            return Err(format!(
                "threads limit must be at least 1, got {}",
                opts.limit
            ));
        }
        // Owned copies so the closure crosses onto the blocking-pool reader.
        let account = account.as_str().to_owned();
        let label = opts
            .label
            .as_ref()
            .map(|mailbox| mailbox.as_str().to_owned());
        let cursor = opts.cursor;
        let limit = opts.limit;
        self.host_store()
            .read(move |conn| page(conn, &account, label.as_deref(), cursor.as_ref(), limit))
            .await
    }
}

/// One page of the conversation list, straight off the store's `message` rows.
///
/// The statement is one grouped scan of the account's messages — the same rows the
/// engine's own list read walks, so the two surfaces cannot disagree — with every
/// optional clause appended positionally:
///
/// - **Facts fold over members.** `unread`, `starred` and `has_attachments` are `SUM`s over the
///   group: a thread is starred when *any* member is, and a member is unread when it carries
///   neither `$seen` nor `$draft` (RFC 8621 §2 — a draft is never "unread"), which is `(flags &
///   SEEN|DRAFT) = 0`.
/// - **The header comes from the newest member.** SQLite documents that with exactly one
///   `min()`/`max()` aggregate in a query, bare columns take their value from the row that produced
///   it (sqlite.org/lang_select.html, "Bare columns in an aggregate query") — so the single
///   `MAX(date_utc)` makes `subject`/`preview`/`from_*` read from the newest member. Every other
///   aggregate here is a `COUNT`/`SUM`, which the rule does not count; two members sharing the
///   exact same nanosecond pick arbitrarily between them.
/// - **The page key is `(date, thread_id)`.** The stored `date_utc` is exact ISO-8601 text; the
///   cursor contract is whole seconds, so the ordering key is `unixepoch` of the member `MAX`
///   picked, truncated to an integer — one expression generating cursors and comparing them, so
///   pages stay consistent with each other even where sub-second order is folded away. Undated
///   threads key as -1, which every real date beats (no mail predates the unix epoch), so they sort
///   last and remain reachable from any cursor.
/// - **Labels are the members' mailboxes.** A correlated `DISTINCT` subquery gathers them per
///   thread; the filter's `EXISTS` is the same membership, so a filter and a label always agree.
///
/// `thread_id IS NOT NULL` skips a message mid-application: the threading pass
/// assigns every message an id in the same transaction that stores it, so a `NULL`
/// is a row no committed read should ever group on.
fn page(
    conn: &Connection,
    account: &str,
    label: Option<&str>,
    cursor: Option<&ThreadCursor>,
    limit: i64,
) -> Result<ThreadsPage, String> {
    // The keyword bits are compile-time constants of the stored bitfield, inlined
    // into the text rather than bound: binding them would stretch the parameter
    // list for two numbers no caller can change.
    let unread_bit = SEEN | DRAFT;
    let mut sql = format!(
        "SELECT m.thread_id, COUNT(*), \
         SUM((m.flags & {unread_bit}) = 0), SUM((m.flags & {FLAGGED}) > 0), \
         SUM(m.has_attachment), \
         m.subject, m.preview, m.from_name, m.from_addr, \
         CAST(unixepoch(MAX(m.date_utc)) AS INTEGER), \
         (SELECT group_concat(value, char(10)) FROM \
            (SELECT DISTINCT b.value FROM membership b JOIN message mm \
               ON mm.scope_key = b.scope_key AND mm.provider_key = b.provider_key \
              WHERE mm.account = ?1 AND mm.thread_id = m.thread_id \
                AND b.kind = 'mailbox')) \
         FROM message m WHERE m.account = ?1 AND m.thread_id IS NOT NULL"
    );
    // Positional parameters are numbered as the optional clauses land, because a
    // bound slice answers ?1..?n with no gaps.
    let mut params: Vec<Value> = vec![Value::Text(account.to_owned())];
    let mut next = 2;
    if let Some(label) = label {
        let _ = write!(
            sql,
            " AND EXISTS (SELECT 1 FROM membership b \
               WHERE b.scope_key = m.scope_key AND b.provider_key = m.provider_key \
                 AND b.kind = 'mailbox' AND b.value = ?{next})"
        );
        params.push(Value::Text(label.to_owned()));
        next += 1;
    }
    sql.push_str(" GROUP BY m.thread_id");
    if let Some(cursor) = cursor {
        let (date_index, id_index) = (next, next + 1);
        let _ = write!(
            sql,
            " HAVING (COALESCE(CAST(unixepoch(MAX(m.date_utc)) AS INTEGER), -1), m.thread_id) \
               < (?{date_index}, ?{id_index})"
        );
        params.push(Value::Integer(cursor.date));
        params.push(Value::Text(cursor.id.clone()));
        next += 2;
    }
    let _ = write!(
        sql,
        " ORDER BY COALESCE(CAST(unixepoch(MAX(m.date_utc)) AS INTEGER), -1) DESC, \
           m.thread_id DESC LIMIT ?{next}"
    );
    params.push(Value::Integer(limit));

    let mut stmt = conn.prepare_cached(&sql).map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|err| err.to_string())?;
    let mut threads = Vec::new();
    for row in rows {
        let (
            thread_id,
            total,
            unread,
            starred,
            attachments,
            subject,
            snippet,
            from_name,
            from_address,
            last_date,
            labels,
        ) = row.map_err(|err| err.to_string())?;
        threads.push(ThreadSummary {
            thread_id,
            subject,
            snippet,
            unread,
            total,
            labels: mailbox_ids(labels.as_deref())?,
            last_date,
            starred: starred != 0,
            has_attachments: attachments != 0,
            from_name,
            from_address,
        });
    }

    // A short page is the end of the list by construction. A full one carries the
    // cursor its last row names — unless that row has no date to key on, in which
    // case nothing after it is ordered by anything a `(date, id)` cursor can say.
    let full = usize::try_from(limit).is_ok_and(|limit| threads.len() == limit);
    let next_cursor = if full {
        threads.last().and_then(|last| {
            last.last_date.map(|date| ThreadCursor {
                date,
                id: last.thread_id.clone(),
            })
        })
    } else {
        None
    };
    Ok(ThreadsPage {
        threads,
        next_cursor,
    })
}

/// Splits the labels subquery's newline-joined text back into mailbox ids, sorted
/// so a page's label lists are ordered the same way whatever the scan visited
/// first.
fn mailbox_ids(joined: Option<&str>) -> Result<Vec<MailboxId>, String> {
    let mut ids = Vec::new();
    if let Some(joined) = joined {
        for value in joined.split('\n').filter(|value| !value.is_empty()) {
            ids.push(
                MailboxId::try_from(value)
                    .map_err(|err| format!("a stored label is not a mailbox id: {err}"))?,
            );
        }
    }
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
#[path = "threads_tests.rs"]
mod threads_tests;
