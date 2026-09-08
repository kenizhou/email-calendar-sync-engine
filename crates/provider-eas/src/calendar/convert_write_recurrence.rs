// SPDX-License-Identifier: MPL-2.0
//! The engine's structural [`RecurrenceRule`] → the raw EAS `Recurrence`
//! container (P2 Task 3) — the write twin of `convert_recurrence.rs`.
//!
//! ## Mapping (the inverse of `rule_from`, every cite shared with it)
//!
//! `FREQ` → `Type` ([MS-ASCAL] §2.2.2.45): Weekly→1, Monthly→2 (day of
//! month) or 3 (nth weekday), Yearly→5 (date) or 6 (nth weekday); Daily→0 —
//! but a daily rule **with** `by_day` has no EAS form (§2.2.2.37.1's Type-0
//! `DayOfWeek` resolves as *weekly*), so it is refused rather than
//! silently re-timed. `BYDAY` → the `DayOfWeek` bitmask (§2.2.2.15; plain
//! entries only — an `nth` entry is expressible exactly on the monthly
//! positional and yearly types). `FirstDayOfWeek` (§2.2.2.24) is written
//! for weekly rules whose week start differs from the Monday default —
//! the WKST counterpart the read side folds back out.
//!
//! Bounds (§2.2.2.47): `Count` → `Occurrences` (spec max 999); `Until` →
//! the wire Compact-DateTime UTC string, from the caller-resolved instant
//! when the draft carries one ([`DraftRecurrence::until`] — the trait's
//! uniform carriage) and otherwise derived through the event's own
//! **fixed-offset** zone, which is exact without tzdata (the D6 fold: an
//! EAS event's zone is a fixed offset by construction, so wall − offset is
//! the instant; an adapter carrying a named-DST zone never reaches here).
//!
//! Degrade loud, never flatten: sub-daily frequencies, `RSCALE`/`skip`,
//! `BYSETPOS`/`BYYEARDAY`/`BYWEEKNO`/`BYHOUR`/`BYMINUTE`/`BYSECOND`, a
//! `BYDAY` entry an nth-position form cannot carry, and multi-part
//! monthly/yearly rules are all refused — a rule that would silently
//! expand differently on the server than it does locally is corruption,
//! not a degrade.

use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
    time::{LocalDateTime, UtcDateTime},
};
use engine_provider::{ProviderError, ProviderResult};

use super::model::CalendarRecurrence;

/// The [MS-ASCAL] §2.2.2.32 `Occurrences` spec maximum.
const MAX_OCCURRENCES: u32 = 999;

/// The DayOfWeek bit for one weekday ([MS-ASCAL] §2.2.2.15: 1=Sunday …
/// 64=Saturday — the mask values, not the FirstDayOfWeek indices).
fn day_bit(day: Weekday) -> u32 {
    match day {
        Weekday::Su => 1,
        Weekday::Mo => 2,
        Weekday::Tu => 4,
        Weekday::We => 8,
        Weekday::Th => 16,
        Weekday::Fr => 32,
        Weekday::Sa => 64,
    }
}

/// The FirstDayOfWeek index for one weekday ([MS-ASCAL] §2.2.2.24: 0=Sunday
/// … 6=Saturday — the SYSTEMTIME wDayOfWeek convention).
fn first_day_index(day: Weekday) -> u32 {
    match day {
        Weekday::Su => 0,
        Weekday::Mo => 1,
        Weekday::Tu => 2,
        Weekday::We => 3,
        Weekday::Th => 4,
        Weekday::Fr => 5,
        Weekday::Sa => 6,
    }
}

