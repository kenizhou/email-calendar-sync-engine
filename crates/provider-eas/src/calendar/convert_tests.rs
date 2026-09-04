// SPDX-License-Identifier: MPL-2.0
//! Golden conversion tests: `CalendarEventProps` → engine-core `Event`
//! (`convert.rs`, the P2 Task-2 read-side seam).
//!
//! Fixtures reuse the module's pinned TZI blobs (`tests.rs`) so the fold's
//! inputs stay the same bytes the parse tests locked. The recurrence-mapping
//! goldens live in `convert_recurrence_tests.rs` (the 500-line split); the
//! expander helper below is shared with them.

use engine_core::{
    calendar::{Alert, EventStatus, FreeBusyStatus, Location, ParticipantRole, Privacy, Trigger},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, Duration, TimeZoneId},
};
use engine_recurrence::{Horizon, expand};
use serde_json::json;

use super::{
    CalendarAttendee, CalendarEventProps, CalendarRecurrence, TimeZoneBlob, TziTimeZone,
    calendar_event_from_props,
    tests::{TZI_DST_CET, TZI_FLAT_UTC8},
};

/// The flat UTC+8 fold fixture, shared with `convert_recurrence_tests`.
pub(super) fn flat_utc8() -> TimeZoneBlob {
    TimeZoneBlob {
        raw_base64: Some(TZI_FLAT_UTC8.to_owned()),
        parsed: Some(TziTimeZone {
            base_bias_minutes: -480,
            standard: None,
            daylight: None,
        }),
    }
}

fn dst_cet() -> TimeZoneBlob {
    TimeZoneBlob {
        raw_base64: Some(TZI_DST_CET.to_owned()),
        parsed: Some(super::parse_tzi_blob(TZI_DST_CET).expect("the pinned CET blob decodes")),
    }
}

fn zoned(wall: &str, zone: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: wall.parse().expect("wall clock parses"),
        zone: TimeZoneId::iana(zone).expect("zone name"),
    }
}

/// Expands `event` over `[from, to)` and returns the occurrence count — the
/// shared expander assertion. `pub(super)` so `convert_recurrence_tests`
/// reuses the same helper.
pub(super) fn occurrence_count(
    event: &engine_core::calendar::Event,
    from: &str,
    to: &str,
) -> usize {
    let horizon = Horizon::new(from.parse().unwrap(), to.parse().unwrap()).expect("horizon");
    let rows = expand(event, &horizon, &TimeZoneId::utc()).expect("the rule is expandable");
    rows.len()
}

/// A single timed meeting in flat UTC+8: the UTC wire stamps fold to the
/// zone's wall clock under a fixed-offset `Etc/GMT-8` zone (the POSIX-inverted
/// sign), every status/privacy/participant field maps, and the reminder
/// becomes an alert.
#[test]
fn single_timed_event_folds_times_and_metadata() {
    let props = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        end_time: Some("20260818T100000Z".to_owned()),
        dtstamp: Some("20260815T120000Z".to_owned()),
        subject: Some("Weekly Sync".to_owned()),
        location: Some("Room 42".to_owned()),
        body_plain: Some("Agenda: sync status".to_owned()),
        organizer_name: Some("Felix Zhou".to_owned()),
        organizer_email: Some("felixzhou@kylins.local".to_owned()),
        sensitivity: Some(2),
        busy_status: Some(2),
        meeting_status: Some(1),
        reminder_set: true,
        reminder_minutes: Some(15),
        time_zone: Some(flat_utc8()),
        uid: Some("040000008200E00074C5B7101A82E008".to_owned()),
        attendees: vec![
            CalendarAttendee {
                name: Some("Bob".to_owned()),
                email: "bob@example.test".to_owned(),
                status: Some(3),
            },
            CalendarAttendee {
                name: Some("Carol".to_owned()),
                email: "carol@example.test".to_owned(),
                status: None,
            },
        ],
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-1", &props);

    assert_eq!(event.id, EventId::try_from("srv:ev-1").unwrap());
    assert_eq!(
        event.uid,
        Uid::new("040000008200E00074C5B7101A82E008").unwrap()
    );
    assert_eq!(
        event.calendars,
        Memberships::of_one(CalendarId::try_from("fid-cal-1").unwrap())
    );
    assert_eq!(event.title, "Weekly Sync");
    assert_eq!(event.description.as_deref(), Some("Agenda: sync status"));
    // UTC 09:00 + 8h = the UTC+8 wall clock; Etc zones invert the sign.
    assert_eq!(event.start, zoned("2026-08-18T17:00:00", "Etc/GMT-8"));
    assert_eq!(event.duration, "PT1H".parse::<Duration>().unwrap());
    assert_eq!(event.status, EventStatus::Confirmed);
    assert_eq!(event.free_busy_status, FreeBusyStatus::Busy);
    assert_eq!(event.privacy, Privacy::Private);
    assert_eq!(
        event.updated.map(|t| t.to_string()),
        Some("2026-08-15T12:00:00Z".to_owned())
    );
    assert_eq!(
        event.locations,
        vec![Location::named("Room 42")],
        "the scalar Location projects to one named engine Location"
    );
    assert_eq!(event.recurrence, None);
    // The reminder becomes a display alert 15 minutes before the start.
    assert_eq!(
        event.alerts,
        vec![Alert::display(Trigger::before_start(
            "PT15M".parse::<Duration>().unwrap()
        ))]
    );
    // Organizer (owner role, not awaiting its own reply) then the attendees.
    let roles: Vec<_> = event
        .participants
        .iter()
        .map(|p| {
            (
                p.email.as_deref(),
                p.name.as_deref(),
                p.has_role(&ParticipantRole::Owner),
                p.has_role(&ParticipantRole::Attendee),
                p.participation_status.as_str().to_owned(),
                p.expect_reply,
            )
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            (
                Some("felixzhou@kylins.local"),
                Some("Felix Zhou"),
                true,
                false,
                "accepted".to_owned(),
                false
            ),
            (
                Some("bob@example.test"),
                Some("Bob"),
                false,
                true,
                "accepted".to_owned(),
                true
            ),
            (
                Some("carol@example.test"),
                Some("Carol"),
                false,
                true,
                "needs-action".to_owned(),
                true
            ),
        ]
    );
    // The EAS-native facts with no first-class field survive verbatim.
    assert_eq!(event.extended.get("eas/busy-status"), Some(&json!(2u8)));
    assert_eq!(event.extended.get("eas/meeting-status"), Some(&json!(1u8)));
    assert_eq!(event.extended.get("eas/sensitivity"), Some(&json!(2u8)));
    assert_eq!(
        event.extended.get("eas/timezone"),
        Some(&json!(TZI_FLAT_UTC8)),
        "the raw TZI blob survives under the adapter namespace"
    );
}

