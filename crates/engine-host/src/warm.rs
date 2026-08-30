//! Batch body warming (ER-3): the background pass that makes the synced window
//! readable (and searchable) offline, one bounded batch at a time.
//!
//! A sync lands **metadata**; a body is fetched when a message is opened, which
//! leaves a fresh account's whole window cold. [`warm_mail_bodies`] is the pass a
//! host's scheduler loops over to close that gap: it takes the engine's own
//! [`mail_missing_body`](Engine::mail_missing_body) work list (one indexed query —
//! the warm set is the larger half, so per-message cache probes would spend more
//! than the fetches they precede), hands the whole batch to a
//! [`BatchSourceFetch`], and caches both halves of everything that came back with
//! engine-sync's single-message semantics (`fetch_message_body`): extract with
//! `engine-mime`, write the raw blob and the text **best-effort** — a failed cache
//! write never fails the warm; the row simply stays on the work list.
//!
//! # Why a batch seam
//!
//! The single-message verb exists ([`Engine::message_body`]); what it cannot do is
//! amortize. IMAP can answer one `UID FETCH 1:50` for fifty bodies at the cost of
//! one round trip, and a warm is the one caller that always wants exactly that.
//! [`BatchSourceFetch`] is that seam: per-item failures ride *inside* the batch
//! result (one dead message must not forfeit its forty-nine neighbors — the same
//! isolation the sequential default gives), and an adapter that can pipeline
//! ([`ImapProvider`]) overrides the loop while the rest inherit
//! [`sequential_sources`].
//!
//! # Budget semantics
//!
//! `budget` bounds **one pass**: the work-list query is `LIMIT budget`, so a pass
//! touches at most that many messages and spends at most that many fetches — a
//! scheduler paces the account, not this function, which never loops internally.
//! What is left over is reported as [`WarmReport::remaining_hint`] (the engine's
//! own re-query, not an arithmetic guess), so the caller can schedule the next
//! pass or stop: a warm end-state is `remaining_hint == 0`, not `failed == 0`.

use std::collections::HashMap;

use async_trait::async_trait;
use engine_api::{AccountId, Engine, Provider};
use engine_core::{ids::ProviderKey, mail::Message, raw::RawMime};
use engine_provider::ProviderError;
use engine_store::{MessageBodyStore, MessageSourceCache};
use provider_eas::EasAdapter;
use provider_imap::ImapProvider;
use tokio::io::{AsyncRead, AsyncWrite};

/// Fetches the raw RFC 5322 sources of a whole batch in one call — the warm's
/// amortization seam over [`Provider::fetch_message_source`].
///
/// Returns one entry per batch item, **in batch order**, keyed by its provider
/// key: per-item failures travel inline as `Err` rather than failing the call, so
/// a warm is never forfeited by the one dead message in it (an expunged UID is a
/// normal `Conflict`, not an abort). The `Message` halves of the pairs are what
/// adapters address the fetch with — the IMAP `(mailbox, UIDVALIDITY, UID)` key,
/// the JMAP/Graph `blob_id` handle.
#[async_trait]
pub trait BatchSourceFetch {
    /// Fetches every batch item's raw source, one result per item, batch-ordered.
    async fn fetch_message_sources(
        &self,
        account: &AccountId,
        batch: &[(&ProviderKey, &Message)],
    ) -> Vec<(ProviderKey, Result<RawMime, ProviderError>)>;
}

/// The default batch strategy: one [`Provider::fetch_message_source`] per item,
/// in batch order, each failure isolated to its own entry.
///
/// Every impl that has no protocol-level batch verb delegates here (EAS today:
/// its `ItemOperation` fetch covers one message per request, so a multi-fetch
/// override is a later enhancement, not a correctness gap).
pub async fn sequential_sources<P: Provider>(
    provider: &P,
    account: &AccountId,
    batch: &[(&ProviderKey, &Message)],
) -> Vec<(ProviderKey, Result<RawMime, ProviderError>)> {
    let mut out = Vec::with_capacity(batch.len());
    for (key, message) in batch {
        let fetched = provider.fetch_message_source(account, message).await;
        out.push(((*key).clone(), fetched));
    }
    out
}