/// Maps one rule onto the wire container.
///
/// `resolved_until` is the caller-resolved `UNTIL` instant a
/// [`DraftRecurrence`](engine_provider::DraftRecurrence) carries, when the
/// rule came from a create or a recurrence edit. `until_wall_to_utc` folds a
/// wall-clock bound to the wire's UTC digits for rules that arrived without
/// one (a base rebuilt from downsync) — and for draft rules too, since an
/// EAS event's zone is a fixed offset and the arithmetic is exact.
///
/// # Errors
///
/// Refuses [`FailureClass::Permanent`] for every rule shape the wire cannot
/// express (see the module docs) — never a silent flattening.
pub(super) fn rule_to_wire(
    rule: &RecurrenceRule,
    resolved_until: Option<&UtcDateTime>,
    until_wall_to_wire: &dyn Fn(LocalDateTime) -> String,
) -> ProviderResult<CalendarRecurrence> {
    refuse_unsupported_parts(rule)?;
    let mut wire = CalendarRecurrence {
        recurrence_type: 0,
        interval: (rule.interval.get() > 1).then_some(rule.interval.get()),
        ..CalendarRecurrence::default()
    };
    match rule.frequency {
        Frequency::Daily => {
            refuse(
                !rule.by_day.is_empty(),
                "a daily rule restricted by weekday has no EAS form — the wire's Type-0 \
                 DayOfWeek resolves as weekly ([MS-ASCAL] §2.2.2.37.1); widen the rule to \
                 weekly or drop the day restriction",
            )?;
            wire.recurrence_type = 0;
        }
        Frequency::Weekly => {
            refuse(
                !rule.by_month.is_empty() || !rule.by_month_day.is_empty(),
                "a weekly rule with month/month-day parts has no EAS form",
            )?;
            wire.recurrence_type = 1;
            if !rule.by_day.is_empty() {
                wire.day_of_week = Some(day_mask(&rule.by_day)?);
            }
            // §2.2.2.24: the week's first day — load-bearing for INTERVAL>1
            // weekly rules. The Monday default is the engine's, so only a
            // differing start is written.
            if rule.first_day_of_week != Weekday::Mo {
                wire.first_day_of_week = Some(first_day_index(rule.first_day_of_week));
            }
        }
        Frequency::Monthly => {
            refuse(
                !rule.by_month.is_empty(),
                "a monthly rule with a month part has no EAS form (the month is implied)",
            )?;
            match (single_by_month_day(rule), single_positional(rule)) {
                (Some(day), None) => {
                    wire.recurrence_type = 2;
                    wire.day_of_month = Some(day);
                }
                (None, Some((bit, nth))) => {
                    wire.recurrence_type = 3;
                    wire.day_of_week = Some(bit);
                    wire.week_of_month = Some(nth);
                }
                _ => refuse(
                    true,
                    "a monthly rule needs exactly one of BYMONTHDAY (a plain day) or a single \
                     positional BYDAY (the nth weekday) — mixed or multi-part rules have no EAS \
                     form",
                )?,
            }
        }
        Frequency::Yearly => {
            let Some(month) = single_by_month(rule) else {
                return Err(refusal(
                    "a yearly rule needs exactly one BYMONTH — an every-month-of-the-year \
                     rule has no single EAS container",
                ));
            };
            wire.month_of_year = Some(month);
            if let Some(day) = single_by_month_day(rule) {
                wire.recurrence_type = 5;
                wire.day_of_month = Some(day);
            } else {
                let Some((bit, nth)) = single_positional(rule) else {
                    return Err(refusal(
                        "a yearly rule needs a BYMONTHDAY (Type 5) or a single positional \
                         BYDAY (Type 6)",
                    ));
                };
                wire.recurrence_type = 6;
                wire.day_of_week = Some(bit);
                wire.week_of_month = Some(nth);
            }
        }
        other => {
            refuse(
                true,
                &format!(
                    "a {other:?}-frequency rule has no EAS form — the wire's Type enum is \
                     daily/weekly/monthly/yearly only ([MS-ASCAL] §2.2.2.45)"
                ),
            )?;
        }
    }
    bind_bound(rule, resolved_until, &mut wire, until_wall_to_wire)?;
    Ok(wire)
}

