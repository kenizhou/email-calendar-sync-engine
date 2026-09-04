// SPDX-License-Identifier: MPL-2.0
//! Write-side conversion tests: the neutral engine shapes →
//! [`CalendarEventWrite`] (`convert_write.rs`, P2 Task 3), including the
//! round trip back through the parse side (`parse_calendar_application_data`
//! + `calendar_event_from_props`) for every shape that is representable.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, Weekday},
    ids::{CalendarId, Uid},
    time::{CalendarDate, CalendarDateTime, Duration, TimeZoneId},
};
use engine_provider::{DraftRecurrence, EventDraft, EventPatch};

use super::{
    CalendarEventProps, CalendarException, CalendarRecurrence, TimeZoneBlob, TziTimeZone,
    calendar_event_from_props, parse_calendar_application_data,
};
use crate::calendar_write::build_calendar_application_data;

const FOLDER: &str = "fid-cal-1";
const SERVER_ID: &str = "srv:ev-9";

/// The flat UTC+8 fold zone the read side produces (POSIX-inverted sign).
fn utc8() -> TimeZoneId {
    TimeZoneId::iana("Etc/GMT-8").expect("zone name")
}

pub(super) fn zoned(day: &str, wall: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: format!("{day}T{wall}").parse().expect("wall clock"),
        zone: utc8(),
    }
}

pub(super) fn stamp() -> engine_core::time::UtcDateTime {
    "2026-08-15T12:00:00Z".parse().unwrap()
}

pub(super) fn draft(start: CalendarDateTime, end: CalendarDateTime) -> EventDraft {
    EventDraft::new(
        CalendarId::try_from(FOLDER).unwrap(),
        Uid::new("uid-write-1").unwrap(),
        "Sprint Review",
        start,
        end,
        stamp(),
    )
}

/// The wire document round-tripped back into the neutral `Event` (id and
/// membership pinned to the test constants — identity is the caller's).
pub(super) fn round_trip(
    w: &crate::calendar_write::CalendarEventWrite,
) -> engine_core::calendar::Event {
    let tree = build_calendar_application_data(w, "16.1");
    let props = parse_calendar_application_data(&tree).expect("the built document parses");
    calendar_event_from_props(FOLDER, SERVER_ID, &props)
}

/// A realistic recurring master as the read side produces it: flat UTC+8
/// zone, weekly-Tuesday count-4 rule, one deleted occurrence, one moved and
/// retitled occurrence, an organizer plus one attendee, a 15-minute
/// reminder, and the EAS-native busy/sensitivity facts.
pub(super) fn series_base() -> engine_core::calendar::Event {
    let props = CalendarEventProps {
        all_day_event: false,
        start_time: Some("20260811T010000Z".to_owned()),
        end_time: Some("20260811T013000Z".to_owned()),
        dtstamp: Some("20260810T080000Z".to_owned()),
        subject: Some("Weekly Standup".to_owned()),
        location: Some("Room 42".to_owned()),
        body_plain: Some("Agenda: sync status".to_owned()),
        organizer_name: Some("Felix Zhou".to_owned()),
        organizer_email: Some("felixzhou@kylins.local".to_owned()),
        sensitivity: Some(1),
        busy_status: Some(2),
        reminder_set: true,
        reminder_minutes: Some(15),
        meeting_status: None,
        response_requested: false,
        time_zone: Some(flat_utc8_blob()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            day_of_week: Some(4),
            occurrences: Some(4),
            ..CalendarRecurrence::default()
        }),
        uid: Some("uid-standup".to_owned()),
        exceptions: vec![
            CalendarException {
                deleted: true,
                exception_start_time: Some("20260818T010000Z".to_owned()),
                ..CalendarException::default()
            },
            CalendarException {
                exception_start_time: Some("20260825T010000Z".to_owned()),
                start_time: Some("20260825T030000Z".to_owned()),
                end_time: Some("20260825T033000Z".to_owned()),
                subject: Some("Late standup".to_owned()),
                ..CalendarException::default()
            },
        ],
        attendees: vec![super::CalendarAttendee {
            name: Some("Ana Rivera".to_owned()),
            email: "ana@example.test".to_owned(),
            status: Some(3),
        }],
    };
    calendar_event_from_props(FOLDER, SERVER_ID, &props)
}

