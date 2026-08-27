// SPDX-License-Identifier: MPL-2.0
//! The `sync_mailboxes` verb: EAS `FolderSync` mapped onto the engine's
//! `ScopeSync<Mailbox>` (the JMAP `container_sync` code precedent — the
//! spike's §3.3 verdict was "no gap").
//!
//! ## Mapping
//!
//! The **hierarchy SyncKey is the cursor**: `cursor: None` (or an empty
//! cursor string) bootstraps from `"0"`, whose round returns the full
//! hierarchy → [`SyncUpdate::Snapshot`] with the mail folders' keys as the
//! `present` set. A `Some(key)` round returns the wire's Add/Update/Delete
//! elements → [`SyncUpdate::Delta`] (`changed` + explicit `removed`). The
//! rotated key the response carries is the `next_cursor` the store persists
//! — echoed from the request when a success response omits it (the Sync
//! empty-body invariant: an empty key would poison the cursor).
//!
//! **Status 9** ("folder hierarchy out of date") is the EAS
//! `cannotCalculateChanges`: the stored key can never produce a delta again.
//! The call recovers internally — one re-bootstrap from `"0"` — and returns
//! the full hierarchy as a snapshot, so the orchestrator only ever sees one
//! healthy `ScopeSync` (the JMAP needs-resync→snapshot-fallback precedent).
//!
//! **Class filtering**: `Mailbox` is the *mail* container type. FolderSync
//! returns every class (calendar, contacts, tasks, notes), so non-mail
//! folders are filtered out of the mail scope — they belong to the
//! calendar/contacts scopes, whose adapter slices run their own FolderSync
//! and keep their own classes (`docs/agent-guidance/eas.md`). A delta's
//! deletions pass through unfiltered: the wire's Delete element carries only
//! a ServerId (no class), and removing a key the mail scope never held is a
//! store no-op.

use std::collections::BTreeSet;

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::{Mailbox, MailboxRole},
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ProviderError, ProviderResult, ScopeSync};
use serde_json::json;
use tokio::sync::Mutex;

use super::error::provider_error;
use crate::{
    client::{EasClient, EasError},
    types::{EasFolder, FolderSyncResult},
};

/// The FolderSync bootstrap key ([MS-ASFolderSync] §2.2.3.1.7.2): requesting
/// it returns the full hierarchy, so a round sent with this key is by
/// definition a snapshot. Shared with the email slice — a collection Sync
/// bootstraps from the same "0".
pub(super) const BOOTSTRAP_KEY: &str = "0";

/// FolderSync status 9: "folder hierarchy out of date" — the stored hierarchy
/// SyncKey is invalidated ([MS-ASFolderSync] §2.2.3.1.10; `status.rs` maps it
/// to `ResetSyncKey`).
const HIERARCHY_OUT_OF_DATE: u32 = 9;

/// The adapter's extended-property namespace (the namespacing convention the
/// engine leaves to each adapter): the EAS-native facts that have no
/// first-class `Mailbox` field.
const EXTENDED_NAMESPACE: &str = "eas";

/// One FolderSync pass: request `key`, map the wire delta (or, for the
/// bootstrap key, the full hierarchy) into a `ScopeSync<Mailbox>`, and
/// recover a status-9 invalidation by re-bootstrapping once.
///
/// The client's in-memory hierarchy-key cache is primed from the engine's
/// cursor first — the cursor is the authority the store round-trips, and the
/// cache (which folder ops echo per [MS-ASCMD]) must not disagree with it.
pub(super) async fn sync(
    client: &Mutex<EasClient>,
    cursor: Option<&SyncState>,
) -> ProviderResult<ScopeSync<Mailbox>> {
    let key = request_key(cursor);
    let mut client = client.lock().await;
    client.set_hierarchy_sync_key(key.to_owned());
    match client.folder_sync(key).await {
        Ok(result) => scope_sync(&result, key),
        // The stored key is dead. Restart from the bootstrap key — the full
        // hierarchy it returns is the snapshot recovery — exactly once: a
        // server that answers 9 to `"0"` itself surfaces through
        // `provider_error` as `NeedsResync` for the orchestrator to drop the
        // cursor and retry, so this call can never loop.
        Err(EasError::CommandStatus {
            status: HIERARCHY_OUT_OF_DATE,
            ..
        }) if key != BOOTSTRAP_KEY => {
            let result = client
                .folder_sync(BOOTSTRAP_KEY)
                .await
                .map_err(provider_error)?;
            scope_sync(&result, BOOTSTRAP_KEY)
        }
        Err(e) => Err(provider_error(e)),
    }
}