#[async_trait]
impl BatchSourceFetch for EasAdapter {
    async fn fetch_message_sources(
        &self,
        account: &AccountId,
        batch: &[(&ProviderKey, &Message)],
    ) -> Vec<(ProviderKey, Result<RawMime, ProviderError>)> {
        sequential_sources(self, account, batch).await
    }
}

/// What one bounded warm pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmReport {
    /// Batch items fetched and cached. The cache *writes* are best-effort, so a
    /// message whose write faulted still counts here — and stays on the work
    /// list, which is what `remaining_hint` is for.
    pub fetched: usize,
    /// Batch items whose fetch failed (counted per class-blind item; the batch
    /// result carries the errors themselves).
    pub failed: usize,
    /// How many messages the engine still reports missing either cache half
    /// after the pass — the engine's own re-query, not a guess. The gap between
    /// this and `fetched + failed` is messages the budget never reached.
    pub remaining_hint: usize,
}

/// Warms one bounded batch of the account's missing bodies.
///
/// The pass: [`Engine::mail_missing_body`] (bounded by `budget`) → resolve the
/// rows to whole `Message`s through [`Engine::messages_by_keys`] → one
/// [`BatchSourceFetch`] call for the entire list → for each `Ok`, extract the
/// displayable text and cache both halves best-effort through
/// [`Engine::host_store`]. A row that resolves to no message (moved or tombstoned
/// between the two reads) is skipped, not failed: the store is already the
/// authority on what exists, and the row simply stays on the next pass's list.
///
/// No loop, no timing — pacing is the scheduler's; see the module docs.
///
/// # Errors
///
/// `Err` only when the engine itself could not answer a store read (the work
/// list, the resolve, or the final re-query). Provider and cache-write failures
/// are never errors here: they are per-item outcomes and best-effort writes.
pub async fn warm_mail_bodies(
    engine: &Engine,
    batch: &dyn BatchSourceFetch,
    account: &AccountId,
    budget: usize,
) -> Result<WarmReport, String> {
    let missing = engine
        .mail_missing_body(core::slice::from_ref(account), budget)
        .await
        .map_err(|err| err.to_string())?;

    // The work list names keys; the fetch addresses messages. Resolve once for
    // the whole batch, then pair in the work list's own (newest-first) order.
    // An empty list (nothing missing — or a zero budget) resolves and fetches
    // nothing; the re-query below still reports the account's true backlog.
    let mut fetched = 0;
    let mut failed = 0;
    if !missing.is_empty() {
        let keys: Vec<ProviderKey> = missing.iter().map(|row| row.mail.key.clone()).collect();
        let resolved = engine
            .messages_by_keys(account, &keys)
            .await
            .map_err(|err| err.to_string())?;
        let mut pairs = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some(message) = resolved.iter().find(|message| message.id.key() == key) {
                pairs.push((key, message));
            }
        }
        if !pairs.is_empty() {
            let results = batch.fetch_message_sources(account, &pairs).await;
            let store = engine.host_store();
            for (key, outcome) in results {
                match outcome {
                    Ok(raw) => {
                        // Extract first, then move the bytes into the blob cache.
                        let body = engine_mime::extract_body(&raw);
                        let _ = store.put_message_source(account, &key, raw).await;
                        let _ = store.put_message_body(account, &key, &body).await;
                        fetched += 1;
                    }
                    Err(_) => failed += 1,
                }
            }
        }
    }

    // The engine's own answer, re-queried: it counts what the budget never
    // reached *and* anything whose best-effort write faulted — the honest number.
    let remaining_hint = engine
        .mail_missing_body(core::slice::from_ref(account), usize::MAX)
        .await
        .map_err(|err| err.to_string())?
        .len();
    Ok(WarmReport {
        fetched,
        failed,
        remaining_hint,
    })
}

