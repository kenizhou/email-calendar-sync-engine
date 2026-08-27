// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the `fetch_message_source` slice (`source.rs`) — the
//! `#[path]` split the repo uses to hold the 500-line cap (the
//! `email_tests.rs` precedent).

use super::*;

#[test]
fn fetch_statuses_classify_per_the_itemoperations_table() {
    let class = |status: u8| fetch_status_error(status).class();
    // Retry-shaped: server error, ambiguous partial success, provision
    // statuses that escaped the transport's one retry.
    for status in [3u8, 17, 142, 143] {
        assert_eq!(
            class(status),
            engine_core::error::FailureClass::Retryable,
            "ItemOperations {status} retries"
        );
    }
    // The stale-target class the trait names for fetch verbs: an item that
    // moved (EAS rotates ServerIds on moves) is re-sync-then-retry.
    assert_eq!(class(6), engine_core::error::FailureClass::Conflict);
    // Credentials required re-authenticates.
    assert_eq!(class(18), engine_core::error::FailureClass::Authentication);
    // Permanent as-is: protocol error, invalid range, too large, conversion,
    // attachment, access denied, unknown codes.
    for status in [2u8, 8, 11, 14, 15, 16, 222] {
        assert_eq!(
            class(status),
            engine_core::error::FailureClass::Permanent,
            "ItemOperations {status} is permanent"
        );
    }
    // The surfaced detail carries the protocol status code.
    assert!(
        fetch_status_error(6).detail().contains('6'),
        "detail keeps the status: {}",
        fetch_status_error(6).detail()
    );
}

/// The truncation predicate: the Truncated flag alone (an unranged answer
/// may carry no Total), a Total shortfall, either — and neither means the
/// whole item has arrived.
#[test]
fn truncation_is_flag_or_total_shortfall() {
    assert!(is_truncated(Some(true), None, 100), "flag alone");
    assert!(is_truncated(None, Some(300), 100), "Total shortfall");
    assert!(
        is_truncated(Some(false), Some(300), 120),
        "an explicit not-truncated flag does not cancel a Total shortfall"
    );
    assert!(!is_truncated(None, None, 100), "no signal, unranged whole");
    assert!(
        !is_truncated(Some(false), Some(300), 300),
        "flag clear and Total satisfied"
    );
}

/// The continuation span: from the assembled length, chunk-wide, capped at
/// Total-1; the m ≤ n invariant holds even for the degenerate call the
/// loop never makes.
#[test]
fn next_range_spans_from_assembled_capped_at_total() {
    assert_eq!(next_range(120, Some(300)), (120, 299), "capped at Total-1");
    assert_eq!(
        next_range(120, None),
        (120, 119 + RANGE_CHUNK_BYTES),
        "no Total known: a full chunk span"
    );
    let degenerate = next_range(300, Some(300));
    assert!(degenerate.0 <= degenerate.1, "m <= n always");
}