/// The hierarchy key this pass requests: the cursor's string, with `None` and
/// the empty string (a corrupted cursor) both bootstrapping from `"0"`. The
/// same rule resolves a collection Sync's key (the email slice).
pub(super) fn request_key(cursor: Option<&SyncState>) -> &str {
    cursor.map_or(BOOTSTRAP_KEY, |state| {
        let key = state.as_str();
        if key.is_empty() { BOOTSTRAP_KEY } else { key }
    })
}

/// Maps one FolderSync round into a `ScopeSync` — a snapshot when the round
/// requested the bootstrap key (by protocol definition that round carries the
/// full hierarchy), a delta otherwise — with the returned SyncKey as the
/// next cursor, or the request's key when the response omits one.
fn scope_sync(result: &FolderSyncResult, request_key: &str) -> ProviderResult<ScopeSync<Mailbox>> {
    let next_cursor = SyncState::new(next_key(&result.sync_key, request_key));
    if request_key == BOOTSTRAP_KEY {
        let objects = mailboxes(&result.changes)?;
        let present: BTreeSet<ProviderKey> = objects.iter().map(key_of).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(objects, present),
            next_cursor,
        ))
    } else {
        let changed = mailboxes(&result.changes)?;
        // The wire's Delete carries only a ServerId (no class), so deletions
        // pass through unfiltered — tombstoning a key the mail scope never
        // held is a store no-op. An empty ServerId is dropped (it can key
        // nothing).
        let removed: Vec<ProviderKey> = result
            .deletions
            .iter()
            .filter(|id| !id.is_empty())
            .map(|id| ProviderKey::new(id.clone()))
            .collect::<Result<_, _>>()
            .map_err(|e| empty_key(&e.to_string()))?;
        Ok(ScopeSync::new(
            SyncUpdate::delta(changed, removed),
            next_cursor,
        ))
    }
}

/// The cursor a response's SyncKey advances to. An empty key would poison the
/// persisted cursor (the Sync empty-body invariant), so a success response
/// that omits the element keeps the request's key — the round changed
/// nothing the caller needs to remember.
pub(super) fn next_key<'a>(returned: &'a str, request_key: &'a str) -> &'a str {
    if returned.is_empty() {
        request_key
    } else {
        returned
    }
}

/// Maps the wire's Add/Update elements to whole [`Mailbox`]es, dropping the
/// non-mail classes (see the module docs: they belong to the calendar and
/// contacts scopes).
fn mailboxes(changes: &[EasFolder]) -> ProviderResult<Vec<Mailbox>> {
    changes
        .iter()
        .filter(|folder| is_mail_class(&folder.class))
        .map(mailbox)
        .collect()
}

/// Whether a folder belongs in the mail container scope. FolderSync reports
/// every class; `"Email"` (and the classless shape a missing `Type` element
/// parses to — the `folder_type_to_class` unrecognized→Email default) is the
/// mail one.
fn is_mail_class(class: &str) -> bool {
    class == "Email" || class.is_empty()
}

