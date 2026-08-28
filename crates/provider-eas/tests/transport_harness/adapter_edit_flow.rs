// SPDX-License-Identifier: MPL-2.0
//! Adapter `edit_mail` scenarios: the three trait verbs over their EAS
//! commands — `SetKeywords` rides a `Sync` Commands Change (the collection
//! SyncKey sourced from the adapter's key ledger), `MoveTo` rides
//! `MoveItems` with the bound folder as the source collection, and
//! `Delete` is refused (EAS has no per-item hard delete). The wire shapes
//! are pinned per verb: the Change's `ApplicationData` (Read / Flag), the
//! Move's SrcMsgId/SrcFldId/DstFldId triple, and the key threading across
//! consecutive edits.

use std::{collections::BTreeSet, sync::Arc};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, SystemKeyword},
    sync::SyncWindow,
};
use engine_provider::{EmailChunk, EmailStream, MailEdit, Provider as _};
use futures_util::StreamExt;
use provider_eas::{
    adapter::EasAdapter,
    commands::{AS_CHANGE, AS_COMMANDS, AS_SERVER_ID, AS_SYNC_KEY, PAGE_AIRSYNC},
    wbxml::{
        WbxmlElement,
        tags::{email, pages},
    },
};

use super::{
    adapter_email_wire::{request_field, sync_round},
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// MoveItems wire tokens (page `pages::MOVE`): the Move container and its
/// three addressing children ([MS-ASCMD] §2.2.3.119) — the `items_flow`
/// literal-token convention.
const MV_MOVE: u8 = 0x06;
const MV_SRC_MSG_ID: u8 = 0x07;
const MV_SRC_FLD_ID: u8 = 0x08;
const MV_DST_FLD_ID: u8 = 0x09;

fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

fn folder() -> MailboxId {
    MailboxId::try_from("fid-inbox").unwrap()
}

fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

fn target() -> ProviderKey {
    ProviderKey::new("sid:7").unwrap()
}

async fn drain(mut stream: EmailStream<'_>) -> Vec<EmailChunk> {
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.expect("chunk"));
    }
    chunks
}

/// Recursive element lookup by code-page + token — the wire-walk every
/// scenario's request assertion rides.
fn find_el(el: &WbxmlElement, page: u8, token: u8) -> Option<&WbxmlElement> {
    if el.page == page && el.token == token {
        return Some(el);
    }
    el.children.iter().find_map(|c| find_el(c, page, token))
}

/// Drives one full `stream_email` pass against the two-round bootstrap the
/// mock serves (round 1: the empty bootstrap with a rotated key — the
/// Exchange 15.2 quirk the stream follows; round 2: the empty steady
/// round), leaving the adapter's collection-key ledger at `"c2"`.
async fn seed_ledger(adapter: &EasAdapter) {
    let chunks = drain(adapter.stream_email(&account(), None, SyncWindow::full(), 25, 0)).await;
    assert!(
        chunks.last().is_some_and(|c| c.advance_to.is_some()),
        "the seeding pass completed and checkpointed a key"
    );
}

/// The `Commands > Change` element of a Sync upsync request — the verb this
/// file pins.
fn change_element(req: &CapturedRequest) -> WbxmlElement {
    let tree = req.wbxml_tree().expect("request decodes");
    find_el(&tree, PAGE_AIRSYNC, AS_COMMANDS)
        .and_then(|commands| find_el(commands, PAGE_AIRSYNC, AS_CHANGE))
        .cloned()
        .expect("a Change command rides the Commands element")
}

/// (a) The cold-ledger refusal: a fresh adapter has not observed the
/// collection's SyncKey (the trait's `edit_mail` carries no cursor), so the
/// edit refuses `NeedsResync` BEFORE any wire round — the orchestrator
/// re-syncs, the pass seeds the ledger, and the outbox retries the op.
#[tokio::test]
async fn a_cold_ledger_refuses_with_needs_resync() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);

    let err = adapter
        .edit_mail(&account(), &MailEdit::mark_seen(target(), true))
        .await
        .expect_err("no key observed ⇒ no edit");
    assert_eq!(err.class(), FailureClass::NeedsResync);
    assert_eq!(server.count(), 0, "the refusal never touches the wire");
}

