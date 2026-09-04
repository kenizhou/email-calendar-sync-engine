// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the shared hierarchy ledger (`hierarchy.rs`) — the
//! `#[path]` split the repo uses to hold the 500-line cap. The wire-level
//! interleave scenarios live in `tests/transport_harness/adapter_hierarchy_flow.rs`.

use super::*;

fn folder(id: &str, name: &str) -> EasFolder {
    EasFolder {
        server_id: id.to_owned(),
        parent_id: "0".to_owned(),
        display_name: name.to_owned(),
        class: "Calendar".to_owned(),
        folder_type: Some(8),
    }
}

/// No interleaved round completed between the drain and the failure:
/// the drained backlog returns verbatim for the next pass.
#[test]
fn restore_returns_a_drained_backlog_verbatim_when_nothing_pended() {
    let ledger = HierarchyLedger::default();
    let drained = Backlog {
        rows: vec![folder("fid-cal-1", "Calendar")],
        deletions: vec!["fid-gone".to_owned()],
        present: Some(vec![folder("fid-cal-1", "Calendar")]),
    };
    ledger.restore(Container::Calendar, drained);
    let state = ledger.state.lock().expect("hierarchy ledger");
    let calendar = &state.calendar;
    assert_eq!(
        calendar
            .rows
            .iter()
            .map(|f| f.server_id.as_str())
            .collect::<Vec<_>>(),
        vec!["fid-cal-1"]
    );
    assert_eq!(calendar.deletions, vec!["fid-gone".to_owned()]);
    assert_eq!(calendar.present.as_ref().map_or(0, Vec::len), 1);
}

/// Rows pended by a round that COMPLETED after this one drained are
/// newer: the drained rows restore first, so the consuming fold's
/// dedupe (last row per id wins) keeps the newer pended state.
#[test]
fn restore_puts_drained_rows_behind_newer_pended_ones() {
    let ledger = HierarchyLedger::default();
    // The interleave's slot side: another scope's round completed and
    // pended a newer row for the same ServerId.
    {
        let mut state = ledger.state.lock().expect("hierarchy ledger");
        state.backlog_of_mut(Container::Calendar).rows = vec![folder("fid-cal-1", "Renamed Later")];
    }
    // The interleave's drained side: the older state this round took.
    let drained = Backlog {
        rows: vec![folder("fid-cal-1", "Stale Name")],
        deletions: Vec::new(),
        present: None,
    };
    ledger.restore(Container::Calendar, drained);
    let merged = ledger
        .state
        .lock()
        .expect("hierarchy ledger")
        .calendar
        .rows
        .clone();
    assert_eq!(
        merged
            .iter()
            .map(|f| f.display_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Stale Name", "Renamed Later"],
        "the drained row restores first — behind the newer pended row"
    );
    let mut folded = merged;
    dedupe_folders(&mut folded);
    assert_eq!(
        folded
            .iter()
            .map(|f| f.display_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Renamed Later"],
        "the consuming fold's dedupe keeps the newer row"
    );
}

/// A snapshot present-set pended by a completed interleaved round is
/// newer than everything the failed round drained: the restore must
/// not clobber it with the stale drained one, and it supersedes the
/// drained rows wholesale (the bootstrap-supersedes rule — a drained
/// row whose folder since vanished must not resurrect).
#[test]
fn restore_never_clobbers_a_fresh_present_set() {
    let ledger = HierarchyLedger::default();
    {
        let mut state = ledger.state.lock().expect("hierarchy ledger");
        state.backlog_of_mut(Container::Calendar).present =
            Some(vec![folder("fid-cal-1", "Calendar")]);
    }
    let drained = Backlog {
        rows: vec![folder("fid-cal-9", "Vanished Before The Snapshot")],
        deletions: vec!["fid-gone".to_owned()],
        present: Some(vec![
            folder("fid-cal-1", "Calendar"),
            folder("fid-cal-9", "Old Calendar"),
        ]),
    };
    ledger.restore(Container::Calendar, drained);
    let state = ledger.state.lock().expect("hierarchy ledger");
    let calendar = &state.calendar;
    assert!(
        calendar.rows.is_empty(),
        "the fresh present-set supersedes the drained rows wholesale"
    );
    let present_ids: Vec<&str> = calendar
        .present
        .as_ref()
        .map(|rows| rows.iter().map(|f| f.server_id.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        present_ids,
        vec!["fid-cal-1"],
        "the stale drained present-set (fid-cal-9 still present) does not win"
    );
    assert!(
        calendar.deletions.contains(&"fid-gone".to_owned()),
        "the drained tombstones still ride — a deletion is idempotent"
    );
}
