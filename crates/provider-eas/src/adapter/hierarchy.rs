// SPDX-License-Identifier: MPL-2.0
//! The shared account-level hierarchy-SyncKey ledger (P2 Task 3, parked
//! from Task 2's review; the contacts scope wired in P2 Task 5): one
//! server FolderSync cursor serving ALL the container scopes
//! (`EasFolderList`, `EasCalendarList`, and `EasContactList`).
//!
//! ## Why it exists
//!
//! The server answers FolderSync with ONE hierarchy SyncKey per device
//! partnership, but the engine persists a cursor **per scope** — so two
//! scopes that both ran FolderSync hold different generations of the same
//! server key, and every interleaved pass answers status 9 ("hierarchy out
//! of date") and re-enumerates from `"0"` as a snapshot. Correct (the
//! status-9 recovery), but wasteful: folder lists change rarely, and the
//! interleaved fan-out paid a full re-enumeration per round. A THIRD
//! container scope (contacts, P2 Task 5) would have paid it twice per
//! fan-out round — wiring `EasContactList` into the same ledger is why
//! the container set grew here rather than beside it.
//!
//! ## The shape (the collection-key ledger precedent)
//!
//! The adapter owns the hierarchy key the server last handed it, exactly as
//! it owns the bound collection's key for `edit_mail` (`adapter/mod.rs`'s
//! ledger section): a container pass rides the ledger's key when it is
//! fresher than the scope's engine cursor — resuming from a key another
//! scope rotated is lossless **only because the ledger also carries the
//! rows the riding scope missed** (its class's folders and the class-less
//! deletions of every round it did not serve), plus the present-set of any
//! snapshot another scope took (the riding scope's result then reads as a
//! snapshot itself, tombstoning at apply time). A cold ledger falls back to
//! the engine cursor — the pre-ledger behavior, bit for bit.
//!
//! **A scope with no engine cursor never rides** (its first-ever pass, or a
//! corrupted empty cursor): a delta answered to a warm key establishes no
//! snapshot authority, and the rotated key persisted as that scope's
//! cursor would permanently hide every standing folder of its class. Such
//! a pass bootstraps from `"0"` — unless a snapshot backlog is already
//! pending, whose present-set establishes the full enumeration and lets
//! the warm key ride losslessly after all.
//!
//! Scope: **per adapter**. An account whose scopes are served by several
//! adapters (each with its own ledger) keeps the old self-healing
//! status-9 behavior across those adapters — the ledger fixes the
//! interleaved passes of ONE adapter serving several scopes, the shape the
//! engine's calendar+mail (and now contacts) fan-out on one bound adapter
//! produces.

use std::collections::BTreeSet;

use engine_core::sync::SyncState;
use engine_provider::ProviderResult;
use tokio::sync::Mutex;

use super::{error::provider_error, mailboxes::BOOTSTRAP_KEY};
use crate::{
    client::{EasClient, EasError},
    types::EasFolder,
};

/// FolderSync status 9: "folder hierarchy out of date" — the stored
/// hierarchy SyncKey is invalidated (the shared constant of every
/// container slice's recovery).
const HIERARCHY_OUT_OF_DATE: u32 = 9;

/// Which container scope one pass serves — also the class filter and the
/// backlog slot the round reads and feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Container {
    /// The mail container scope (`EasFolderList`): `"Email"` folders and
    /// the classless shape.
    Mail,
    /// The calendar container scope (`EasCalendarList`): `"Calendar"`
    /// folders.
    Calendar,
    /// The contacts container scope (`EasContactList`): `"Contacts"`
    /// folders (folder Type 9, [MS-ASFD]).
    Contacts,
}

impl Container {
    /// Whether one FolderSync row belongs in this scope (the mail slice's
    /// `is_mail_class` rule and the class filters, restated once).
    fn keeps(self, folder: &EasFolder) -> bool {
        match self {
            Self::Mail => folder.class == "Email" || folder.class.is_empty(),
            Self::Calendar => folder.class == "Calendar",
            Self::Contacts => folder.class == "Contacts",
        }
    }

    /// The other scopes — where this round's non-serving rows pend.
    fn others(self) -> [Self; 2] {
        match self {
            Self::Mail => [Self::Calendar, Self::Contacts],
            Self::Calendar => [Self::Mail, Self::Contacts],
            Self::Contacts => [Self::Mail, Self::Calendar],
        }
    }
}

/// Rows one scope has not consumed yet: Add/Update folders of its class,
/// class-less deletions, and — after another scope bootstrapped — the
/// full present-set that supersedes both (a snapshot's rows ARE the
/// scope's current server state; applying them as a snapshot tombstones
/// whatever vanished in between).
#[derive(Debug, Default)]
struct Backlog {
    rows: Vec<EasFolder>,
    deletions: Vec<String>,
    present: Option<Vec<EasFolder>>,
}