/// (b) The happy keyword edit, key-threaded: a pass seeds the ledger at
/// `c2`; the mark-read edit rides `c2` as a Sync Commands Change carrying
/// `Read=1`; its rotated key (`c3`) is the key the NEXT edit rides — the
/// ledger discipline (the trait carries no cursor, so the adapter owns the
/// key the server last handed it).
#[tokio::test]
async fn a_pass_seeds_the_ledger_and_edits_ride_the_rotated_key() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, n: usize| match n {
        1 => MockResponse::wbxml(&sync_round("1", "c1", false, &[], &[], &[])),
        2 => MockResponse::wbxml(&sync_round("1", "c2", false, &[], &[], &[])),
        _ => MockResponse::wbxml(&sync_round("1", "c3", false, &[], &[], &[])),
    }) as Handler);
    let adapter = adapter_at(&server);
    seed_ledger(&adapter).await;
    assert_eq!(server.count(), 2, "the two-round bootstrap seeding");

    let receipt = adapter
        .edit_mail(&account(), &MailEdit::mark_seen(target(), true))
        .await
        .expect("the keyword edit rides the seeded key");
    assert_eq!(
        receipt.message_key.as_str(),
        "sid:7",
        "the receipt names the edited message's key"
    );

    let edit_req = server.request(3);
    assert_eq!(
        request_field(&edit_req, AS_SYNC_KEY),
        "c2",
        "the edit rides the ledger key the pass seeded"
    );
    let change = change_element(&edit_req);
    assert!(
        change.children.iter().any(|c| c.token == AS_SERVER_ID
            && matches!(&c.value, provider_eas::wbxml::WbxmlValue::Text(t) if t == "sid:7")),
        "the Change addresses the target's ServerId"
    );
    let app_data = change
        .children
        .iter()
        .find(|c| c.token == provider_eas::commands::AS_APPLICATION_DATA)
        .expect("ApplicationData is schema-required");
    assert!(
        app_data.children.iter().any(|c| c.page == email::PAGE
            && c.token == email::READ
            && matches!(&c.value, provider_eas::wbxml::WbxmlValue::Text(t) if t == "1")),
        "mark-read upsyncs Read=1"
    );

    // The rotated key threads: the next edit rides c3, and flagging emits
    // the full Flag container (Status 2 + FollowUp + task dates).
    let receipt = adapter
        .edit_mail(&account(), &MailEdit::set_flagged(target(), true))
        .await
        .expect("the follow-up edit rides the rotated key");
    assert_eq!(receipt.message_key.as_str(), "sid:7");
    let edit_req = server.request(4);
    assert_eq!(
        request_field(&edit_req, AS_SYNC_KEY),
        "c3",
        "the ledger advanced to the edit's rotated key"
    );
    let change = change_element(&edit_req);
    let app_data = change
        .children
        .iter()
        .find(|c| c.token == provider_eas::commands::AS_APPLICATION_DATA)
        .expect("ApplicationData present");
    assert!(
        app_data
            .children
            .iter()
            .any(|c| c.page == email::PAGE && c.token == email::FLAG && !c.children.is_empty()),
        "flagging emits the full Flag container"
    );
}

/// (c) The clearing shapes: un-flag emits the EMPTY `<Flag/>` element (the
/// Android form) and un-read emits `Read=0` — neither carries the other
/// element.
#[tokio::test]
async fn clearing_keyword_states_emit_the_clear_wire_shapes() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, n: usize| match n {
        1 => MockResponse::wbxml(&sync_round("1", "c1", false, &[], &[], &[])),
        2 => MockResponse::wbxml(&sync_round("1", "c2", false, &[], &[], &[])),
        _ => MockResponse::wbxml(&sync_round("1", "c3", false, &[], &[], &[])),
    }) as Handler);
    let adapter = adapter_at(&server);
    seed_ledger(&adapter).await;

    adapter
        .edit_mail(&account(), &MailEdit::set_flagged(target(), false))
        .await
        .expect("unflag sends");
    let change = change_element(&server.request(3));
    let app_data = change
        .children
        .iter()
        .find(|c| c.token == provider_eas::commands::AS_APPLICATION_DATA)
        .expect("ApplicationData present");
    let flag = app_data
        .children
        .iter()
        .find(|c| c.page == email::PAGE && c.token == email::FLAG)
        .expect("the empty Flag element is present");
    assert!(
        flag.children.is_empty(),
        "clearing a flag is the empty <Flag/> element"
    );
    assert!(
        !app_data.children.iter().any(|c| c.token == email::READ),
        "an unflag edit carries no Read element"
    );

    adapter
        .edit_mail(&account(), &MailEdit::mark_seen(target(), false))
        .await
        .expect("unread sends");
    let change = change_element(&server.request(4));
    let app_data = change
        .children
        .iter()
        .find(|c| c.token == provider_eas::commands::AS_APPLICATION_DATA)
        .expect("ApplicationData present");
    assert!(
        app_data.children.iter().any(|c| c.token == email::READ
            && matches!(&c.value, provider_eas::wbxml::WbxmlValue::Text(t) if t == "0")),
        "mark-unread upsyncs Read=0"
    );
    assert!(
        !app_data.children.iter().any(|c| c.token == email::FLAG),
        "an unread edit carries no Flag element"
    );
}

