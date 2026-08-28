// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the `edit_mail` slice (`mutate.rs`) — the `#[path]` split
//! the repo uses to hold the 500-line cap (the `email_tests.rs` precedent).
//! The wire-level proofs (the Change's ApplicationData shapes, MoveItems
//! addressing, key threading across edits) live in the harness
//! `adapter_edit_flow` scenarios; these pin the pure mapping and
//! ledger rules.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    mail::{Keyword, SystemKeyword},
};

use super::*;
use crate::types::EasItem;

fn target() -> ProviderKey {
    ProviderKey::new("sid:7").unwrap()
}

fn set(keywords: &[Keyword]) -> BTreeSet<Keyword> {
    keywords.iter().cloned().collect()
}

/// The keyword → wire mapping: set/clear each of the two expressible
/// keywords, both at once, neither (the no-op), and the refusal for
/// anything else.
#[test]
fn keyword_edits_map_onto_the_wire_change() {
    let seen = Keyword::system(SystemKeyword::Seen);
    let flagged = Keyword::system(SystemKeyword::Flagged);

    let change = keyword_change(
        &target(),
        &set(std::slice::from_ref(&seen)),
        &BTreeSet::new(),
    )
    .expect("mark-read maps")
    .expect("an expressible edit is a change");
    assert_eq!(change.server_id, "sid:7");
    assert_eq!(change.read, Some(true));
    assert_eq!(change.starred, None);

    let change = keyword_change(
        &target(),
        &BTreeSet::new(),
        &set(std::slice::from_ref(&flagged)),
    )
    .expect("unflag maps")
    .expect("an expressible edit is a change");
    assert_eq!(change.read, None);
    assert_eq!(change.starred, Some(false));

    let change = keyword_change(&target(), &set(&[seen, flagged]), &BTreeSet::new())
        .expect("both keywords map")
        .expect("an expressible edit is a change");
    assert_eq!(change.read, Some(true));
    assert_eq!(change.starred, Some(true));

    assert!(
        keyword_change(&target(), &BTreeSet::new(), &BTreeSet::new())
            .expect("the empty edit is not an error")
            .is_none(),
        "nothing expressible changed — the no-op direction"
    );
}

/// The vocabulary gate: a keyword with no wire element is refused
/// permanently BEFORE any round-trip — the IMAP PERMANENTFLAGS spirit.
#[test]
fn keywords_beyond_read_and_flag_are_refused() {
    for keyword in [
        Keyword::system(SystemKeyword::Draft),
        Keyword::system(SystemKeyword::Answered),
        Keyword::new("$forwarded").unwrap(),
    ] {
        let err = keyword_change(&target(), &set(&[keyword]), &BTreeSet::new())
            .expect_err("no wire form exists");
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(
            err.detail().contains("read and flagged"),
            "the refusal names the vocabulary: {}",
            err.detail()
        );
        let err = keyword_change(
            &target(),
            &BTreeSet::new(),
            &set(&[Keyword::new("$forwarded").unwrap()]),
        )
        .expect_err("the remove side refuses equally");
        assert_eq!(err.class(), FailureClass::Permanent);
    }
}

/// The ledger's read rule: a seeded key rides; a cold ledger refuses
/// NeedsResync — never a guess.
#[test]
fn the_ledger_seeds_edits_and_refuses_when_cold() {
    let ledger = CollectionKey::default();
    let err = current_key(&ledger).expect_err("cold ledger refuses");
    assert_eq!(err.class(), FailureClass::NeedsResync);
    assert!(
        err.detail().contains("sync pass"),
        "the refusal says how to recover: {}",
        err.detail()
    );

    *ledger.lock().unwrap() = Some("c2".to_owned());
    assert_eq!(current_key(&ledger).expect("seeded ledger rides"), "c2");
}

/// The rotation record: a clean outcome advances the ledger to the rotated
/// key; a piggybacked response drops it (those rows cannot ride the
/// receipt, so the next pass must reconcile instead of skip).
#[test]
fn rotations_advance_the_ledger_except_piggybacked_responses() {
    let ledger = CollectionKey::default();
    let clean = SyncChangeOutcome {
        new_key: "c3".to_owned(),
        ..Default::default()
    };
    record_rotation(&ledger, &clean);
    assert_eq!(*ledger.lock().unwrap(), Some("c3".to_owned()));

    let piggybacked = SyncChangeOutcome {
        new_key: "c4".to_owned(),
        piggybacked_added: vec![EasItem::default()],
        ..Default::default()
    };
    record_rotation(&ledger, &piggybacked);
    assert!(
        ledger.lock().unwrap().is_none(),
        "a piggybacked response drops the ledger — the rows cannot be returned"
    );
}

/// The Delete refusal detail names the MoveTo alternative — the adapter
/// policy the module docs record. The refusal precedes any wire work, so
/// an offline-configured client stands in for the transport.
#[tokio::test]
async fn delete_names_the_move_to_alternative() {
    let client = EasClient::new(
        crate::types::EasConfig::default(),
        &engine_tls::TlsClientConfig::bundled(),
    )
    .expect("offline client builds");
    let err = edit(
        &Mutex::new(client),
        &MailboxId::try_from("fid-inbox").unwrap(),
        &CollectionKey::default(),
        &MailEdit::delete(target()),
    )
    .await
    .expect_err("EAS cannot hard-delete one item");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        err.detail().contains("MoveTo"),
        "the refusal points at the move: {}",
        err.detail()
    );
}