/// Parses an IMAP message key `imap:v<validity>:u<uid>@<mailbox>` into its
/// `(mailbox, UIDVALIDITY, UID)` — the same shape provider-imap synthesizes and
/// parses crate-internally; re-derived here because its parser is private.
fn parse_imap_key(key: &ProviderKey) -> Option<(&str, u32, u32)> {
    let rest = key.as_str().strip_prefix("imap:v")?;
    let (validity, rest) = rest.split_once(":u")?;
    let (uid, mailbox) = rest.split_once('@')?;
    if mailbox.is_empty() {
        return None;
    }
    Some((mailbox, validity.parse().ok()?, uid.parse().ok()?))
}

/// One mailbox's slice of a batch: the items sharing `(mailbox, UIDVALIDITY)`,
/// in batch order — exactly the set one pipelined `UID FETCH <set>` serves, since
/// a UID only means something inside one UIDVALIDITY generation of one mailbox.
#[derive(Debug)]
struct MailboxGroup<'a> {
    /// The mailbox to EXAMINE, as the key names it.
    mailbox: String,
    /// The generation the keys were synthesized under; the EXAMINE guard.
    uid_validity: u32,
    /// `(key, message, UID)` per item, in the order the batch named them.
    items: Vec<(&'a ProviderKey, &'a Message, u32)>,
}

/// Splits a batch into per-mailbox groups (first-appearance order) plus the keys
/// refused outright: a foreign (non-IMAP) key never reaches the wire, matching
/// provider-imap's own fetch, which rejects unparseable keys before any command.
fn group_imap_batch<'a>(
    batch: &[(&'a ProviderKey, &'a Message)],
) -> (Vec<MailboxGroup<'a>>, Vec<(&'a ProviderKey, ProviderError)>) {
    let mut groups: Vec<MailboxGroup<'a>> = Vec::new();
    let mut rejects = Vec::new();
    for (key, message) in batch {
        let (key, message) = (*key, *message);
        match parse_imap_key(key) {
            Some((mailbox, validity, uid)) => {
                let group = groups
                    .iter_mut()
                    .find(|group| group.mailbox == mailbox && group.uid_validity == validity);
                match group {
                    Some(group) => group.items.push((key, message, uid)),
                    None => groups.push(MailboxGroup {
                        mailbox: mailbox.to_owned(),
                        uid_validity: validity,
                        items: vec![(key, message, uid)],
                    }),
                }
            }
            None => rejects.push((
                key,
                ProviderError::invalid_state(format!(
                    "unparseable IMAP message key: {}",
                    key.as_str()
                )),
            )),
        }
    }
    (groups, rejects)
}

/// Assembles the IMAP UID-set for one group's UIDs: ascending, consecutive runs
/// compressed to `a:b` — the `<set>` of the one pipelined `UID FETCH` the group
/// commands (RFC 9051 §4.1.1).
fn uid_set(uids: &[u32]) -> String {
    use std::fmt::Write as _;
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut set = String::new();
    let mut run: Option<(u32, u32)> = None;
    let flush = |run: &mut Option<(u32, u32)>, set: &mut String| {
        if let Some((first, last)) = run.take() {
            if !set.is_empty() {
                set.push(',');
            }
            // A run of one prints bare; a longer one as `first:last`.
            if first == last {
                let _ = write!(set, "{first}");
            } else {
                let _ = write!(set, "{first}:{last}");
            }
        }
    };
    for &uid in &sorted {
        match run {
            Some((first, last)) if last + 1 == uid => run = Some((first, uid)),
            Some(_) => {
                flush(&mut run, &mut set);
                run = Some((uid, uid));
            }
            None => run = Some((uid, uid)),
        }
    }
    flush(&mut run, &mut set);
    set
}