pub(super) fn flat_utc8_blob() -> TimeZoneBlob {
    TimeZoneBlob {
        raw_base64: Some(crate::calendar::tests::TZI_FLAT_UTC8.to_owned()),
        parsed: Some(TziTimeZone {
            base_bias_minutes: -480,
            standard: None,
            daylight: None,
        }),
    }
}

// ============================================================================
// Drafts (create)
// ============================================================================

/// A zoned one-off draft round-trips exactly: the wire stamps are the wall
/// clock minus the fixed offset, the TZI blob is the flat UTC+8 one, and the
/// parse side folds the same wall clock and zone back out.
#[test]
fn a_zoned_draft_round_trips_through_the_wire() {
    let draft = draft(
        zoned("2026-08-18", "09:00:00"),
        zoned("2026-08-18", "10:00:00"),
    )
    .description("Quarterly review")
    .location("Room 101");
    let w = super::convert_write::write_from_draft(&draft).expect("the draft converts");
    assert_eq!(w.start_time, "20260818T010000Z");
    assert_eq!(w.end_time, "20260818T020000Z");
    assert!(!w.all_day_event);
    assert_eq!(
        w.time_zone_base64,
        crate::calendar_write::build_fixed_offset_tzi_base64(480)
    );
    assert_eq!(w.subject.as_deref(), Some("Sprint Review"));
    assert_eq!(w.body_plain.as_deref(), Some("Quarterly review"));
    assert_eq!(w.location.as_deref(), Some("Room 101"));
    // Server-managed identity is never emitted.
    assert_eq!(w.organizer_email, None);
    assert_eq!(w.organizer_name, None);

    let event = round_trip(&w);
    assert_eq!(event.title, "Sprint Review");
    assert_eq!(event.start, zoned("2026-08-18", "09:00:00"));
    assert_eq!(
        event.duration,
        Duration::from_parts(0, 0, 1, 0, 0, 0).unwrap()
    );
    assert_eq!(event.description.as_deref(), Some("Quarterly review"));
    assert_eq!(event.locations.len(), 1);
    assert_eq!(event.locations[0].name.as_deref(), Some("Room 101"));
}

/// An all-day draft folds to UTC midnights with `AllDayEvent` set, and the
/// exclusive end date rides verbatim (RFC 5545 §3.6.1).
#[test]
fn an_all_day_draft_round_trips_as_utc_midnights() {
    let start = CalendarDateTime::Date(CalendarDate::new(2026, 8, 18).unwrap());
    let end = CalendarDateTime::Date(CalendarDate::new(2026, 8, 19).unwrap());
    let w = super::convert_write::write_from_draft(&draft(start.clone(), end)).expect("converts");
    assert!(w.all_day_event);
    assert_eq!(w.start_time, "20260818T000000Z");
    assert_eq!(w.end_time, "20260819T000000Z");

    let event = round_trip(&w);
    assert_eq!(event.start, start);
    assert_eq!(
        event.duration,
        Duration::from_parts(0, 1, 0, 0, 0, 0).unwrap(),
        "one whole day"
    );
}

/// A floating draft pins to UTC: the wall clock rides verbatim on the wire
/// with a zero-bias TZI (the wire cannot express "no zone").
#[test]
fn a_floating_draft_pins_to_utc() {
    let start = CalendarDateTime::Floating("2026-08-18T09:00:00".parse().unwrap());
    let end = CalendarDateTime::Floating("2026-08-18T10:00:00".parse().unwrap());
    let w = super::convert_write::write_from_draft(&draft(start, end)).expect("converts");
    assert_eq!(w.start_time, "20260818T090000Z");
    assert_eq!(w.end_time, "20260818T100000Z");
    assert_eq!(
        w.time_zone_base64,
        crate::calendar_write::build_fixed_offset_tzi_base64(0)
    );
}

