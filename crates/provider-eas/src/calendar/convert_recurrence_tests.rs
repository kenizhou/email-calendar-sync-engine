// SPDX-License-Identifier: MPL-2.0
//! Recurrence golden tests for the P2 Task-2 conversion: raw EAS
//! `Recurrence`/`Exceptions` → the engine's structural `Recurrence`, with
//! the expander's occurrence counts pinning each mapped rule (the contract
//! `engine-sync` drives). Split from `convert_tests.rs` at the 500-line cap.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceOverride, Weekday},
    time::LocalDateTime,
};
use engine_recurrence::{Horizon, expand};

use super::{
    CalendarEventProps, CalendarException, CalendarRecurrence, calendar_event_from_props,
    convert_tests::{flat_utc8, occurrence_count},
};

/// The occurrence START DATES (UTC) an event expands to over `[from, to)` —
/// the set-level pin the week-start test needs (a count alone cannot see
/// WKST move two of the four occurrences).
fn occurrence_dates(event: &engine_core::calendar::Event, from: &str, to: &str) -> Vec<String> {
    let horizon = Horizon::new(from.parse().unwrap(), to.parse().unwrap()).expect("horizon");
    let rows = expand(event, &horizon, &engine_core::time::TimeZoneId::utc())
        .expect("the rule is expandable");
    rows.iter()
        .map(|row| {
            format!(
                "{:04}-{:02}-{:02}",
                row.start.year(),
                row.start.month(),
                row.start.day()
            )
        })
        .collect()
}