/// Maps one group's UID-keyed fetch outcomes back onto its items, in item order —
/// the fan-out a pipelined response needs, since the server answers per UID in
/// its own order, not the batch's. A UID with no outcome (expunged mid-batch)
/// reads as a `Conflict`, exactly like provider-imap's single-fetch path.
fn fan_out_group(
    group: &MailboxGroup,
    by_uid: &mut HashMap<u32, Result<RawMime, ProviderError>>,
) -> Vec<(ProviderKey, Result<RawMime, ProviderError>)> {
    group
        .items
        .iter()
        .map(|(key, _, uid)| {
            let outcome = by_uid.remove(uid).unwrap_or_else(|| {
                Err(ProviderError::conflict(format!(
                    "message UID {uid} no longer exists (expunged): re-sync before fetching"
                )))
            });
            ((*key).clone(), outcome)
        })
        .collect()
}

#[async_trait]
impl<S> BatchSourceFetch for ImapProvider<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Serves the batch through the pipelined shape: group by
    /// `(mailbox, UIDVALIDITY)`, assemble each group's one-command UID set, fetch,
    /// fan the UID-keyed answers back out in batch order.
    ///
    /// **The wire step's ceiling today.** provider-imap's session is private
    /// (`connection: Mutex<Connection<S>>` is `pub(crate)`, and no public batched
    /// body fetch exists), and this crate must not modify provider-imap — so the
    /// group's assembled set is executed as per-UID `fetch_message_source` calls
    /// over the provider's own standing session (serialized under its lock), not
    /// yet as the single `UID FETCH <uid_set> (UID BODY.PEEK[])` round trip the
    /// grouping exists for. Everything around that hop *is* the pipeline: foreign
    /// keys are refused before any wire traffic, the UID set is assembled and
    /// names the command in every failure's detail, and the fan-out is the pure
    /// UID→key mapping the one-command response will feed. Collapsing the hop is
    /// one provider seam away (its private `Connection::uid_fetch(set, items)`
    /// already speaks the command); the scripted-server test pins today's request
    /// count so that change lands consciously.
    async fn fetch_message_sources(
        &self,
        account: &AccountId,
        batch: &[(&ProviderKey, &Message)],
    ) -> Vec<(ProviderKey, Result<RawMime, ProviderError>)> {
        let (groups, rejects) = group_imap_batch(batch);
        let mut by_key: HashMap<ProviderKey, Result<RawMime, ProviderError>> = rejects
            .into_iter()
            .map(|(key, err)| ((*key).clone(), Err(err)))
            .collect();
        for group in &groups {
            // The one command this group's items form — the batch identity every
            // failure below reports, and the set the pipelined hop will send.
            let set = uid_set(
                &group
                    .items
                    .iter()
                    .map(|(_, _, uid)| *uid)
                    .collect::<Vec<_>>(),
            );
            let mut by_uid: HashMap<u32, Result<RawMime, ProviderError>> = HashMap::new();
            for (_key, message, uid) in &group.items {
                let outcome = match Provider::fetch_message_source(self, account, message).await {
                    Ok(raw) => Ok(raw),
                    Err(err) => Err(ProviderError::new(
                        err.class(),
                        format!(
                            "{} (warm batch UID FETCH {set} on {})",
                            err.detail(),
                            group.mailbox
                        ),
                    )),
                };
                by_uid.insert(*uid, outcome);
            }
            for (key, outcome) in fan_out_group(group, &mut by_uid) {
                by_key.insert(key, outcome);
            }
        }
        batch
            .iter()
            .map(|(key, _)| {
                let outcome = by_key
                    .remove(key)
                    .expect("every batch key was grouped or rejected");
                ((*key).clone(), outcome)
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "warm_tests.rs"]
mod warm_tests;

// The IMAP-side tests are a sibling split so neither file crosses the 500-line
// ceiling (the `provider_tests`/`*_over_tests` precedent).
#[cfg(test)]
#[path = "warm_imap_tests.rs"]
mod warm_imap_tests;