/// A named-DST zone start is refused: resolving its offset needs tzdata no
/// adapter carries — never a guessed offset.
#[test]
fn a_named_dst_zone_start_is_refused() {
    let start = CalendarDateTime::Zoned {
        local: "2026-08-18T09:00:00".parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let end = CalendarDateTime::Zoned {
        local: "2026-08-18T10:00:00".parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let err = super::convert_write::write_from_draft(&draft(start, end))
        .expect_err("no fixed offset is derivable");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(
        err.detail().contains("fixed-offset"),
        "the refusal names what it needs: {}",
        err.detail()
    );
}

// ============================================================================
// Recurrence write (rule + bound)
// ============================================================================

pub(super) fn weekly_tuesday() -> engine_core::calendar::RecurrenceRule {
    let mut rule = engine_core::calendar::RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Tu,
        nth_of_period: None,
    }];
    rule
}

/// A counted weekly rule round-trips structurally: Type 1, the Tuesday bit,
/// Occurrences — and no FirstDayOfWeek element (the Monday default is not
/// written).
#[test]
fn a_counted_weekly_rule_round_trips() {
    let mut rule = weekly_tuesday();
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(4).unwrap());
    let start = zoned("2026-08-11", "09:00:00");
    let end = zoned("2026-08-11", "09:30:00");
    let d = draft(start, end).repeating(DraftRecurrence::new(rule));
    let w = super::convert_write::write_from_draft(&d).expect("converts");
    let rec = w.recurrence.as_ref().expect("the rule rides");
    assert_eq!(rec.recurrence_type, 1);
    assert_eq!(rec.day_of_week, Some(4));
    assert_eq!(rec.occurrences, Some(4));
    assert_eq!(rec.first_day_of_week, None);

    let event = round_trip(&w);
    let recurrence = event.recurrence.as_ref().expect("recurring");
    assert_eq!(recurrence.rules, vec![weekly_tuesday_with_bound(4)]);
    assert!(recurrence.overrides.is_empty());
}

fn weekly_tuesday_with_bound(count: u32) -> engine_core::calendar::RecurrenceRule {
    let mut rule = weekly_tuesday();
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(count).unwrap());
    rule
}

/// A weekly rule whose week starts on Sunday writes FirstDayOfWeek 0 — the
/// WKST counterpart — and it round-trips back.
#[test]
fn a_sunday_week_start_is_written_and_round_trips() {
    let mut rule = weekly_tuesday();
    rule.first_day_of_week = Weekday::Su;
    let start = zoned("2026-08-11", "09:00:00");
    let end = zoned("2026-08-11", "09:30:00");
    let w = super::convert_write::write_from_draft(
        &draft(start, end).repeating(DraftRecurrence::new(rule.clone())),
    )
    .expect("converts");
    assert_eq!(w.recurrence.as_ref().unwrap().first_day_of_week, Some(0));
    let event = round_trip(&w);
    assert_eq!(
        event.recurrence.as_ref().unwrap().rules[0].first_day_of_week,
        Weekday::Su
    );
}

/// An `Until` bound on a zoned series needs the caller-resolved instant; with
/// it the wire carries that instant and the parse side folds the same wall
/// clock back out.
#[test]
fn an_until_bound_rides_the_resolved_instant() {
    let mut rule = weekly_tuesday();
    rule.bound = RecurrenceBound::Until("2026-09-01T09:00:00".parse().unwrap());
    let start = zoned("2026-08-11", "09:00:00");
    let end = zoned("2026-08-11", "09:30:00");
    let resolved: engine_core::time::UtcDateTime = "2026-09-01T01:00:00Z".parse().unwrap();
    let d = draft(start, end).repeating(DraftRecurrence::ending_at(rule, resolved));
    let w = super::convert_write::write_from_draft(&d).expect("converts");
    assert_eq!(
        w.recurrence.as_ref().unwrap().until.as_deref(),
        Some("20260901T010000Z")
    );

    let event = round_trip(&w);
    assert_eq!(
        event.recurrence.as_ref().unwrap().rules[0].bound,
        RecurrenceBound::Until("2026-09-01T09:00:00".parse().unwrap())
    );
}

