//! Ordering the account's folder fan-out Inbox-first.
//!
//! Split from [`crate::mail_account`] — the fan-out this orders — so the ordering decision
//! (a pure scope classification) and the tests that pin it live in their own module.

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole},
    sync::{ObjectKind, SyncScope},
};
use engine_provider::Provider;
use engine_store::StoreRead;

/// Indices into `providers`, with the Inbox's first.
///
/// The Inbox is the folder the user is looking at, so filling it first is the difference between
/// a list that populates while the rest downloads and one that waits on whichever folder the
/// provider happened to hand over first. Everything else keeps its given order.
///
/// A provider whose scope names no mailbox (JMAP, Gmail, Graph — one provider for the account)
/// cannot be ordered against the others and does not need to be: there is only one.
pub(crate) fn inbox_first<P: Provider>(
    account: &AccountId,
    providers: &[P],
    inbox: Option<&MailboxId>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..providers.len()).collect();
    let Some(inbox) = inbox else {
        return order;
    };
    // Stable, so everything that is not the Inbox keeps the order the caller gave.
    order.sort_by_key(|&index| !names_mailbox(&providers[index].email_scope(account), inbox));
    order
}

/// The account's Inbox, from the folder list the pass has just synced.
pub(crate) async fn stored_inbox<S: StoreRead>(
    store: &S,
    account: &AccountId,
) -> Option<MailboxId> {
    let scopes = store.account_scopes(account.clone()).await.ok()?;
    for scope in scopes
        .into_iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Mailbox))
    {
        for (_key, payload) in store.scope_objects(&scope).await.ok()? {
            if let Ok(mailbox) = serde_json::from_value::<Mailbox>(payload)
                && mailbox.role == Some(MailboxRole::Inbox)
            {
                return Some(mailbox.id);
            }
        }
    }
    None
}

/// Whether a mail scope is the one for `mailbox`.
fn names_mailbox(scope: &SyncScope, mailbox: &MailboxId) -> bool {
    match scope {
        SyncScope::ImapMailbox { mailbox: named, .. }
        | SyncScope::GraphFolder { folder: named, .. }
        | SyncScope::EasFolder { folder: named, .. } => named == mailbox,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use engine_core::ids::MailboxId;

    use super::*;

    fn imap(account: &AccountId, mailbox: &str) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: MailboxId::try_from(mailbox).unwrap(),
        }
    }

    /// The ordering decision itself, which is what the engine actually promises.
    ///
    /// Deliberately not asserted through a real pass: the folders then run concurrently and the
    /// order they *reach the network* is the order their lease claims resolve in, which is the
    /// scheduler's business and not a guarantee the engine makes or should be tested for. What it
    /// promises is where the Inbox sits in the queue it hands to the fan-out.
    #[test]
    fn the_inbox_is_ordered_first_and_the_rest_keep_their_order() {
        let account = AccountId::try_from("acct").unwrap();
        let inbox = MailboxId::try_from("INBOX").unwrap();
        let scopes = [
            imap(&account, "Archive"),
            imap(&account, "INBOX"),
            imap(&account, "Projects"),
        ];

        let mut order: Vec<usize> = (0..scopes.len()).collect();
        order.sort_by_key(|&index| !names_mailbox(&scopes[index], &inbox));

        assert_eq!(order, vec![1, 0, 2], "Inbox first, the others as given");
    }

    #[test]
    fn a_scope_that_names_no_mailbox_is_never_the_inbox() {
        // JMAP, Gmail and Graph give one provider for the whole account, so there is nothing to
        // order and nothing that could match.
        let account = AccountId::try_from("acct").unwrap();
        let jmap = SyncScope::JmapType {
            account,
            data_type: engine_core::sync::JmapDataType::Email,
        };
        assert!(!names_mailbox(
            &jmap,
            &MailboxId::try_from("INBOX").unwrap()
        ));
    }

    #[test]
    fn an_eas_folder_scope_names_its_folder() {
        // EAS, like IMAP and Graph mail, is one provider per folder, so the folder its scope
        // names is the one the Inbox-first ordering must sort on. EAS ServerIds are opaque
        // per-device-partnership strings.
        let account = AccountId::try_from("acct").unwrap();
        let eas = |folder: &str| SyncScope::EasFolder {
            account: account.clone(),
            folder: MailboxId::try_from(folder).unwrap(),
        };
        let inbox = MailboxId::try_from("1").unwrap();
        assert!(names_mailbox(&eas("1"), &inbox));
        assert!(!names_mailbox(&eas("5"), &inbox));
    }
}
