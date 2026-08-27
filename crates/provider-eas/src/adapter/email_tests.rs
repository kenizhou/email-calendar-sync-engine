// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the `stream_email` slice (`email.rs`) — the `#[path]
//! split the repo uses to hold the 500-line cap (the `stream_tests.rs`
//! precedent).

use super::*;

fn item(id: &str) -> EasItem {
    EasItem {
        server_id: id.to_owned(),
        ..EasItem::default()
    }
}

fn engine_message(id: &str) -> Message {
    Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("fid-inbox").unwrap()),
    )
}

#[test]
fn zero_fetch_batch_is_the_adapter_maximum() {
    assert_eq!(window_size(0), MAX_WINDOW_SIZE);
    assert_eq!(window_size(25), 25);
    assert_eq!(window_size(usize::MAX), MAX_WINDOW_SIZE);
}

/// The Kylins pins for the Exchange 15.2 bootstrap quirk, verbatim: the
/// follow fires only for an empty no-more bootstrap round with a usable
/// rotated key, and cannot re-fire (the follow-up's key is not "0").
#[test]
fn empty_bootstrap_is_followed_exactly_once() {
    assert!(
        should_follow_empty_bootstrap("0", 0, false, "col-1"),
        "the quirk shape: bootstrap, empty, nothing more, usable key"
    );
    assert!(
        !should_follow_empty_bootstrap("0", 0, false, "0"),
        "no usable key to follow"
    );
    assert!(
        !should_follow_empty_bootstrap("0", 0, false, ""),
        "an empty key would poison the cursor"
    );
    assert!(
        !should_follow_empty_bootstrap("0", 3, false, "col-1"),
        "items came back — no follow needed"
    );
    assert!(
        !should_follow_empty_bootstrap("0", 0, true, "col-1"),
        "MoreAvailable already keeps the loop going"
    );
    assert!(
        !should_follow_empty_bootstrap("col-1", 0, false, "col-2"),
        "only a bootstrap round qualifies — loop safety"
    );
}

#[test]
fn a_round_of_one_chunk_carries_the_whole_delta() {
    let chunks = additive_round_chunks(
        vec![engine_message("sid:1"), engine_message("sid:2")],
        vec![ProviderKey::new("sid:9").unwrap()],
        0,
        "col-2",
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].mode, PassMode::Additive);
    assert_eq!(chunks[0].changed.len(), 2);
    assert_eq!(chunks[0].removed.len(), 1);
    assert_eq!(
        chunks[0].advance_to.as_ref().map(SyncState::as_str),
        Some("col-2")
    );
}

/// The sub-chunk contract: only the completing chunk may checkpoint the
/// round's key — committing it before the round's rows are stored would
/// skip them on a crash resume (the server considers the page delivered
/// at the rotated key).
#[test]
fn wide_rounds_hold_the_cursor_until_the_completing_chunk() {
    let chunks = additive_round_chunks(
        vec![
            engine_message("sid:1"),
            engine_message("sid:2"),
            engine_message("sid:3"),
        ],
        vec![ProviderKey::new("sid:9").unwrap()],
        2,
        "col-2",
    );
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].changed.len(), 2);
    assert_eq!(chunks[0].advance_to, None, "the sub-chunk holds");
    assert!(chunks[0].removed.is_empty(), "removals ride the completion");
    assert_eq!(chunks[1].changed.len(), 1);
    assert_eq!(chunks[1].removed.len(), 1);
    assert_eq!(
        chunks[1].advance_to.as_ref().map(SyncState::as_str),
        Some("col-2")
    );
}

#[test]
fn an_empty_round_still_checkpoints() {
    let chunks = additive_round_chunks(Vec::new(), Vec::new(), 5, "col-7");
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].changed.is_empty());
    assert_eq!(
        chunks[0].advance_to.as_ref().map(SyncState::as_str),
        Some("col-7")
    );
}

#[test]
fn wire_flags_map_to_system_keywords_and_native_facts_survive() {
    let folder = MailboxId::try_from("fid-inbox").unwrap();
    let wire = EasItem {
        server_id: "sid:1".to_owned(),
        subject: Some("Hello".to_owned()),
        from: Some("alice@example.test".to_owned()),
        read: Some(true),
        flag: Some(true),
        is_draft: Some(true),
        importance: Some(2),
        message_class: Some("IPM.Note".to_owned()),
        meeting_message_type: Some(1),
        ..EasItem::default()
    };
    let mapped = message(&wire, &folder).expect("mapping succeeds");
    assert!(mapped.has_system_keyword(SystemKeyword::Seen));
    assert!(mapped.has_system_keyword(SystemKeyword::Flagged));
    assert!(mapped.has_system_keyword(SystemKeyword::Draft));
    assert_eq!(mapped.envelope.subject.as_deref(), Some("Hello"));
    assert_eq!(
        mapped.envelope.from.first().map(|a| a.email.as_str()),
        Some("alice@example.test")
    );
    assert_eq!(
        mapped.extended.get("eas/importance"),
        Some(&json!(2u8)),
        "the native facts survive under the adapter namespace"
    );
    assert_eq!(
        mapped.extended.get("eas/meeting-message-type"),
        Some(&json!(1u8))
    );
    // Exactly the system keywords the wire flags set — nothing inferred
    // beyond them.
    let expected = [
        Keyword::system(SystemKeyword::Seen),
        Keyword::system(SystemKeyword::Flagged),
        Keyword::system(SystemKeyword::Draft),
    ]
    .into_iter()
    .collect();
    assert_eq!(mapped.keywords, expected);
}

#[test]
fn an_unreadable_timestamp_and_unflagged_items_stay_lenient() {
    let folder = MailboxId::try_from("fid-inbox").unwrap();
    let wire = EasItem {
        server_id: "sid:2".to_owned(),
        date_received: Some("not-a-timestamp".to_owned()),
        read: Some(false),
        ..EasItem::default()
    };
    let mapped = message(&wire, &folder).expect("mapping succeeds");
    assert_eq!(mapped.received_at, None, "undated, never an error");
    assert!(!mapped.has_system_keyword(SystemKeyword::Seen));
}

#[test]
fn an_item_without_a_server_id_is_permanent() {
    let folder = MailboxId::try_from("fid-inbox").unwrap();
    let err = message(&item(""), &folder).expect_err("an empty ServerId cannot key a Message");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}