/// An all-day event folds to DATE values: the date parts of the UTC stamps
/// ([MS-ASCAL] §2.2.2.1 — all-day bounds arrive as UTC midnight, the wire END
/// is the exclusive next midnight), a whole-day duration, and BusyStatus 0
/// frees the time.
#[test]
fn all_day_event_folds_to_date_values() {
    let props = CalendarEventProps {
        all_day_event: true,
        start_time: Some("20260820T000000Z".to_owned()),
        end_time: Some("20260821T000000Z".to_owned()),
        busy_status: Some(0),
        subject: Some("Company Holiday".to_owned()),
        time_zone: Some(flat_utc8()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-2", &props);
    assert_eq!(
        event.start,
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 20).unwrap())
    );
    assert!(event.is_all_day());
    assert_eq!(event.duration, "P1D".parse::<Duration>().unwrap());
    assert_eq!(event.free_busy_status, FreeBusyStatus::Free);
}

/// A DST TZI folds per-instant: a July event takes the daylight offset
/// (+2h → `Etc/GMT-2`), a January one the standard offset (+1h →
/// `Etc/GMT-1`) — each event keeps the exact instant under a fixed-offset
/// zone, and the raw blob + rules survive in `extended` (kept structurally).
#[test]
fn dst_tzi_folds_to_the_offset_in_effect_at_the_event() {
    let july = CalendarEventProps {
        start_time: Some("20260715T090000Z".to_owned()),
        end_time: Some("20260715T100000Z".to_owned()),
        time_zone: Some(dst_cet()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-8", &july);
    assert_eq!(event.start, zoned("2026-07-15T11:00:00", "Etc/GMT-2"));

    let january = CalendarEventProps {
        start_time: Some("20260115T090000Z".to_owned()),
        end_time: Some("20260115T100000Z".to_owned()),
        time_zone: Some(dst_cet()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-9", &january);
    assert_eq!(event.start, zoned("2026-01-15T10:00:00", "Etc/GMT-1"));
}

/// Zone-policy edges: no Timezone element at all → the UTC fold (`Etc/UTC`,
/// the digits already are UTC); a present-but-unparseable blob → floating
/// (the bias is unknown — never guessed).
#[test]
fn missing_timezone_folds_utc_and_unparseable_folds_floating() {
    let utc = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        end_time: Some("20260818T100000Z".to_owned()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-10", &utc);
    assert_eq!(event.start, zoned("2026-08-18T09:00:00", "Etc/UTC"));

    let floating = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        end_time: Some("20260818T100000Z".to_owned()),
        time_zone: Some(TimeZoneBlob {
            raw_base64: Some("!!!not-base64!!!".to_owned()),
            parsed: None,
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-11", &floating);
    assert_eq!(
        event.start,
        CalendarDateTime::Floating("2026-08-18T09:00:00".parse().unwrap())
    );
}

/// Degradations stay loud and never drop the item: a missing StartTime
/// degrades to the epoch (the server fills real values at creation —
/// [MS-ASCAL] §3.2.4.4), an out-of-enum recurrence Type drops the rule but
/// keeps the event single-occurrence, and a missing UID falls back to the
/// ServerId (never an invented identity).
#[test]
fn malformed_values_degrade_without_dropping_the_item() {
    let no_start = CalendarEventProps {
        subject: Some("Kept".to_owned()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-12", &no_start);
    assert_eq!(event.title, "Kept");
    assert_eq!(event.start, zoned("1970-01-01T00:00:00", "Etc/UTC"));

    let bad_type = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 4, // not in the [MS-ASCAL] §2.2.2.45 enum
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-13", &bad_type);
    assert!(
        !event.is_recurring(),
        "the unexpressible rule is dropped, the event kept"
    );

    let no_uid = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-14", &no_uid);
    assert_eq!(event.uid, Uid::new("srv:ev-14").unwrap());
}

/// The MeetingStatus cancelled bit (C, value 4 — wire values {5,7,13,15})
/// maps to the engine's `cancelled` tombstone status; Sensitivity 3
/// (Confidential) maps to `secret`.
#[test]
fn cancelled_meeting_status_and_confidential_sensitivity_map() {
    let props = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        meeting_status: Some(5),
        sensitivity: Some(3),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-15", &props);
    assert_eq!(event.status, EventStatus::Cancelled);
    assert_eq!(event.privacy, Privacy::Secret);
}