/// The shared hierarchy ledger — a plain mutex, never held across an await
/// (every toucher holds the verb lock already; the collection-key ledger
/// discipline).
#[derive(Debug, Default)]
pub(super) struct HierarchyLedger {
    state: std::sync::Mutex<HierarchyState>,
}

#[derive(Debug, Default)]
struct HierarchyState {
    /// The hierarchy SyncKey the server last handed this adapter (`None`
    /// until its first container pass).
    key: Option<String>,
    mail: Backlog,
    calendar: Backlog,
    contacts: Backlog,
}

/// One container pass's outcome, in wire rows — each slice maps it onto its
/// own `ScopeSync` (the mapping code already lived per-slice).
pub(super) struct HierarchyRound {
    /// The pass's rows for its scope (backlog + response, in order).
    pub folders: Vec<EasFolder>,
    /// The deletions to apply (class-less on the wire, unfiltered — a key
    /// the scope never held tombstones as a store no-op).
    pub deletions: Vec<String>,
    /// `Some` when the round must read as a SNAPSHOT: it bootstrapped, or
    /// it consumed a snapshot backlog (the rows are then the full current
    /// state of the scope's class, and the keys are the tombstone
    /// authority).
    pub present_rows: Option<Vec<EasFolder>>,
    /// The rotated key — both the ledger's new value and the scope's
    /// `next_cursor`.
    pub next_key: String,
    /// Whether a status-9 invalidation forced a re-bootstrapped snapshot
    /// inside this pass (the `cursor_recovered` fact the contacts source
    /// sync reports; the calendar slice's `ScopeSync` shape has no slot
    /// for it, so it ignores the flag).
    pub recovered: bool,
}

impl HierarchyLedger {
    /// One FolderSync pass for `container`: ride the ledger's key when it
    /// is fresher than `cursor`, apply this scope's backlog, and pend the
    /// round's other-scope rows and deletions back into the ledger.
    /// Recovers a status-9 invalidation by re-bootstrapping once (the
    /// pre-ledger recovery, unchanged).
    ///
    /// # Errors
    ///
    /// Surfaces [`ProviderError`] from the FolderSync round-trip (a status
    /// 9 answered to `"0"` itself is `NeedsResync`, as before); a failed
    /// pass — including a failed status-9 recovery round — restores the
    /// drained backlog first, so the next pass still sees it.
    pub(super) async fn pass(
        &self,
        client: &Mutex<EasClient>,
        cursor: Option<&SyncState>,
        container: Container,
    ) -> ProviderResult<HierarchyRound> {
        // Take the request key and drain my backlog under the lock, then
        // release before the await (the ledger discipline). A scope WITH
        // an engine cursor may ride the ledger's key; a scope WITHOUT one
        // (its first-ever pass, or a corrupted empty cursor) must NOT:
        // the delta a warm key answers establishes no snapshot authority,
        // and the rotated key persisted as the fresh scope's cursor would
        // permanently hide every standing folder of its class — a restart
        // resumes from the poisoned cursor, so even the status-9
        // self-heal is bypassed. It bootstraps instead, unless another
        // scope's snapshot is already pending in the backlog: consuming
        // that present-set establishes the full enumeration, so the warm
        // key then rides losslessly.
        let cursor_key = super::mailboxes::request_key(cursor);
        let (request_key, backlog) = {
            let mut state = self.state.lock().expect("hierarchy ledger");
            let backlog = std::mem::take(state.backlog_of_mut(container));
            let key = match &state.key {
                Some(key) if cursor_key != BOOTSTRAP_KEY || backlog.present.is_some() => {
                    key.clone()
                }
                _ => cursor_key.to_owned(),
            };
            (key, backlog)
        };
        let mut client = client.lock().await;
        client.set_hierarchy_sync_key(request_key.clone());
        let (result, snapshot_round, recovered) = match client.folder_sync(&request_key).await {
            Ok(result) => (result, request_key == BOOTSTRAP_KEY, false),
            // The requested key is dead — restart from the bootstrap key,
            // exactly once; the full hierarchy that round returns IS the
            // snapshot recovery (a server answering 9 to "0" itself
            // surfaces through `provider_error` as NeedsResync, so this
            // cannot loop). A FAILED recovery round restores the drained
            // backlog before surfacing, exactly like any failed pass.
            Err(EasError::CommandStatus {
                status: HIERARCHY_OUT_OF_DATE,
                ..
            }) if request_key != BOOTSTRAP_KEY => {
                let recovery = match client.folder_sync(BOOTSTRAP_KEY).await {
                    Ok(recovery) => recovery,
                    Err(e) => {
                        self.restore(container, backlog);
                        return Err(provider_error(e));
                    }
                };
                (recovery, true, true)
            }
            Err(e) => {
                self.restore(container, backlog);
                return Err(provider_error(e));
            }
        };
        drop(client);
        let bootstrapped = snapshot_round;
        let next_key = super::mailboxes::next_key(&result.sync_key, &request_key).to_owned();

        // Fold my round. A bootstrap round supersedes the drained backlog
        // wholesale (the full hierarchy it returns IS the current state —
        // a drained row whose folder since vanished would otherwise
        // resurrect); a delta round folds the backlog rows first (older),
        // then the response's (newer, superseding by ServerId), with the
        // deletions unioned.
        let mut folders = if bootstrapped {
            Vec::new()
        } else {
            backlog.rows
        };
        folders.extend(
            result
                .changes
                .iter()
                .filter(|f| container.keeps(f))
                .cloned(),
        );
        dedupe_folders(&mut folders);
        let mut deletions = backlog.deletions;
        let response_deletions: Vec<String> = result
            .deletions
            .iter()
            .filter(|id| !id.is_empty() && !deletions.contains(id))
            .cloned()
            .collect();
        deletions.extend(response_deletions);

        // Snapshot authority: my own bootstrap's rows are the present set;
        // a consumed present-backlog makes a delta round read as a snapshot
        // too, its rows folded in and everything deleted since removed.
        let present_rows = if bootstrapped {
            Some(folders.clone())
        } else if let Some(present) = backlog.present {
            let mut rows = present;
            rows.extend(folders.clone());
            dedupe_folders(&mut rows);
            rows.retain(|row| !deletions.contains(&row.server_id));
            Some(rows)
        } else {
            None
        };

        // Pend the other scopes' share: their rows from this round, the
        // class-less deletions (every scope receives them), and — for a
        // bootstrap — the present-set that supersedes whatever each had.
        // The key records only when the response carried one: a key-less
        // success names no server state, and pinning the request key would
        // shadow a fresher engine cursor (the Sync empty-body invariant).
        {
            let mut state = self.state.lock().expect("hierarchy ledger");
            if !result.sync_key.is_empty() {
                state.key = Some(next_key.clone());
            }
            for other_scope in container.others() {
                let other = state.backlog_of_mut(other_scope);
                if bootstrapped {
                    let other_rows: Vec<EasFolder> = result
                        .changes
                        .iter()
                        .filter(|f| other_scope.keeps(f))
                        .cloned()
                        .collect();
                    *other = Backlog {
                        rows: Vec::new(),
                        deletions: Vec::new(),
                        present: Some(other_rows),
                    };
                } else {
                    other.rows.extend(
                        result
                            .changes
                            .iter()
                            .filter(|f| other_scope.keeps(f))
                            .cloned(),
                    );
                    let response_deletions: Vec<String> = result
                        .deletions
                        .iter()
                        .filter(|id| !id.is_empty() && !other.deletions.contains(id))
                        .cloned()
                        .collect();
                    other.deletions.extend(response_deletions);
                }
            }
        }
        Ok(HierarchyRound {
            folders,
            deletions,
            present_rows,
            next_key,
            recovered,
        })
    }

