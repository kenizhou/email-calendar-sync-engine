// SPDX-License-Identifier: MPL-2.0
//! The contacts family (P2 Task 5): the full
//! [`ContactsProvider`](engine_provider::ContactsProvider) surface —
//! container discovery, card sync, and the write verbs — the calendar
//! family's shape applied to the Contacts class.
//!
//! ## Containers (`sync_address_books`)
//!
//! The **same FolderSync machinery** as the mail/calendar slices, wired
//! into the shared hierarchy ledger as its third container scope: contact
//! folders (class `Contacts`, folder Type 9 — [MS-ASFD]) land in the
//! per-account [`EasContactList`](engine_core::sync::SyncScope::EasContactList)
//! scope. The hierarchy SyncKey is the cursor (`None`/empty → bootstrap
//! `"0"` → snapshot; `Some(key)` → delta); a status-9 invalidation
//! recovers inside the call as a re-bootstrapped snapshot, surfaced as
//! `cursor_recovered` (the source-sync enum's slot — the calendar
//! `ScopeSync` had none). Delta deletions pass through unfiltered (the
//! wire's Delete element carries no class).
//!
//! ## Cards (`sync_contacts`)
//!
//! Sync class `Contacts` per collection — the bound contact folder IS
//! the `CollectionId` (the binding [`EasAdapter::with_contacts`]). The
//! **collection SyncKey is the cursor**, `MoreAvailable` pages the pass
//! inside the call, and a SyncKey invalidation (collection status 3/12)
//! discards the accumulated rounds and restarts from `"0"` **once** as a
//! snapshot with `cursor_recovered` — the `sync_events` recovery shape
//! verbatim (the whole-scope result applies atomically, so nothing has
//! been handed to the caller when the key dies). Items convert through
//! `contacts::contact_card_from_props` (id = ServerId, membership = the
//! bound folder); a malformed item is skipped, never failing the pass.
//!
//! ## Writes (`create_contact` / `patch_contact` / `delete_contact`)
//!
//! Sync Add/Change/Delete upsync — the `calendar_write` discipline
//! verbatim, in `contacts_write` (the 500-line split): the Add's ack is
//! the only id-reveal point, an empty patch is a no-op receipt, and
//! already-gone is delete success. Every verb rides the contacts
//! collection-key ledger (a cold ledger refuses `NeedsResync`).
//!
//! ## The honest refusals
//!
//! **`fetch_contact`** keeps its trait default (rejects): EAS has no
//! per-item read that leaves the collection key untouched — the Sync
//! Fetch command rides the cursor, and the ItemOperations fetch parses
//! only Body-shaped payloads ([`ItemOperationsFetchResult`] carries no
//! ApplicationData). A host reconciles through the next `sync_contacts`
//! pass. **`fetch_contact_photo`** keeps its default too: the EAS
//! `Picture` is inline payload dropped at parse time (the v1 ruling,
//! pinned by the picture-drop tests) and no fetchable URI survives —
//! `contact_photos` stays off (see [`EasAdapter::with_contacts`]).
//! **Group reads/writes** stay off: distribution lists are a separate,
//! unmodeled container class.

use std::collections::BTreeSet;

use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactField, ContactFieldSet, ContactPatch,
        ContactSourceClass,
    },
    ids::{AccountId, AddressBookId, ProviderKey},
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    ContactDestination, ContactSourceSync, ContactWriteReceipt, ContactsProvider, ProviderError,
    ProviderResult, ScopeSync, WriteGuard,
};
use serde_json::json;
use tokio::sync::Mutex;

use super::{
    CollectionKey, EasAdapter,
    contacts_write::{create_card, delete_card, patch_card},
    email::{MAX_WINDOW_SIZE, should_follow_empty_bootstrap},
    error::{provider_error, sync_status_error},
    hierarchy::Container,
    mailboxes::{BOOTSTRAP_KEY, next_key, request_key},
};
use crate::{
    client::EasClient,
    contacts::contact_card_from_props,
    status::{RecoveryAction, recovery_action_for_sync},
    types::{EasFolder, SyncRequest},
};

/// The adapter's extended-property namespace (every slice's convention).
const EXTENDED_NAMESPACE: &str = "eas";

// ============================================================================
// sync_address_books — FolderSync, Contacts class
// ============================================================================

/// One FolderSync pass over the contact containers: the shared hierarchy
/// ledger drives the round (see `hierarchy.rs`); this slice only maps the
/// resulting wire rows onto `ContactSourceSync<AddressBook>`.
pub(super) async fn sync_address_books(
    client: &Mutex<EasClient>,
    ledger: &super::hierarchy::HierarchyLedger,
    cursor: Option<&SyncState>,
) -> ProviderResult<ContactSourceSync<AddressBook>> {
    let round = ledger.pass(client, cursor, Container::Contacts).await?;
    Ok(ContactSourceSync::Available {
        sync: books_scope_sync(&round),
        cursor_recovered: round.recovered,
    })
}

