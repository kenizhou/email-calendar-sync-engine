//! The event contract's wire shape, pinned to the byte: externally tagged,
//! snake_case tags and fields, `None` payload options *omitted* rather than nulled
//! — plus the collector the assertions (and the round tests after them) observe
//! emissions through. The exact strings below are the JSON the Kylins shell's
//! TypeScript mirror (P1 T8) copies, so a serde attribute change that moves one
//! byte of them breaks that mirror — which is precisely what these tests exist to
//! catch before it can.

use super::*;

/// The event as the wire carries it: one compact JSON string, field for field.
fn wire(event: &EngineEvent) -> String {
    serde_json::to_string(event).unwrap()
}

/// The wire text read back must equal the event that produced it. Because `None`
/// options serialize as absence, each call also proves the *omitted* form
/// deserializes back into the same `None` — the round trip is not just equality
/// of `Some` payloads.
fn unwires(text: &str, event: &EngineEvent) {
    assert_eq!(&serde_json::from_str::<EngineEvent>(text).unwrap(), event);
}

#[test]
fn a_commit_serializes_externally_tagged_with_snake_case_fields() {
    let event = EngineEvent::Commit {
        account: "acct-1".to_owned(),
        folder: "INBOX".to_owned(),
        upserted: vec!["m-1".to_owned(), "m-2".to_owned()],
        removed: vec!["m-9".to_owned()],
        fetched: 3,
        total: Some(42),
    };
    // The tag is "commit", the fields snake_case in declaration order, `total`
    // present because it is `Some`.
    assert_eq!(
        wire(&event),
        r#"{"commit":{"account":"acct-1","folder":"INBOX","upserted":["m-1","m-2"],"removed":["m-9"],"fetched":3,"total":42}}"#
    );
    unwires(
        r#"{"commit":{"account":"acct-1","folder":"INBOX","upserted":["m-1","m-2"],"removed":["m-9"],"fetched":3,"total":42}}"#,
        &event,
    );

    // `total: None` is omitted — absent from the object, not sent as `null`.
    let unbounded = EngineEvent::Commit {
        account: "acct-1".to_owned(),
        folder: "Drafts".to_owned(),
        upserted: Vec::new(),
        removed: Vec::new(),
        fetched: 0,
        total: None,
    };
    assert_eq!(
        wire(&unbounded),
        r#"{"commit":{"account":"acct-1","folder":"Drafts","upserted":[],"removed":[],"fetched":0}}"#
    );
    unwires(
        r#"{"commit":{"account":"acct-1","folder":"Drafts","upserted":[],"removed":[],"fetched":0}}"#,
        &unbounded,
    );
}

#[test]
fn an_account_status_serializes_its_state_as_one_snake_case_word() {
    // Each state travels as exactly one word, in both directions.
    for (state, word) in [
        (AccountState::Idle, "idle"),
        (AccountState::Syncing, "syncing"),
        (AccountState::Error, "error"),
        (AccountState::RateLimited, "rate_limited"),
    ] {
        let word = format!("\"{word}\"");
        assert_eq!(serde_json::to_string(&state).unwrap(), word);
        assert_eq!(
            serde_json::from_str::<AccountState>(&word).unwrap(),
            state,
            "{word} reads back as the state that wrote it"
        );
    }

    // `detail: None` is omitted from the object; `Some` rides as a bare number.
    let syncing = EngineEvent::AccountStatus {
        account: "acct-1".to_owned(),
        state: AccountState::Syncing,
        detail: None,
    };
    assert_eq!(
        wire(&syncing),
        r#"{"account_status":{"account":"acct-1","state":"syncing"}}"#
    );
    unwires(
        r#"{"account_status":{"account":"acct-1","state":"syncing"}}"#,
        &syncing,
    );

    let throttled = EngineEvent::AccountStatus {
        account: "acct-1".to_owned(),
        state: AccountState::RateLimited,
        detail: Some(37),
    };
    assert_eq!(
        wire(&throttled),
        r#"{"account_status":{"account":"acct-1","state":"rate_limited","detail":37}}"#
    );
    unwires(
        r#"{"account_status":{"account":"acct-1","state":"rate_limited","detail":37}}"#,
        &throttled,
    );
}

