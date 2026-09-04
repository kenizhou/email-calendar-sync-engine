// SPDX-License-Identifier: MPL-2.0
//! Raw EAS `Recurrence`/`Exceptions` → the engine's structural
//! [`Recurrence`] (P2 Task 2).
//!
//! ## Recurrence mapping (every semantic per [MS-ASCAL] v20220429)
//!
//! `FREQ` from `Type` (§2.2.2.45 enum {0,1,2,3,5,6} — there is no value 4 and
//! no regenerate-after-done family in this revision): 0→`Daily` — but a
//! Type-0 recurrence **with** `DayOfWeek` IS weekly (§2.2.2.37.1, resolving
//! identically to Type 1, `Interval` counting weeks); 1→`Weekly`; 2/3→
//! `Monthly`; 5/6→`Yearly`. `DayOfWeek` bitmask (§2.2.2.15: 1=Sun..64=Sat;
//! 62=weekdays, 65=weekend days — plain bit combinations) → `by_day` in
//! canonical Mon→Sun order. Nth patterns (§2.2.2.48): `WeekOfMonth` 1-4
//! becomes the **nth `NDay`** (`BYDAY=2TU` structurally — never `BYSETPOS`,
//! which the engine's expander rejects), 5 (last) becomes −1. The
//! `DayOfWeek`=127 special (§2.2.2.37.1 + §4.4/§4.5): there `WeekOfMonth`
//! IS the day of the month → `by_month_day` (5 → −1, the last day — the
//! negative bymonthday EAS expresses). `DayOfMonth` → `by_month_day`,
//! `MonthOfYear` → `by_month`. Bounds (§2.2.2.47): `Occurrences` →
//! `Count` (the EAS count includes the first occurrence — exactly RFC 5545
//! COUNT semantics); `Until` — a Compact DateTime, i.e. UTC ([MS-ASDTYPE]
//! §2.7.2) — folds to the fixed-offset zone's wall clock. Both present
//! (mutually exclusive per spec): `Until` wins, loudly.
//!
//! Degrade loud, never garbage: an out-of-enum `Type`, a missing
//! §2.2.2.37.1-required part, or an unexpressible value warns and drops the
//! rule — the event stays single-occurrence (the `calendar-semantics.md`
//! isolate rule), and its exceptions are skipped with it.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::{
    calendar::{
        Frequency, NDay, Recurrence, RecurrenceBound, RecurrenceOverride, RecurrenceRule, Weekday,
    },
    time::{CalendarDate, CalendarDateTime, LocalDateTime},
};

use super::{
    convert_time::{TimeFold, epoch_wall, parse_wire_utc},
    model::{CalendarEventProps, CalendarException, CalendarRecurrence},
};

/// The calendar bit-flags in canonical Mon→Sun order: bit value → weekday
/// ([MS-ASCAL] §2.2.2.15: 1=Sun 2=Mon 4=Tue 8=Wed 16=Thu 32=Fri 64=Sat; the
/// specials 62/65/127 are bit combinations that expand normally).
const MASK_BITS_MON_FIRST: [(u32, Weekday); 7] = [
    (2, Weekday::Mo),
    (4, Weekday::Tu),
    (8, Weekday::We),
    (16, Weekday::Th),
    (32, Weekday::Fr),
    (64, Weekday::Sa),
    (1, Weekday::Su),
];

/// Builds the event's recurrence: the rule (when the wire carries one) plus
/// the exceptions folded as per-instance overrides. `None` when the wire
/// carried no rule — a plain single event. Exceptions only make sense
/// against a rule, so an unexpressible rule (warned, dropped by `rule_from`)
/// skips the exceptions with it — a lone override would otherwise ADD a
/// stray instance instead of moving one.
pub(super) fn recurrence_from_props(
    props: &CalendarEventProps,
    fold: &TimeFold,
    all_day: bool,
) -> Option<Recurrence> {
    let Some(rule) = props
        .recurrence
        .as_ref()
        .and_then(|rec| rule_from(rec, fold, all_day))
    else {
        if !props.exceptions.is_empty() {
            log::warn!(
                "calendar conversion: item carries exceptions without an expressible \
                 recurrence rule; skipping them (an override without its rule would add a \
                 stray instance instead of moving one)"
            );
        }
        return None;
    };
    let mut recurrence = Recurrence::from_rule(rule);
    let series_start_wall = start_wall(props, fold);
    for exception in &props.exceptions {
        fold_exception(exception, fold, all_day, series_start_wall, &mut recurrence);
    }
    Some(recurrence)
}