/// The same `Until` without the resolved instant derives through the
/// event's own fixed-offset zone — exact without tzdata (an EAS event's
/// zone is a fixed offset by construction; the resolved instant is the
/// cross-transport uniformity device, not a requirement this adapter has).
#[test]
fn a_zoned_until_without_the_resolved_instant_derives_through_the_fold() {
    let mut rule = weekly_tuesday();
    rule.bound = RecurrenceBound::Until("2026-09-01T09:00:00".parse().unwrap());
    let start = zoned("2026-08-11", "09:00:00");
    let end = zoned("2026-08-11", "09:30:00");
    let w = super::convert_write::write_from_draft(
        &draft(start, end).repeating(DraftRecurrence::new(rule)),
    )
    .expect("the wall clock derives the instant at a fixed offset");
    assert_eq!(
        w.recurrence.as_ref().unwrap().until.as_deref(),
        Some("20260901T010000Z")
    );
}

/// Rules with no EAS form are refused loudly, never silently flattened:
/// sub-daily frequencies, BYSETPOS, a daily rule restricted by weekday, and
/// a monthly rule carrying both positional and day-of-month parts.
#[test]
fn unrepresentable_rules_are_refused() {
    fn refuses_with(rule: engine_core::calendar::RecurrenceRule, name: &str) {
        let start = zoned("2026-08-11", "09:00:00");
        let end = zoned("2026-08-11", "09:30:00");
        let err = super::convert_write::write_from_draft(
            &draft(start, end).repeating(DraftRecurrence::new(rule)),
        )
        .expect_err(name);
        assert_eq!(
            err.class(),
            engine_core::error::FailureClass::Permanent,
            "{name}"
        );
    }
    refuses_with(
        engine_core::calendar::RecurrenceRule::new(Frequency::Hourly),
        "hourly",
    );
    let mut bysetpos = weekly_tuesday();
    bysetpos.by_set_position = vec![1];
    refuses_with(bysetpos, "bysetpos");
    let mut daily_byday = engine_core::calendar::RecurrenceRule::new(Frequency::Daily);
    daily_byday.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    refuses_with(daily_byday, "daily with by_day");
    let mut monthly_both = engine_core::calendar::RecurrenceRule::new(Frequency::Monthly);
    monthly_both.by_day = vec![NDay {
        day: Weekday::Tu,
        nth_of_period: Some(NonZeroI32::new(2).unwrap()),
    }];
    monthly_both.by_month_day = vec![15];
    refuses_with(monthly_both, "monthly with both parts");
}

/// The positional families round-trip: monthly on the 15th (Type 2), monthly
/// on the 2nd Tuesday (Type 3), yearly on Mar 1 (Type 5), and yearly on the
/// 2nd Tuesday of March (Type 6).
#[test]
fn the_positional_families_round_trip() {
    let cases: Vec<(engine_core::calendar::RecurrenceRule, u8, u32)> = {
        let mut m15 = engine_core::calendar::RecurrenceRule::new(Frequency::Monthly);
        m15.by_month_day = vec![15];
        let mut m2tu = engine_core::calendar::RecurrenceRule::new(Frequency::Monthly);
        m2tu.by_day = vec![NDay {
            day: Weekday::Tu,
            nth_of_period: Some(NonZeroI32::new(2).unwrap()),
        }];
        let mut y = engine_core::calendar::RecurrenceRule::new(Frequency::Yearly);
        y.by_month = vec!["3".to_owned()];
        y.by_month_day = vec![1];
        let mut y2tu = engine_core::calendar::RecurrenceRule::new(Frequency::Yearly);
        y2tu.by_month = vec!["3".to_owned()];
        y2tu.by_day = vec![NDay {
            day: Weekday::Tu,
            nth_of_period: Some(NonZeroI32::new(2).unwrap()),
        }];
        vec![(m15, 2, 15), (m2tu, 3, 4), (y, 5, 3), (y2tu, 6, 3)]
    };
    for (rule, wire_type, _part) in cases {
        let start = zoned("2026-08-11", "09:00:00");
        let end = zoned("2026-08-11", "09:30:00");
        let w = super::convert_write::write_from_draft(
            &draft(start, end).repeating(DraftRecurrence::new(rule.clone())),
        )
        .expect("the rule converts");
        assert_eq!(w.recurrence.as_ref().unwrap().recurrence_type, wire_type);
        let event = round_trip(&w);
        assert_eq!(
            event.recurrence.as_ref().unwrap().rules,
            vec![rule],
            "wire type {wire_type} round-trips"
        );
    }
}