/// Maps one wire folder to a [`Mailbox`]: ServerId→id, ParentId `"0"`→no
/// parent ([MS-ASFolderSync]'s root sentinel), `Type`→the normalized role
/// where the engine has one (2 Inbox, 3 Drafts, 4 Trash, 5 Sent; the Outbox
/// and the user-created mail types have no normalized role — the raw type
/// survives in `extended`). EAS reports no unread count (that is
/// `GetItemEstimate`, not FolderSync) and no per-folder revision tokens.
fn mailbox(folder: &EasFolder) -> ProviderResult<Mailbox> {
    let id =
        MailboxId::try_from(folder.server_id.as_str()).map_err(|e| empty_key(&e.to_string()))?;
    let mut mailbox = Mailbox::new(id, folder.display_name.clone());
    mailbox.parent = if folder.parent_id.is_empty() || folder.parent_id == BOOTSTRAP_KEY {
        None
    } else {
        Some(MailboxId::try_from(folder.parent_id.as_str()).map_err(|e| {
            ProviderError::permanent(format!(
                "FolderSync folder {} carries an unusable ParentId: {e}",
                folder.server_id
            ))
        })?)
    };
    mailbox.role = role(folder.folder_type);
    mailbox
        .extended
        .set(format!("{EXTENDED_NAMESPACE}/class"), json!(folder.class));
    if let Some(folder_type) = folder.folder_type {
        mailbox.extended.set(
            format!("{EXTENDED_NAMESPACE}/folder-type"),
            json!(folder_type),
        );
    }
    Ok(mailbox)
}

/// The normalized role for an EAS folder type byte ([MS-ASFolderSync]
/// §2.2.3): only the default mail folders have one. Junk has no EAS type
/// (Exchange's Junk E-mail is a plain user folder); the Outbox type exists on
/// the wire but not in the engine's role set — `None`, with the type kept in
/// `extended`.
fn role(folder_type: Option<u8>) -> Option<MailboxRole> {
    match folder_type {
        Some(2) => Some(MailboxRole::Inbox),
        Some(3) => Some(MailboxRole::Drafts),
        Some(4) => Some(MailboxRole::Trash),
        Some(5) => Some(MailboxRole::Sent),
        _ => None,
    }
}

/// A Mailbox's provider key — the store's row identity for it.
fn key_of(mailbox: &Mailbox) -> ProviderKey {
    mailbox.id.key().clone()
}