/// The series' start wall clock (the fallback duration anchor an exception
/// without its own start uses): epoch when the item carried no StartTime.
fn start_wall(props: &CalendarEventProps, fold: &TimeFold) -> LocalDateTime {
    props
        .start_time
        .as_deref()
        .and_then(parse_wire_utc)
        .map_or_else(epoch_wall, |utc| fold.wall(utc))
}

// ============================================================================
// Recurrence container → RecurrenceRule
// ============================================================================

/// Maps one raw `Recurrence` container, or `None` (warned) when the wire data
/// has no structural equivalent — the caller keeps the event
/// single-occurrence.
fn rule_from(rec: &CalendarRecurrence, fold: &TimeFold, all_day: bool) -> Option<RecurrenceRule> {
    // §2.2.2.37.1: a Type-0 recurrence WITH DayOfWeek IS a weekly
    // recurrence (Interval counts weeks).
    let frequency = match rec.recurrence_type {
        0 if rec.day_of_week.is_some() => Frequency::Weekly,
        0 => Frequency::Daily,
        1 => Frequency::Weekly,
        2 | 3 => Frequency::Monthly,
        5 | 6 => Frequency::Yearly,
        other => {
            log::warn!(
                "calendar conversion: Recurrence Type {other} outside the [MS-ASCAL] \
                 §2.2.2.45 enum {{0,1,2,3,5,6}} — no engine rule expresses it; the event \
                 stays single-occurrence"
            );
            return None;
        }
    };
    let mut rule = RecurrenceRule::new(frequency);
    // Interval 0 is degenerate (§2.2.2.25 admits it; an engine rule's
    // interval must be positive) — treated as 1, loudly.
    if rec.interval == Some(0) {
        log::warn!("calendar conversion: Recurrence Interval 0 is degenerate; treating it as 1");
    }
    if let Some(interval) = rec.interval.filter(|n| *n > 1) {
        rule.interval = NonZeroU32::new(interval)?;
    }

    // Per-Type BYxxx parts; a missing §2.2.2.37.1-required part or an
    // unexpressible value aborts the whole rule (warned). A Type 0/1 without
    // DayOfWeek stays expressible (daily / weekly on the start weekday), so
    // only the other Types enforce their required parts.
    match rec.recurrence_type {
        0 | 1 => {
            if let Some(mask) = rec.day_of_week {
                rule.by_day = plain_ndays(mask)?;
            }
        }
        2 => {
            let day = required(rec.day_of_month, "DayOfMonth", 2)?;
            rule.by_month_day = vec![bymonthday(day)?];
        }
        3 | 6 => {
            if rec.recurrence_type == 6 {
                let month = required(rec.month_of_year, "MonthOfYear", 6)?;
                rule.by_month = vec![bymonth(month)?];
            }
            let mask = required(rec.day_of_week, "DayOfWeek", rec.recurrence_type)?;
            let week = required(rec.week_of_month, "WeekOfMonth", rec.recurrence_type)?;
            if mask == 127 {
                // §2.2.2.37.1 + §4.4/§4.5: WeekOfMonth IS the day of the
                // month; 5 = the last day → BYMONTHDAY=-1.
                rule.by_month_day = vec![week_of_month_value(week)?];
            } else {
                let nth = week_of_month_value(week)?;
                rule.by_day = plain_ndays(mask)?
                    .into_iter()
                    .map(|nday| NDay {
                        day: nday.day,
                        nth_of_period: NonZeroI32::new(nth),
                    })
                    .collect();
            }
        }
        5 => {
            let month = required(rec.month_of_year, "MonthOfYear", 5)?;
            let day = required(rec.day_of_month, "DayOfMonth", 5)?;
            rule.by_month = vec![bymonth(month)?];
            rule.by_month_day = vec![bymonthday(day)?];
        }
        _ => unreachable!("out-of-enum Type returned None above"),
    }

    // Bound: Until and Occurrences are mutually exclusive (§2.2.2.47); if
    // the wire carried both anyway, Until wins (an engine rule carries
    // exactly one bound).
    if rec.until.is_some() && rec.occurrences.is_some() {
        log::warn!(
            "calendar conversion: Recurrence carries both Until and Occurrences \
             (mutually exclusive per [MS-ASCAL] §2.2.2.47); keeping Until"
        );
    }
    if let Some(until) = &rec.until {
        if let Some(bound) = until_bound(until, fold, all_day) {
            rule.bound = bound;
        } else {
            log::warn!(
                "calendar conversion: unparseable Until {until:?}; the series stays unbounded"
            );
        }
    } else if let Some(count) = rec.occurrences.filter(|n| *n > 0) {
        rule.bound = RecurrenceBound::Count(NonZeroU32::new(count)?);
    } else if rec.occurrences == Some(0) {
        log::warn!(
            "calendar conversion: Recurrence Occurrences is 0 — degenerate; the series stays \
             unbounded"
        );
    }
    Some(rule)
}