    /// Puts a drained backlog back after a failed round — the server state
    /// did not move for THIS round's key, but the ledger may have: another
    /// scope's round can complete between this round's drain and its
    /// failure (the pend happens after `drop(client)`, the restore while
    /// still holding the client lock), pending newer rows or a fresh
    /// snapshot present-set into the slot. The merge is by recency:
    /// drained rows are older than anything pended since the drain, so
    /// they restore FIRST (the consuming fold's dedupe keeps the last row
    /// per id), and a present-set pended after the drain supersedes the
    /// drained rows and the drained present-set wholesale (the
    /// bootstrap-supersedes rule — a drained row whose folder since
    /// vanished must not resurrect).
    fn restore(&self, container: Container, backlog: Backlog) {
        let mut state = self.state.lock().expect("hierarchy ledger");
        let slot = state.backlog_of_mut(container);
        // Class-less tombstones union either way — a deletion is
        // idempotent, and dropping one would un-tombstone nothing.
        let drained_deletions: Vec<String> = backlog
            .deletions
            .into_iter()
            .filter(|id| !id.is_empty() && !slot.deletions.contains(id))
            .collect();
        slot.deletions.extend(drained_deletions);
        if slot.present.is_some() {
            // A snapshot another scope took after this round drained IS
            // the scope's newer current state — it supersedes everything
            // the round drained.
            return;
        }
        let mut rows = backlog.rows;
        rows.append(&mut slot.rows);
        slot.rows = rows;
        slot.present = backlog.present;
    }
}

impl HierarchyState {
    fn backlog_of_mut(&mut self, container: Container) -> &mut Backlog {
        match container {
            Container::Mail => &mut self.mail,
            Container::Calendar => &mut self.calendar,
            Container::Contacts => &mut self.contacts,
        }
    }
}

/// Keeps the LAST occurrence per ServerId (the later row is the newer
/// state), preserving first-seen order.
fn dedupe_folders(folders: &mut Vec<EasFolder>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<EasFolder> = Vec::with_capacity(folders.len());
    for row in folders.iter().rev() {
        if seen.insert(row.server_id.clone()) {
            out.push(row.clone());
        }
    }
    out.reverse();
    *folders = out;
}

#[cfg(test)]
#[path = "hierarchy_tests.rs"]
mod tests;
