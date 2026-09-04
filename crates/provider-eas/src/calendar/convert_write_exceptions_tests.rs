// SPDX-License-Identifier: MPL-2.0
//! The instance-write and structural-guard half of the write-conversion
//! tests (`convert_write_exceptions.rs`): the exception under the master,
//! the deleted-marker form, and the refusals for shapes the wire cannot
//! carry. The draft/series round-trips live in `convert_write_tests.rs`
//! (the 500-line split); the shared fixtures come from there.

use engine_core::{
    calendar::{Recurrence, RecurrenceOverride},
    time::Duration,
};
use engine_provider::{DraftRecurrence, EventPatch, Occurrence};

use super::convert_write_tests::{round_trip, series_base, stamp, weekly_tuesday, zoned};

/// An instance patch lands as an exception under the master: the master's own
/// fields stay as they were, the existing exceptions ride, and the target
/// occurrence gains a modified exception carrying exactly the patched fields
/// (start and end both — moving an exception without its end is ambiguous).
#[test]
fn an_instance_patch_writes_the_exception_under_the_master() {
    let base = series_base();
    let patch = EventPatch::new(stamp())
        .summary("Special Tuesday")
        .start(zoned("2026-09-01", "14:00:00"));
    let w = super::convert_write::write_exception(
        &base,
        &Occurrence::starting(zoned("2026-09-01", "09:00:00")),
        &patch,
    )
    .expect("the exception converts");
    // The master's own document is untouched.
    assert_eq!(w.subject.as_deref(), Some("Weekly Standup"));
    assert_eq!(w.start_time, "20260811T010000Z");
    assert_eq!(w.exceptions.len(), 3, "the two existing exceptions ride");
    let new = w
        .exceptions
        .iter()
        .find(|e| e.exception_start_time.as_deref() == Some("20260901T010000Z"))
        .expect("the target occurrence's exception");
    assert!(!new.deleted);
    assert_eq!(new.start_time.as_deref(), Some("20260901T060000Z"));
    assert_eq!(new.end_time.as_deref(), Some("20260901T063000Z"));
    assert_eq!(new.subject.as_deref(), Some("Special Tuesday"));

    let event = round_trip(&w);
    let recurrence = event.recurrence.as_ref().expect("recurring");
    let key: engine_core::time::LocalDateTime = "2026-09-01T09:00:00".parse().unwrap();
    let RecurrenceOverride::Patch(over) = recurrence.overrides.get(&key).expect("the override")
    else {
        panic!("the new exception reads back as a patch override");
    };
    assert_eq!(
        over.get("title"),
        Some(&serde_json::json!("Special Tuesday"))
    );
}

/// An instance patch that also edits the recurrence is refused — an
/// occurrence has no rule of its own.
#[test]
fn an_instance_patch_cannot_edit_the_recurrence() {
    let base = series_base();
    let patch = EventPatch::new(stamp()).recurrence(DraftRecurrence::new(weekly_tuesday()));
    let err = super::convert_write::write_exception(
        &base,
        &Occurrence::starting(zoned("2026-09-01", "09:00:00")),
        &patch,
    )
    .expect_err("the pairing is refused");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
}

/// An occurrence delete lands as the deleted-marker exception (the EAS
/// EXDATE form), replacing whatever override the occurrence had.
#[test]
fn an_occurrence_delete_becomes_the_deleted_marker() {
    let base = series_base();
    let w = super::convert_write::write_occurrence_deleted(
        &base,
        &Occurrence::starting(zoned("2026-08-25", "09:00:00")),
    )
    .expect("converts");
    let gone = w
        .exceptions
        .iter()
        .find(|e| e.exception_start_time.as_deref() == Some("20260825T010000Z"))
        .expect("the target occurrence");
    assert!(gone.deleted, "the override becomes a deleted marker");
    assert_eq!(gone.subject, None, "a deleted marker carries no data");
    assert_eq!(w.exceptions.len(), 2);

    let event = round_trip(&w);
    let key: engine_core::time::LocalDateTime = "2026-08-25T09:00:00".parse().unwrap();
    assert_eq!(
        event.recurrence.as_ref().unwrap().overrides.get(&key),
        Some(&RecurrenceOverride::Excluded)
    );
}

/// A whole-minute display alert maps back to Reminder minutes; anything the
/// wire cannot express (a second alert, a sub-minute offset) is refused
/// rather than silently dropped.
#[test]
fn alerts_map_to_reminder_minutes_or_refuse() {
    let mut base = series_base();
    base.alerts.clear();
    let w = super::convert_write::write_from_series(&base, &EventPatch::new(stamp()))
        .expect("no alerts is fine");
    assert_eq!(w.reminder_minutes, None);

    base.alerts.push(engine_core::calendar::Alert::display(
        engine_core::calendar::Trigger::before_start(
            Duration::from_parts(0, 0, 0, 20, 0, 0).unwrap(),
        ),
    ));
    let w = super::convert_write::write_from_series(&base, &EventPatch::new(stamp()))
        .expect("a plain reminder maps");
    assert_eq!(w.reminder_minutes, Some(20));

    base.alerts.push(engine_core::calendar::Alert::display(
        engine_core::calendar::Trigger::before_start(
            Duration::from_parts(0, 0, 0, 0, 30, 0).unwrap(),
        ),
    ));
    let err = super::convert_write::write_from_series(&base, &EventPatch::new(stamp()))
        .expect_err("two alerts have no one-Reminder form");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}

/// A base whose recurrence is structural-but-unrepresentable (multiple rule
/// sets, or an override patch carrying keys the read side never produces) is
/// refused rather than silently flattened.
#[test]
fn structural_shapes_the_wire_cannot_carry_are_refused() {
    let mut base = series_base();
    let mut rules = Recurrence::from_rule(weekly_tuesday());
    rules.rules.push(weekly_tuesday());
    base.recurrence = Some(rules);
    let err = super::convert_write::write_from_series(&base, &EventPatch::new(stamp()))
        .expect_err("a rule union has no EAS form");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}