/// A §2.2.2.37.1-required recurrence part: present, else warn + `None`
/// (which drops the whole rule — the conservative degradation).
fn required(part: Option<u32>, name: &str, rec_type: u8) -> Option<u32> {
    if let Some(value) = part {
        return Some(value);
    }
    log::warn!(
        "calendar conversion: Recurrence Type {rec_type} requires {name} per [MS-ASCAL] \
         §2.2.2.37.1 but the wire omitted it; dropping the rule (the event stays \
         single-occurrence)"
    );
    None
}

/// [MS-ASCAL] §2.2.2.15 DayOfWeek bitmask → plain `NDay`s in canonical
/// Mon→Sun order. Mask 0 or > 127 → warn + `None` (drops the rule).
fn plain_ndays(mask: u32) -> Option<Vec<NDay>> {
    if mask == 0 || mask > 127 {
        log::warn!(
            "calendar conversion: DayOfWeek mask {mask} outside [MS-ASCAL] §2.2.2.15 1..=127; \
             dropping the rule"
        );
        return None;
    }
    Some(
        MASK_BITS_MON_FIRST
            .iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, day)| NDay {
                day: *day,
                nth_of_period: None,
            })
            .collect(),
    )
}

/// Range-checked BYMONTHDAY ([MS-ASCAL] §2.2.2.14 1-31).
fn bymonthday(day_of_month: u32) -> Option<i32> {
    if (1..=31).contains(&day_of_month) {
        // Range-checked above, so the widening is total.
        Some(i32::try_from(day_of_month).ok()?)
    } else {
        log::warn!(
            "calendar conversion: DayOfMonth {day_of_month} outside [MS-ASCAL] §2.2.2.14 \
             1..=31; dropping the rule"
        );
        None
    }
}

/// Range-checked BYMONTH ([MS-ASCAL] §2.2.2.29 1-12), as the engine's
/// 1-based month string.
fn bymonth(month_of_year: u32) -> Option<String> {
    if (1..=12).contains(&month_of_year) {
        Some(month_of_year.to_string())
    } else {
        log::warn!(
            "calendar conversion: MonthOfYear {month_of_year} outside [MS-ASCAL] §2.2.2.29 \
             1..=12; dropping the rule"
        );
        None
    }
}

/// [MS-ASCAL] §2.2.2.48 WeekOfMonth → the positional value: 1-4 = nth, 5 =
/// last (−1); anything else → warn + `None` (drops the rule).
fn week_of_month_value(week_of_month: u32) -> Option<i32> {
    match week_of_month {
        // Range-checked by the match arms, so the widening is total.
        1..=4 => Some(i32::try_from(week_of_month).ok()?),
        5 => Some(-1),
        other => {
            log::warn!(
                "calendar conversion: WeekOfMonth {other} outside [MS-ASCAL] §2.2.2.48 1..=5; \
                 dropping the rule"
            );
            None
        }
    }
}

/// The `Until` bound ([MS-ASCAL] §2.2.2.47: the start time of the LAST
/// instance) — a Compact DateTime, i.e. UTC ([MS-ASDTYPE] §2.7.2), folded to
/// the fixed-offset zone's wall clock (an all-day series keys at the date's
/// midnight). `None` when unparseable.
fn until_bound(wire: &str, fold: &TimeFold, all_day: bool) -> Option<RecurrenceBound> {
    let utc = parse_wire_utc(wire)?;
    let wall = if all_day {
        LocalDateTime::new(utc.year(), utc.month(), utc.day(), 0, 0, 0).ok()?
    } else {
        fold.wall(utc)
    };
    Some(RecurrenceBound::Until(wall))
}

// ============================================================================
// Exceptions → per-instance overrides
// ============================================================================