#[test]
fn an_outbox_changed_serializes_its_pending_count() {
    let event = EngineEvent::OutboxChanged {
        account: "acct-1".to_owned(),
        pending: 2,
    };
    assert_eq!(
        wire(&event),
        r#"{"outbox_changed":{"account":"acct-1","pending":2}}"#
    );
    unwires(
        r#"{"outbox_changed":{"account":"acct-1","pending":2}}"#,
        &event,
    );
}

#[test]
fn a_send_result_serializes_success_and_its_optional_detail() {
    // A success carries no `detail` key at all.
    let sent = EngineEvent::SendResult {
        account: "acct-1".to_owned(),
        message_id: "m-1".to_owned(),
        success: true,
        detail: None,
    };
    assert_eq!(
        wire(&sent),
        r#"{"send_result":{"account":"acct-1","message_id":"m-1","success":true}}"#
    );
    unwires(
        r#"{"send_result":{"account":"acct-1","message_id":"m-1","success":true}}"#,
        &sent,
    );

    // A failure carries its reason.
    let bounced = EngineEvent::SendResult {
        account: "acct-1".to_owned(),
        message_id: "m-2".to_owned(),
        success: false,
        detail: Some("554 rejected".to_owned()),
    };
    assert_eq!(
        wire(&bounced),
        r#"{"send_result":{"account":"acct-1","message_id":"m-2","success":false,"detail":"554 rejected"}}"#
    );
    unwires(
        r#"{"send_result":{"account":"acct-1","message_id":"m-2","success":false,"detail":"554 rejected"}}"#,
        &bounced,
    );
}

#[test]
fn a_calendar_changed_serializes_its_account() {
    let event = EngineEvent::CalendarChanged {
        account: "acct-1".to_owned(),
    };
    assert_eq!(wire(&event), r#"{"calendar_changed":{"account":"acct-1"}}"#);
    unwires(r#"{"calendar_changed":{"account":"acct-1"}}"#, &event);
}

#[test]
fn a_contacts_changed_serializes_its_account() {
    let event = EngineEvent::ContactsChanged {
        account: "acct-1".to_owned(),
    };
    assert_eq!(wire(&event), r#"{"contacts_changed":{"account":"acct-1"}}"#);
    unwires(r#"{"contacts_changed":{"account":"acct-1"}}"#, &event);
}

#[test]
fn a_collecting_sink_records_in_order_snapshots_and_clears() {
    let sink = CollectingSink::default();
    assert!(sink.events().is_empty(), "a fresh sink has heard nothing");

    let queued = EngineEvent::OutboxChanged {
        account: "acct-1".to_owned(),
        pending: 1,
    };
    let settled = EngineEvent::AccountStatus {
        account: "acct-1".to_owned(),
        state: AccountState::Idle,
        detail: None,
    };
    sink.emit(queued.clone());
    sink.emit(settled.clone());
    assert_eq!(
        sink.events(),
        vec![queued, settled],
        "the record is the emissions, in order"
    );

    // `events` is a snapshot: what a caller does with the copy cannot rewrite
    // what the sink heard.
    let mut taken = sink.events();
    taken.clear();
    assert_eq!(sink.events().len(), 2, "the sink keeps its own copy");

    sink.clear();
    assert!(sink.events().is_empty(), "clear forgets everything heard");
}

#[test]
fn a_sink_hears_through_its_trait_object() {
    // Compile-time, stated up front: emitting sides hold the sink as a bare
    // trait object and hand it across threads — the trait's Send + Sync
    // supertraits say they may.
    fn shareable<T: Send + Sync + ?Sized>() {}

    let sink = CollectingSink::default();
    let ear: &dyn EventSink = &sink;
    ear.emit(EngineEvent::OutboxChanged {
        account: "acct-1".to_owned(),
        pending: 0,
    });
    assert_eq!(sink.events().len(), 1, "the dyn call reached the collector");
    shareable::<dyn EventSink>();
    shareable::<CollectingSink>();
}