/// Maps one ledger round into a `ScopeSync<AddressBook>` — a snapshot
/// when the round carries present-set authority, a delta otherwise.
fn books_scope_sync(round: &super::hierarchy::HierarchyRound) -> ScopeSync<AddressBook> {
    let next_cursor = SyncState::new(round.next_key.as_str());
    if let Some(present_rows) = &round.present_rows {
        let objects = address_books(present_rows);
        let present: BTreeSet<ProviderKey> =
            objects.iter().map(|book| book.id.key().clone()).collect();
        ScopeSync::new(SyncUpdate::snapshot(objects, present), next_cursor)
    } else {
        let changed = address_books(&round.folders);
        let removed: Vec<ProviderKey> = round
            .deletions
            .iter()
            .filter(|id| !id.is_empty())
            .filter_map(|id| ProviderKey::new(id.clone()).ok())
            .collect();
        ScopeSync::new(SyncUpdate::delta(changed, removed), next_cursor)
    }
}

/// The wire's Add/Update folders that belong to the contacts container
/// scope: folder Type 9 (the default contacts folder) and Type 14
/// (user-created contact folders) both parse to class `"Contacts"`
/// ([MS-ASFD] — the classless shape a missing Type element parses to is
/// mail by the mail slice's default, so it is excluded here).
fn address_books(changes: &[EasFolder]) -> Vec<AddressBook> {
    changes
        .iter()
        .filter(|folder| folder.class == "Contacts")
        .filter_map(|folder| {
            let Ok(id) = AddressBookId::try_from(folder.server_id.as_str()) else {
                log::warn!(
                    "FolderSync contact folder {:?} cannot key an address book; skipping it",
                    folder.server_id
                );
                return None;
            };
            let mut book = AddressBook::new(
                id,
                folder.display_name.clone(),
                ContactSourceClass::Personal,
            );
            // The class holds the account owner's own folders: EAS exposes
            // no per-folder privilege to ask about, so they are writable.
            book.is_writable = true;
            // Folder Type 9 IS the default contacts folder (14 is the
            // user-created one — the mail 1/12 pair's contacts twin).
            book.is_default = folder.folder_type == Some(9);
            book.extended
                .set(format!("{EXTENDED_NAMESPACE}/class"), json!(folder.class));
            if let Some(folder_type) = folder.folder_type {
                book.extended.set(
                    format!("{EXTENDED_NAMESPACE}/folder-type"),
                    json!(folder_type),
                );
            }
            Some(book)
        })
        .collect()
}

// ============================================================================
// sync_contacts — Sync class "Contacts"
// ============================================================================

/// One whole-scope card pass over the bound contact folder: pages
/// `MoreAvailable` rounds to the end, converts every item through the
/// read-side seam, and recovers a SyncKey invalidation by re-bootstrapping
/// once as a snapshot (see the module docs).
pub(super) async fn sync_contacts(
    client: &Mutex<EasClient>,
    book: &AddressBookId,
    ledger: &CollectionKey,
    cursor: Option<&SyncState>,
) -> ProviderResult<ContactSourceSync<ContactCard>> {
    let mut client = client.lock().await;
    let mut key = request_key(cursor).to_owned();
    let mut snapshot = key == BOOTSTRAP_KEY;
    let mut recovered = false;
    let mut changed: Vec<ContactCard> = Vec::new();
    let mut removed: Vec<ProviderKey> = Vec::new();
    let mut present: BTreeSet<ProviderKey> = BTreeSet::new();
    loop {
        let result = client
            .sync(&SyncRequest {
                collection_id: book.as_str().to_owned(),
                sync_key: key.clone(),
                // Routes the response parser to the Contacts-shaped path.
                class: "Contacts".to_owned(),
                window_size: MAX_WINDOW_SIZE,
                filter_age_days: 0,
                fetch_body: false,
                truncation_size: None,
                mime_support: None,
                mime_truncation: None,
                supported: None,
            })
            .await
            .map_err(provider_error)?;
        match recovery_action_for_sync(result.status) {
            RecoveryAction::Ok => {}
            // The stored key is dead: discard the accumulated rounds and
            // restart from "0" as a snapshot, exactly once — the
            // `sync_events` recovery verbatim (nothing has been handed to
            // the caller yet; a dead key that IS the bootstrap key falls
            // through to the surface arm below).
            RecoveryAction::ResetSyncKey | RecoveryAction::RunFolderSync if !snapshot => {
                key.clear();
                key.push_str(BOOTSTRAP_KEY);
                snapshot = true;
                recovered = true;
                changed.clear();
                removed.clear();
                present.clear();
                continue;
            }
            _ => return Err(sync_status_error(result.status)),
        }

        let next = next_key(&result.sync_key, &key).to_owned();
        let mut round_items = 0usize;
        for item in result.contacts_added.iter().chain(&result.contacts_updated) {
            if item.server_id.is_empty() {
                log::warn!(
                    "Sync contact item without a ServerId in collection {}; skipping it",
                    book.as_str()
                );
                continue;
            }
            let card = contact_card_from_props(book, &item.server_id, &item.props);
            present.insert(card.id.key().clone());
            changed.push(card);
            round_items += 1;
        }
        if !snapshot {
            let deletes: Vec<ProviderKey> = result
                .deleted_server_ids
                .iter()
                .filter(|id| !id.is_empty())
                .filter_map(|id| ProviderKey::new(id.clone()).ok())
                .collect();
            round_items += deletes.len();
            removed.extend(deletes);
        }

        let follow = should_follow_empty_bootstrap(&key, round_items, result.more_available, &next);
        let stalled = result.more_available && next == key && round_items == 0;
        if !result.more_available && !follow || stalled {
            let update = if snapshot {
                SyncUpdate::snapshot(changed, present)
            } else {
                SyncUpdate::delta(changed, removed)
            };
            // The pass completed cleanly at `next` — record it as the
            // contacts ledger's key (identical to the cursor the engine
            // persists — one fact) for the write verbs to ride.
            *ledger.lock().expect("collection-key ledger") = Some(next.clone());
            return Ok(ContactSourceSync::Available {
                sync: ScopeSync::new(update, SyncState::new(next)),
                cursor_recovered: recovered,
            });
        }
        key = next;
    }
}