/// Folds one exception into `recurrence`. A deleted occurrence becomes
/// [`RecurrenceOverride::Excluded`]; a modified one a JSCalendar patch
/// (start/duration/title/description/location — an absent child means
/// "unchanged", per §2.2.2.21 the modified subset). The map key is the
/// ORIGINAL occurrence's folded wall clock — the same values the rule
/// generates, so the expander's instance keys match.
fn fold_exception(
    exception: &CalendarException,
    fold: &TimeFold,
    all_day: bool,
    series_start_wall: LocalDateTime,
    recurrence: &mut Recurrence,
) {
    let Some(original_utc) = exception
        .exception_start_time
        .as_deref()
        .and_then(parse_wire_utc)
    else {
        log::warn!(
            "calendar conversion: exception without a parseable ExceptionStartTime; skipping it"
        );
        return;
    };
    // An all-day occurrence keys at its date's midnight ([MS-ASDTYPE]
    // §2.3.1: all-day bounds arrive as UTC midnight); a timed one at the
    // folded wall clock.
    let key = if all_day {
        LocalDateTime::new(
            original_utc.year(),
            original_utc.month(),
            original_utc.day(),
            0,
            0,
            0,
        )
        .unwrap_or(series_start_wall)
    } else {
        fold.wall(original_utc)
    };
    if exception.deleted {
        recurrence
            .overrides
            .insert(key, RecurrenceOverride::Excluded);
        return;
    }
    if let Some(patch) = exception_patch(exception, fold, all_day) {
        recurrence.overrides.insert(key, patch);
    }
}

/// Builds the modified occurrence's patch. `None` (warned/skipped) when the
/// exception changed nothing expressible — an override with no fields is a
/// pointless no-op, not data.
fn exception_patch(
    exception: &CalendarException,
    fold: &TimeFold,
    all_day: bool,
) -> Option<RecurrenceOverride> {
    let mut builder = engine_core::calendar::OverrideBuilder::new();
    let mut moved = false;
    // The occurrence's own start, when it moved (absent = unchanged — the
    // expander falls back to the recurrence id).
    let effective_start_utc = exception.start_time.as_deref().and_then(parse_wire_utc);
    if let Some(moved_utc) = effective_start_utc {
        let start = if all_day {
            CalendarDateTime::Date(
                CalendarDate::new(moved_utc.year(), moved_utc.month(), moved_utc.day()).ok()?,
            )
        } else if let Some(zone) = fold.zone() {
            CalendarDateTime::Zoned {
                local: fold.wall(moved_utc),
                zone,
            }
        } else {
            CalendarDateTime::Floating(fold.wall(moved_utc))
        };
        builder = builder.start(&start);
        moved = true;
    }
    // Its own length, when the wire carries an end: measured from its own
    // start when present, else the original occurrence (the fallback
    // §2.2.2.21 prescribes for an absent StartTime).
    if let Some(end_utc) = exception.end_time.as_deref().and_then(parse_wire_utc) {
        let anchor_utc = effective_start_utc.or_else(|| {
            exception
                .exception_start_time
                .as_deref()
                .and_then(parse_wire_utc)
        });
        if let Some(anchor_utc) = anchor_utc {
            let (start_value, end_value) = if all_day {
                (
                    CalendarDateTime::Date(
                        CalendarDate::new(anchor_utc.year(), anchor_utc.month(), anchor_utc.day())
                            .ok()?,
                    ),
                    CalendarDateTime::Date(
                        CalendarDate::new(end_utc.year(), end_utc.month(), end_utc.day()).ok()?,
                    ),
                )
            } else {
                let anchor = fold.wall(anchor_utc);
                let end = fold.wall(end_utc);
                match fold.zone() {
                    Some(zone) => (
                        CalendarDateTime::Zoned {
                            local: anchor,
                            zone: zone.clone(),
                        },
                        CalendarDateTime::Zoned { local: end, zone },
                    ),
                    None => (
                        CalendarDateTime::Floating(anchor),
                        CalendarDateTime::Floating(end),
                    ),
                }
            };
            if let Ok(duration) = start_value.duration_until(&end_value) {
                builder = builder.duration(duration);
                moved = true;
            } else {
                log::warn!(
                    "calendar conversion: exception end does not follow its start; keeping the \
                     series length"
                );
            }
        }
    }
    if let Some(subject) = &exception.subject {
        builder = builder.title(subject.clone());
        moved = true;
    }
    if let Some(body) = &exception.body_plain {
        builder = builder.description(body.clone());
        moved = true;
    }
    if let Some(location) = &exception.location {
        builder = builder.location_named(location.clone());
        moved = true;
    }
    if !moved {
        log::debug!(
            "calendar conversion: exception carries no modified fields; skipping the no-op \
             override"
        );
        return None;
    }
    builder.build().ok()
}