// ============================================================================
// Series patch (Replace of the master)
// ============================================================================

/// A series patch rebuilds the complete master document: the patched fields
/// land, everything else rides from the base (times folded back through the
/// fixed-offset zone, the attendee without the organizer, the EAS-native
/// busy/sensitivity facts, the reminder, the rule, and every exception).
#[test]
fn a_series_patch_rebuilds_the_whole_master_document() {
    let base = series_base();
    let patch = EventPatch::new(stamp())
        .summary("Renamed Standup")
        .start(zoned("2026-08-11", "10:00:00"));
    let w = super::convert_write::write_from_series(&base, &patch).expect("rebuilds");
    assert_eq!(w.subject.as_deref(), Some("Renamed Standup"));
    assert_eq!(w.start_time, "20260811T020000Z", "10:00 wall at UTC+8");
    assert_eq!(
        w.end_time, "20260811T023000Z",
        "end = moved start + the base length (30m)"
    );
    assert_eq!(w.busy_status, Some(2), "the EAS-native facts survive");
    assert_eq!(w.sensitivity, Some(1));
    assert_eq!(w.reminder_minutes, Some(15));
    assert_eq!(w.location.as_deref(), Some("Room 42"));
    assert_eq!(w.body_plain.as_deref(), Some("Agenda: sync status"));
    // The attendee rides; the organizer never does (server-managed, Status 6).
    assert_eq!(w.attendees.len(), 1);
    assert_eq!(w.attendees[0].email, "ana@example.test");
    assert_eq!(w.attendees[0].name.as_deref(), Some("Ana Rivera"));
    // The rule and both exceptions ride untouched.
    let rec = w.recurrence.as_ref().expect("the rule rides");
    assert_eq!(rec.recurrence_type, 1);
    assert_eq!(w.exceptions.len(), 2);

    let event = round_trip(&w);
    assert_eq!(event.title, "Renamed Standup");
    assert_eq!(event.start, zoned("2026-08-11", "10:00:00"));
    assert_eq!(
        event.duration,
        Duration::from_parts(0, 0, 0, 30, 0, 0).unwrap()
    );
    assert_eq!(
        event.recurrence.as_ref().unwrap().overrides.len(),
        2,
        "both overrides survive the series edit"
    );
}

/// A cleared description/location writes the explicit empty element — never
/// a silent omission, which a ghosting server would keep the old value for.
#[test]
fn clearing_text_properties_writes_explicit_empty_values() {
    let base = series_base();
    let patch = EventPatch::new(stamp())
        .clear_description()
        .clear_location();
    let w = super::convert_write::write_from_series(&base, &patch).expect("rebuilds");
    assert_eq!(w.body_plain.as_deref(), Some(""));
    assert_eq!(w.location.as_deref(), Some(""));
}

/// The form rule: a patch that would change the event's time form — a
/// different zone, or a date against a timed event — is refused, not
/// converted.
#[test]
fn form_changing_patches_are_refused() {
    let base = series_base();
    let rezone = EventPatch::new(stamp()).start(CalendarDateTime::Zoned {
        local: "2026-08-11T10:00:00".parse().unwrap(),
        zone: TimeZoneId::utc(),
    });
    let err =
        super::convert_write::write_from_series(&base, &rezone).expect_err("zone change refused");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);

    let redate = EventPatch::new(stamp()).start(CalendarDateTime::Date(
        CalendarDate::new(2026, 8, 11).unwrap(),
    ));
    let err =
        super::convert_write::write_from_series(&base, &redate).expect_err("form change refused");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}

// ============================================================================
// Instance writes (exceptions)
// ============================================================================
// ============================================================================
// Reminders and structural guards
// ============================================================================