/// (d) `MoveTo` rides `MoveItems` with the adapter's bound folder as the
/// source collection and the destination `MailboxId` verbatim; the
/// inverted status table's success shape (3 + DstMsgId, the Exchange 15.2
/// live evidence) resolves, and the receipt records the SOURCE key — the
/// destination copy is a new ServerId the next sync of that folder
/// reconciles.
#[tokio::test]
async fn move_to_rides_moveitems_with_the_bound_folder_as_source() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&super::items_flow::move_items_response(&[(
            "3",
            Some("5:77"),
        )]))
    }) as Handler);
    let adapter = adapter_at(&server);

    let destination = MailboxId::try_from("fid-archive").unwrap();
    let receipt = adapter
        .edit_mail(&account(), &MailEdit::move_to(target(), destination))
        .await
        .expect("the move resolves");
    assert_eq!(
        receipt.message_key.as_str(),
        "sid:7",
        "a move records the SOURCE key — the destination copy reconciles next sync"
    );

    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let mv = find_el(&tree, pages::MOVE, MV_MOVE).expect("the Move element");
    let text_of = |token: u8| {
        mv.children
            .iter()
            .find(|c| c.token == token)
            .and_then(|c| match &c.value {
                provider_eas::wbxml::WbxmlValue::Text(t) => Some(t.clone()),
                _ => None,
            })
    };
    assert_eq!(text_of(MV_SRC_MSG_ID).as_deref(), Some("sid:7"));
    assert_eq!(
        text_of(MV_SRC_FLD_ID).as_deref(),
        Some("fid-inbox"),
        "the bound folder IS the source collection"
    );
    assert_eq!(
        text_of(MV_DST_FLD_ID).as_deref(),
        Some("fid-archive"),
        "the destination MailboxId travels verbatim"
    );
}

/// (e) `Delete` is refused: the trait's Delete is the PERMANENT delete, and
/// EAS has no per-item hard-delete command (only whole-folder
/// EmptyFolderContents). The documented adapter policy: move to the
/// deleted-items folder with `MoveTo` instead.
#[tokio::test]
async fn permanent_delete_is_refused() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let adapter = adapter_at(&server);

    let err = adapter
        .edit_mail(&account(), &MailEdit::delete(target()))
        .await
        .expect_err("EAS cannot hard-delete one item");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert_eq!(server.count(), 0, "the refusal never touches the wire");
}

/// (f) The vocabulary gate: EAS expresses exactly `Read` and `Flag`, so a
/// keyword outside that pair (e.g. `$draft`) is refused permanently BEFORE
/// the wire — the IMAP `PERMANENTFLAGS` refusal spirit.
#[tokio::test]
async fn keywords_beyond_the_eas_vocabulary_are_refused() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&sync_round("1", "c1", false, &[], &[], &[]))
    }) as Handler);
    let adapter = adapter_at(&server);
    seed_ledger(&adapter).await;

    let mut add = BTreeSet::new();
    add.insert(Keyword::system(SystemKeyword::Draft));
    let err = adapter
        .edit_mail(
            &account(),
            &MailEdit::SetKeywords {
                target: target(),
                add,
                remove: BTreeSet::new(),
            },
        )
        .await
        .expect_err("$draft is not upsyncable");
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(
        server.count(),
        2,
        "only the seeding rounds ran — no edit round"
    );
}