/// caller-facing refusal when the adapter was built without one.
pub(super) fn unbound_contacts() -> ProviderError {
    ProviderError::invalid_state(
        "this EasAdapter is not contacts-bound — build it with \
         EasAdapter::with_contacts before syncing cards",
    )
}

/// The destination's exact neutral field set: everything the write seam
/// can represent. `Kind` is absent by design — the EAS contacts class is
/// individual-only with no kind element (a kind patch of Individual is
/// the no-op identity, anything else refuses).
fn supported_fields() -> ContactFieldSet {
    ContactFieldSet::from_fields([
        ContactField::Name,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Addresses,
        ContactField::Organizations,
        ContactField::Titles,
        ContactField::Notes,
        ContactField::Urls,
        ContactField::Anniversaries,
    ])
}

// ============================================================================
// The trait half
// ============================================================================

#[async_trait::async_trait]
impl ContactsProvider for EasAdapter {
    /// FolderSync contact folders claim their own per-account container
    /// scope, before the per-[`EasContact`](SyncScope::EasContact) cards
    /// they parent — the calendar split's twin.
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::EasContactList {
            account: account.clone(),
        }
    }

    /// EAS item `Sync` is per collection, so card sync is per contact
    /// folder — [`SyncScope::EasContact`] keyed by the bound folder's
    /// ServerId, the Graph `GraphContacts` / CalDAV `CardDavAddressBook`
    /// binding precedent. Without a binding
    /// ([`EasAdapter::with_contacts`]) the default JMAP shape stands —
    /// never consulted, since an unbound adapter's capabilities do not
    /// advertise the family.
    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        match &self.address_book {
            Some(book) => SyncScope::EasContact {
                account: account.clone(),
                address_book: book.clone(),
            },
            None => SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::ContactCard,
            },
        }
    }

    /// The bound folder as the write destination: personal, writable, and
    /// honestly unguarded — EAS Sync Change carries no revision tokens
    /// (the `with_calendar` ruling).
    fn contact_destination(&self) -> Option<ContactDestination> {
        self.address_book.as_ref().map(|book| ContactDestination {
            address_book: book.clone(),
            source_class: ContactSourceClass::Personal,
            writable: true,
            write_guard: Some(WriteGuard::Absent),
            supported_fields: supported_fields(),
        })
    }

    /// FolderSync filtered to the Contacts class (folder Type 9), through
    /// the shared hierarchy ledger — `sync_address_books` above owns the
    /// mapping and its contract.
    async fn sync_address_books(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        sync_address_books(&self.client, &self.hierarchy, cursor).await
    }

    /// Sync class "Contacts" over the bound folder — `sync_contacts`
    /// above owns the mapping and its contract. Requires the contacts
    /// binding; an unbound adapter refuses `InvalidState`, and its
    /// capabilities never advertise the family.
    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        match &self.address_book {
            Some(book) => sync_contacts(&self.client, book, &self.contacts_key, cursor).await,
            None => Err(unbound_contacts()),
        }
    }

    /// Sync `Add` with a synthesized ClientId — `create` above owns the
    /// mapping. Requires the contacts binding.
    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        match &self.address_book {
            Some(book) => create_card(&self.client, book, &self.contacts_key, draft).await,
            None => Err(unbound_contacts()),
        }
    }

    /// Sync `Change` carrying only the patched families (the ghost
    /// model); an empty patch is a no-op receipt. `patch` above owns the
    /// mapping. Requires the contacts binding.
    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        match &self.address_book {
            Some(book) => patch_card(&self.client, book, &self.contacts_key, base, patch).await,
            None => Err(unbound_contacts()),
        }
    }

    /// Sync `Delete` of the ServerId; already-gone is success. `delete`
    /// above owns the mapping. Requires the contacts binding.
    async fn delete_contact(&self, _account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        match &self.address_book {
            Some(book) => delete_card(&self.client, book, &self.contacts_key, base).await,
            None => Err(unbound_contacts()),
        }
    }
}

#[cfg(test)]
#[path = "contacts_tests.rs"]
mod tests;
