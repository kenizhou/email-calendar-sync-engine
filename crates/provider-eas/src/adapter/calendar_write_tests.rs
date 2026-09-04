// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the calendar write slice (`calendar_write.rs`) — the pure
//! refusal shapes. The wire scenarios (Add/Change/Delete over the mock HTTP
//! server) live in `tests/transport_harness/adapter_calendar_write_flow.rs`;
//! the conversion goldens in `src/calendar/convert_write_tests.rs`.

use engine_core::error::FailureClass;

use super::*;

/// The `put_event` refusal names the supported verb and refuses
/// `InvalidState` — the rejecting default the trait allows for a transport
/// whose update is already a patch.
#[test]
fn the_put_refusal_names_the_patch_verb() {
    let err = put_refusal();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        err.detail().contains("patch_event"),
        "the refusal points at the supported path: {}",
        err.detail()
    );
}