/// The shared error for an id the engine cannot key (an empty ServerId, or a
/// Delete element carrying one): a malformed-change failure, permanent
/// because resending the same round returns the same bytes.
fn empty_key(detail: &str) -> ProviderError {
    ProviderError::permanent(format!("FolderSync change with an unusable id: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_folder(server_id: &str, parent: &str, class: &str, typ: Option<u8>) -> EasFolder {
        EasFolder {
            server_id: server_id.to_owned(),
            parent_id: parent.to_owned(),
            display_name: "Name".to_owned(),
            class: class.to_owned(),
            folder_type: typ,
        }
    }

    #[test]
    fn request_key_bootstraps_from_none_and_empty() {
        assert_eq!(request_key(None), BOOTSTRAP_KEY);
        assert_eq!(request_key(Some(&SyncState::new(""))), BOOTSTRAP_KEY);
        assert_eq!(request_key(Some(&SyncState::new("hier-7"))), "hier-7");
    }

    #[test]
    fn an_empty_response_key_keeps_the_request_key() {
        assert_eq!(next_key("", "hier-5"), "hier-5");
        assert_eq!(next_key("hier-6", "hier-5"), "hier-6");
    }

    #[test]
    fn non_mail_classes_are_filtered_from_the_mail_scope() {
        let folders = vec![
            wire_folder("fid-inbox", "0", "Email", Some(2)),
            wire_folder("fid-cal", "0", "Calendar", Some(8)),
            wire_folder("fid-contacts", "0", "Contacts", Some(9)),
            wire_folder("fid-tasks", "0", "Tasks", Some(7)),
            wire_folder("fid-notes", "0", "Notes", Some(11)),
            // A missing Type element parses classless — mail by default.
            wire_folder("fid-typeless", "0", "", None),
        ];
        let mapped = mailboxes(&folders).expect("mapping succeeds");
        let ids: Vec<&str> = mapped.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fid-inbox", "fid-typeless"]);
    }

    #[test]
    fn roles_come_from_the_type_byte_and_outbox_has_none() {
        let cases = [
            (2u8, Some(MailboxRole::Inbox)),
            (3, Some(MailboxRole::Drafts)),
            (4, Some(MailboxRole::Trash)),
            (5, Some(MailboxRole::Sent)),
            (6, None),  // Outbox — no normalized role
            (1, None),  // user-created mail
            (12, None), // user-created mail
        ];
        for (typ, expected) in cases {
            assert_eq!(role(Some(typ)), expected, "type {typ}");
        }
        assert_eq!(role(None), None);
    }

    #[test]
    fn a_folder_maps_with_parent_role_and_native_facts() {
        let folder = wire_folder("fid-arch", "fid-inbox", "Email", Some(1));
        let mailbox = mailbox(&folder).expect("mapping succeeds");
        assert_eq!(mailbox.id.as_str(), "fid-arch");
        assert_eq!(
            mailbox.parent.as_ref().map(MailboxId::as_str),
            Some("fid-inbox")
        );
        assert_eq!(mailbox.role, None);
        assert_eq!(
            mailbox.extended.get("eas/class"),
            Some(&json!("Email")),
            "the EAS class survives under the adapter namespace"
        );
        assert_eq!(
            mailbox.extended.get("eas/folder-type"),
            Some(&json!(1u8)),
            "the raw type byte survives (the Outbox shape has no role)"
        );
        assert_eq!(mailbox.unread_count, None, "FolderSync carries no counts");
    }

    #[test]
    fn the_root_parent_sentinel_maps_to_no_parent() {
        for parent in ["0", ""] {
            let folder = wire_folder("fid-inbox", parent, "Email", Some(2));
            let mailbox = mailbox(&folder).expect("mapping succeeds");
            assert!(
                mailbox.parent.is_none(),
                "parent {parent:?} is the root sentinel"
            );
        }
    }

    #[test]
    fn a_change_without_a_server_id_is_permanent() {
        let folder = wire_folder("", "0", "Email", Some(2));
        let err = mailbox(&folder).expect_err("an empty ServerId cannot key a Mailbox");
        assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    }

    #[test]
    fn a_bootstrap_round_maps_to_a_snapshot_and_an_incremental_to_a_delta() {
        let bootstrap = FolderSyncResult {
            status: 1,
            sync_key: "hier-1".to_owned(),
            changes: vec![
                wire_folder("fid-inbox", "0", "Email", Some(2)),
                wire_folder("fid-cal", "0", "Calendar", Some(8)),
            ],
            deletions: Vec::new(),
        };
        let sync = scope_sync(&bootstrap, BOOTSTRAP_KEY).expect("bootstrap maps");
        let SyncUpdate::Snapshot { objects, present } = &sync.update else {
            panic!("bootstrap must snapshot: {:?}", sync.update);
        };
        assert_eq!(objects.len(), 1, "only the mail folder");
        assert_eq!(present.len(), 1);
        assert_eq!(sync.next_cursor.as_str(), "hier-1");

        let incremental = FolderSyncResult {
            status: 1,
            sync_key: "hier-2".to_owned(),
            changes: vec![wire_folder("fid-arch", "fid-inbox", "Email", Some(1))],
            deletions: vec!["fid-old".to_owned(), String::new()],
        };
        let sync = scope_sync(&incremental, "hier-1").expect("delta maps");
        let SyncUpdate::Delta {
            changed, removed, ..
        } = &sync.update
        else {
            panic!("incremental must delta: {:?}", sync.update);
        };
        assert_eq!(changed.len(), 1);
        assert_eq!(
            removed,
            &vec![ProviderKey::new("fid-old").unwrap()],
            "the empty deletion id is dropped, not errored"
        );
        assert_eq!(sync.next_cursor.as_str(), "hier-2");
    }
}
