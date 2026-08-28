// SPDX-License-Identifier: MPL-2.0
//! The engine-cli acceptance scenario: `engine-cli eas-sync --rounds 2`
//! driven end-to-end against the mock server — the T8 offline proof that
//! one command does the full pass (negotiation + discovery + mailbox
//! snapshot + per-folder bootstrap) and then the incremental one (deltas
//! off the persisted cursors), through the ENGINE's own fan-out
//! (`engine_sync::sync_mail`), with the rows landing in a real SQLite
//! store (asserted back out through `engine-cli search`).
//!
//! The mock dispatches by REQUEST SHAPE, not ordinal: the engine fans
//! folders out concurrently, so requests interleave — every response is a
//! pure function of the command and the decoded request body.

use std::sync::Arc;

use provider_eas::{
    commands::{AS_COLLECTION_ID, AS_SYNC_KEY, FH_SYNC_KEY, PAGE_AIRSYNC, PAGE_FOLDER},
    wbxml::WbxmlElement,
};

use super::{
    adapter_email_wire::{ItemSpec, sync_round},
    fixtures::folder_sync_response,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// One full+incremental `eas-sync` against a fresh store, then a `search`
/// that proves the rows landed. The hierarchy carries one calendar folder
/// (type 8) the CLI must NOT build an email adapter for — the adapter's
/// class filtering, exercised through the whole stack.
#[tokio::test]
async fn engine_cli_syncs_full_then_incremental_offline() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(respond) as Handler);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let db_arg = db.to_str().unwrap().to_owned();

    let out = engine_cli::run(&args(&[
        "eas-sync",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--url",
        &server.eas_url(),
        "--user",
        "user@example.test",
        "--password",
        "app-password",
        "--rounds",
        "2",
    ]))
    .await
    .expect("both rounds succeed");
    assert!(
        out.contains("2 folder(s)"),
        "the calendar folder is filtered out of the email fan-out: {out}"
    );
    assert!(out.contains("protocol 16.1"), "negotiation: {out}");
    // Round 1: the mailbox snapshot (2 mail folders) + each folder's
    // bootstrap (2 items).
    assert!(
        out.contains("mailboxes  +2"),
        "the snapshot files both mail folders: {out}"
    );
    assert!(out.contains("fid-inbox  +2"), "the inbox bootstrap: {out}");
    assert!(
        out.contains("fid-archive +2"),
        "the archive bootstrap: {out}"
    );
    // Round 2: the incremental deltas — one new item per folder, nothing
    // else.
    assert!(out.contains("fid-inbox  +1"), "the inbox delta: {out}");
    assert!(out.contains("fid-archive +1"), "the archive delta: {out}");

    // The stored rows answer a real query — the sync produced searchable
    // mail, not just counters.
    let out = engine_cli::run(&args(&[
        "search",
        "--db",
        &db_arg,
        "--account",
        "acct-eas-1",
        "--kind",
        "mail",
        "welcome",
    ]))
    .await
    .expect("search over the synced store");
    assert!(
        out.contains("sid:"),
        "hits carry the synced ServerIds: {out}"
    );
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// The mock's whole protocol: OPTIONS negotiates 16.1, FolderSync answers
/// the bootstrap or an empty delta, and Sync answers a collection's
/// bootstrap (`"0"` → two items), its first incremental (`…k1` → one new
/// item), or an empty steady round.
fn respond(req: &CapturedRequest, _ordinal: usize) -> MockResponse {
    if req.method == "OPTIONS" {
        return MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "14.0,14.1,16.0,16.1")
            .with_header("MS-ASProtocolCommands", "Sync,FolderSync,Ping,SendMail");
    }
    let Some(cmd) = req.cmd() else {
        return MockResponse::empty_wbxml();
    };
    let Some(tree) = req.wbxml_tree() else {
        return MockResponse::empty_wbxml();
    };
    match cmd.as_str() {
        "FolderSync" => {
            let key = text_under(&tree, PAGE_FOLDER, FH_SYNC_KEY);
            if key == "0" {
                MockResponse::wbxml(&folder_sync_response(
                    "f1",
                    &[
                        ("fid-inbox", "0", "Inbox", "2"),
                        ("fid-archive", "0", "Archive", "1"),
                        ("fid-cal", "0", "Calendar", "8"),
                    ],
                ))
            } else {
                // The steady delta: nothing changed, key unchanged.
                MockResponse::wbxml(&folder_sync_response(&key, &[]))
            }
        }
        "Sync" => {
            let collection = text_under(&tree, PAGE_AIRSYNC, AS_COLLECTION_ID);
            let key = text_under(&tree, PAGE_AIRSYNC, AS_SYNC_KEY);
            let response = if key == "0" {
                sync_round(
                    "1",
                    &format!("{collection}k1"),
                    false,
                    &[item(&collection, 'a'), item(&collection, 'b')],
                    &[],
                    &[],
                )
            } else if key == format!("{collection}k1") {
                sync_round(
                    "1",
                    &format!("{collection}k2"),
                    false,
                    &[item(&collection, 'n')],
                    &[],
                    &[],
                )
            } else {
                sync_round("1", &key, false, &[], &[], &[])
            };
            MockResponse::wbxml(&response)
        }
        _ => MockResponse::empty_wbxml(),
    }
}

/// One wire email item whose ServerId derives from its collection (the two
/// folders' rows must not collide in the store). The mock knows its two
/// folders, so the table is exhaustive.
fn item(collection: &str, suffix: char) -> ItemSpec {
    let (id, subject): (&'static str, &'static str) = match (collection, suffix) {
        ("fid-inbox", 'a') => ("sid:fid-inbox-a", "Welcome alpha"),
        ("fid-inbox", 'b') => ("sid:fid-inbox-b", "Welcome beta"),
        ("fid-inbox", 'n') => ("sid:fid-inbox-n", "Welcome novo"),
        ("fid-archive", 'a') => ("sid:fid-archive-a", "Welcome alpha"),
        ("fid-archive", 'b') => ("sid:fid-archive-b", "Welcome beta"),
        ("fid-archive", 'n') => ("sid:fid-archive-n", "Welcome novo"),
        _ => unreachable!("the mock knows its folders"),
    };
    (
        id,
        subject,
        "alice@example.test",
        "1",
        Some("2026-08-01T00:00:00.000Z"),
    )
}

/// The first text element with `(page, token)` anywhere under `root`.
fn text_under(root: &WbxmlElement, page: u8, token: u8) -> String {
    fn find(el: &WbxmlElement, page: u8, token: u8) -> Option<String> {
        if el.page == page
            && el.token == token
            && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(|c| find(c, page, token))
    }
    find(root, page, token).unwrap_or_default()
}