/// The up-front refusal set: parts no EAS container can carry at all.
fn refuse_unsupported_parts(rule: &RecurrenceRule) -> ProviderResult<()> {
    refuse(
        rule.rscale.is_some(),
        "a non-Gregorian RSCALE rule has no EAS form",
    )?;
    refuse(
        !rule.by_year_day.is_empty()
            || !rule.by_week_no.is_empty()
            || !rule.by_hour.is_empty()
            || !rule.by_minute.is_empty()
            || !rule.by_second.is_empty()
            || !rule.by_set_position.is_empty(),
        "BYYEARDAY/BYWEEKNO/BYHOUR/BYMINUTE/BYSECOND/BYSETPOS have no EAS form",
    )
}

/// The bound → `Until` XOR `Occurrences` ([MS-ASCAL] §2.2.2.47).
fn bind_bound(
    rule: &RecurrenceRule,
    resolved_until: Option<&UtcDateTime>,
    wire: &mut CalendarRecurrence,
    until_wall_to_wire: &dyn Fn(LocalDateTime) -> String,
) -> ProviderResult<()> {
    match &rule.bound {
        RecurrenceBound::Unbounded => {}
        RecurrenceBound::Count(count) => {
            refuse(
                count.get() > MAX_OCCURRENCES,
                &format!(
                    "a count of {} exceeds the [MS-ASCAL] §2.2.2.32 Occurrences maximum of \
                     {MAX_OCCURRENCES}",
                    count.get()
                ),
            )?;
            wire.occurrences = Some(count.get());
        }
        RecurrenceBound::Until(wall) => {
            // The caller-resolved instant is authoritative when present; an
            // EAS event's zone is a fixed offset, so the wall clock derives
            // the same instant exactly otherwise (never a guessed zone).
            wire.until = Some(match resolved_until {
                Some(instant) => super::convert_write::compact_utc_of_instant(instant),
                None => until_wall_to_wire(*wall),
            });
        }
    }
    Ok(())
}

/// The plain-day BYMONTHDAY, when the rule carries exactly one.
fn single_by_month_day(rule: &RecurrenceRule) -> Option<u32> {
    match rule.by_month_day.as_slice() {
        [day] => u32::try_from(*day).ok().filter(|d| (1..=31).contains(d)),
        _ => None,
    }
}

/// The single BYMONTH entry as its 1-12 number ([MS-ASCAL] §2.2.2.29).
fn single_by_month(rule: &RecurrenceRule) -> Option<u32> {
    match rule.by_month.as_slice() {
        [month] => month.trim_end_matches('L').parse().ok(),
        _ => None,
    }
}

/// The single positional `BYDAY` entry (one weekday with an nth), as its
/// (bitmask, WeekOfMonth) pair — nth 1-4 direct, −1 the §2.2.2.48 "last"
/// (5).
fn single_positional(rule: &RecurrenceRule) -> Option<(u32, u32)> {
    match rule.by_day.as_slice() {
        [
            NDay {
                day,
                nth_of_period: Some(nth),
            },
        ] => {
            let nth = nth.get();
            let week = match nth {
                -1 => 5,
                1..=4 => nth,
                _ => return None,
            };
            u32::try_from(week).ok().map(|w| (day_bit(*day), w))
        }
        _ => None,
    }
}

/// The DayOfWeek bitmask for plain `BYDAY` entries (no nth).
fn day_mask(days: &[NDay]) -> ProviderResult<u32> {
    let mut mask = 0u32;
    for entry in days {
        refuse(
            entry.nth_of_period.is_some(),
            "an nth weekday inside a weekly rule has no EAS form ([MS-ASCAL] §2.2.2.15 masks \
             carry no position)",
        )?;
        mask |= day_bit(entry.day);
    }
    Ok(mask)
}

/// Refuse when `condition` holds — the shared loud-degrade helper.
fn refuse(condition: bool, detail: &str) -> ProviderResult<()> {
    if condition {
        Err(refusal(detail))
    } else {
        Ok(())
    }
}

/// The refusal itself, for diverging positions a `bool` gate cannot reach.
fn refusal(detail: &str) -> ProviderError {
    ProviderError::permanent(format!(
        "the EAS calendar write cannot express this recurrence: {detail}"
    ))
}