/// A daily COUNT series (Type 0, Occurrences): the EAS count includes the
/// first occurrence — exactly RFC 5545/`RecurrenceBound::Count` semantics —
/// and the expander materializes exactly that many occurrences.
#[test]
fn daily_count_series_maps_and_expands() {
    let props = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        end_time: Some("20260818T093000Z".to_owned()),
        time_zone: Some(flat_utc8()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 0,
            interval: Some(1),
            occurrences: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-3", &props);
    let recurrence = event
        .recurrence
        .as_ref()
        .expect("the series carries a rule");
    assert_eq!(recurrence.rules.len(), 1);
    let rule = &recurrence.rules[0];
    assert_eq!(rule.frequency, Frequency::Daily);
    assert_eq!(rule.interval.get(), 1);
    assert_eq!(
        rule.bound,
        RecurrenceBound::Count(NonZeroU32::new(5).unwrap())
    );
    assert_eq!(
        occurrence_count(&event, "2026-08-01T00:00:00Z", "2027-01-01T00:00:00Z"),
        5,
        "COUNT=5 yields exactly five occurrences"
    );
}

/// A weekly BYDAY series bounded by Until (Type 1, mask 62 = Mon-Fri): the
/// mask expands to the five weekday `NDay`s, and the wire Until — a Compact
/// DateTime, i.e. UTC ([MS-ASDTYPE] §2.7.2) — folds to the fixed-offset
/// zone's wall clock.
#[test]
fn weekly_byday_series_maps_until_and_expands() {
    let props = CalendarEventProps {
        start_time: Some("20260818T090000Z".to_owned()),
        end_time: Some("20260818T100000Z".to_owned()),
        time_zone: Some(flat_utc8()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(62),
            until: Some("20261225T090000Z".to_owned()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-4", &props);
    let rule = &event.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Weekly);
    assert_eq!(
        rule.by_day,
        vec![
            NDay {
                day: Weekday::Mo,
                nth_of_period: None
            },
            NDay {
                day: Weekday::Tu,
                nth_of_period: None
            },
            NDay {
                day: Weekday::We,
                nth_of_period: None
            },
            NDay {
                day: Weekday::Th,
                nth_of_period: None
            },
            NDay {
                day: Weekday::Fr,
                nth_of_period: None
            },
        ]
    );
    // 09:00Z + 8h = the 17:00 wall clock the series generates at.
    assert_eq!(
        rule.bound,
        RecurrenceBound::Until("2026-12-25T17:00:00".parse().unwrap())
    );
    // 2026-08-18 is a Tuesday: the seven-day window holds Tue 18 .. Mon 24 —
    // five weekdays (Tue-Fri plus the following Monday).
    assert_eq!(
        occurrence_count(&event, "2026-08-18T00:00:00Z", "2026-08-25T00:00:00Z"),
        5
    );
}

/// A monthly nth-of-month series (Type 3: DayOfWeek Tuesday, WeekOfMonth 2):
/// the nth pattern maps to `BYDAY=2TU` structurally (an nth `NDay`, NOT
/// BYSETPOS — the expander supports the former and rejects the latter).
#[test]
fn monthly_nth_weekday_maps_to_nth_nday_and_expands() {
    let props = CalendarEventProps {
        start_time: Some("20260811T090000Z".to_owned()),
        end_time: Some("20260811T100000Z".to_owned()),
        time_zone: Some(flat_utc8()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 3,
            interval: Some(1),
            day_of_week: Some(4),
            week_of_month: Some(2),
            occurrences: Some(6),
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-5", &props);
    let rule = &event.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Monthly);
    assert_eq!(
        rule.by_day,
        vec![NDay {
            day: Weekday::Tu,
            nth_of_period: Some(NonZeroI32::new(2).unwrap()),
        }],
        "WeekOfMonth 1-4 becomes the BYDAY nth, never BYSETPOS"
    );
    assert!(rule.by_set_position.is_empty());
    // Aug 11 2026 through Jan 12 2027: six second-Tuesdays.
    assert_eq!(
        occurrence_count(&event, "2026-08-01T00:00:00Z", "2027-02-01T00:00:00Z"),
        6
    );
}

/// The DayOfWeek=127 special ([MS-ASCAL] §2.2.2.37.1 + §4.4/§4.5): with the
/// mask 127 the WeekOfMonth value IS the day of the month, and WeekOfMonth 5
/// (last) becomes BYMONTHDAY=-1 — the negative bymonthday the engine model
/// carries.
#[test]
fn day_of_week_127_maps_week_of_month_to_bymonthday() {
    let props = CalendarEventProps {
        start_time: Some("20260831T090000Z".to_owned()),
        end_time: Some("20260831T100000Z".to_owned()),
        time_zone: Some(flat_utc8()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 3,
            interval: Some(1),
            day_of_week: Some(127),
            week_of_month: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-6", &props);
    let rule = &event.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Monthly);
    assert!(rule.by_day.is_empty(), "mask 127 contributes no BYDAY");
    assert_eq!(rule.by_month_day, vec![-1], "WeekOfMonth 5 = last day");
    // Aug 31 .. Dec 31 2026: five month-ends.
    assert_eq!(
        occurrence_count(&event, "2026-08-01T00:00:00Z", "2027-01-05T00:00:00Z"),
        5
    );
}

/// Exceptions: a deleted occurrence becomes an `Excluded` override keyed by
/// the ORIGINAL occurrence's folded wall clock, and a moved occurrence
/// becomes a JSCalendar patch carrying its own start/duration/title — keyed
/// by the original's. Expansion drops the excluded instance and renders the
/// moved one at its new time.
#[test]
fn exceptions_fold_to_exclusion_and_moved_patch_overrides() {
    let props = CalendarEventProps {
        start_time: Some("20260811T090000Z".to_owned()),
        end_time: Some("20260811T100000Z".to_owned()),
        time_zone: Some(flat_utc8()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(4), // Tuesday
            occurrences: Some(4), // Aug 11, Aug 18, Aug 25, Sep 1
            ..Default::default()
        }),
        exceptions: vec![
            CalendarException {
                deleted: true,
                exception_start_time: Some("20260818T090000Z".to_owned()),
                ..Default::default()
            },
            CalendarException {
                deleted: false,
                exception_start_time: Some("20260901T090000Z".to_owned()),
                start_time: Some("20260902T100000Z".to_owned()),
                end_time: Some("20260902T103000Z".to_owned()),
                subject: Some("Moved later".to_owned()),
                all_day_event: Some(false),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:ev-7", &props);
    let recurrence = event.recurrence.as_ref().unwrap();

    // The override keys are the ORIGINAL occurrences' wall clocks (UTC+8).
    let aug18: LocalDateTime = "2026-08-18T17:00:00".parse().unwrap();
    let sep1: LocalDateTime = "2026-09-01T17:00:00".parse().unwrap();
    assert!(
        recurrence.is_excluded(&aug18),
        "the deleted instance is excluded"
    );
    let Some(RecurrenceOverride::Patch(patch)) = recurrence.overrides.get(&sep1) else {
        panic!(
            "the moved instance is a patch, got {:?}",
            recurrence.overrides.get(&sep1)
        );
    };
    assert_eq!(
        patch.get("start").and_then(serde_json::Value::as_str),
        Some("2026-09-02T18:00:00"),
        "10:00Z + 8h = the moved-to wall clock"
    );
    assert_eq!(
        patch.get("timeZone").and_then(serde_json::Value::as_str),
        Some("Etc/GMT-8")
    );
    assert_eq!(
        patch.get("duration").and_then(serde_json::Value::as_str),
        Some("PT30M")
    );
    assert_eq!(
        patch.get("title").and_then(serde_json::Value::as_str),
        Some("Moved later")
    );

    // Four instances minus the excluded one: three occurrences survive.
    assert_eq!(
        occurrence_count(&event, "2026-08-01T00:00:00Z", "2026-10-01T00:00:00Z"),
        3
    );
}

/// FirstDayOfWeek ([MS-ASCAL] §2.2.2.24) maps to `first_day_of_week` and is
/// OBSERVABLE in the expanded set: the RFC 5545 §3.8.5.3 WKST example as an
/// EAS shape — biweekly (Type 1, INTERVAL=2) SU+TU (mask 5), COUNT=4,
/// starting Tuesday 1997-08-05. A Monday week start yields Aug 5, 10, 19,
/// 24; a Sunday one yields Aug 5, 17, 19, 31 — the same count, a different
/// set, which is exactly why the wire element must survive the conversion.
#[test]
fn first_day_of_week_maps_and_moves_the_biweekly_set() {
    fn series(first_day_of_week: Option<u32>) -> CalendarEventProps {
        CalendarEventProps {
            start_time: Some("19970805T090000Z".to_owned()),
            end_time: Some("19970805T100000Z".to_owned()),
            recurrence: Some(CalendarRecurrence {
                recurrence_type: 1,
                interval: Some(2),
                day_of_week: Some(5), // Sunday (1) + Tuesday (4)
                occurrences: Some(4),
                first_day_of_week,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    let monday = calendar_event_from_props("fid-cal-1", "srv:wkst-mo", &series(Some(1)));
    let sunday = calendar_event_from_props("fid-cal-1", "srv:wkst-su", &series(Some(0)));

    assert_eq!(
        monday.recurrence.as_ref().unwrap().rules[0].first_day_of_week,
        Weekday::Mo,
        "FirstDayOfWeek 1 = Monday"
    );
    assert_eq!(
        sunday.recurrence.as_ref().unwrap().rules[0].first_day_of_week,
        Weekday::Su,
        "FirstDayOfWeek 0 = Sunday"
    );

    let window = ("1997-08-01T00:00:00Z", "1997-10-01T00:00:00Z");
    assert_eq!(
        occurrence_dates(&monday, window.0, window.1),
        vec!["1997-08-05", "1997-08-10", "1997-08-19", "1997-08-24"],
        "WKST=MO — the RFC 5545 §3.8.5.3 first set"
    );
    assert_eq!(
        occurrence_dates(&sunday, window.0, window.1),
        vec!["1997-08-05", "1997-08-17", "1997-08-19", "1997-08-31"],
        "WKST=SU — the same four-count, two occurrences moved"
    );
}

/// An absent or out-of-enum FirstDayOfWeek keeps the engine default (Monday,
/// `RecurrenceRule::new`'s) — the §2.2.2.24 value has no other honest
/// mapping, so the week start is loudly the default, never a guess.
#[test]
fn absent_or_invalid_first_day_of_week_keeps_the_monday_default() {
    let mut props = CalendarEventProps {
        start_time: Some("19970805T090000Z".to_owned()),
        end_time: Some("19970805T100000Z".to_owned()),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(4),
            first_day_of_week: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = calendar_event_from_props("fid-cal-1", "srv:wkst-none", &props);
    assert_eq!(
        event.recurrence.as_ref().unwrap().rules[0].first_day_of_week,
        Weekday::Mo,
        "absent → the engine default"
    );

    if let Some(rec) = props.recurrence.as_mut() {
        rec.first_day_of_week = Some(7); // outside the §2.2.2.24 enum {0..=6}
    }
    let event = calendar_event_from_props("fid-cal-1", "srv:wkst-bad", &props);
    assert_eq!(
        event.recurrence.as_ref().unwrap().rules[0].first_day_of_week,
        Weekday::Mo,
        "out-of-enum → warned away to the engine default"
    );
}
