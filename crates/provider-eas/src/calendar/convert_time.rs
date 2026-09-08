// SPDX-License-Identifier: MPL-2.0
//! The time fold: wire Compact-DateTime values (UTC by [MS-ASDTYPE] §2.7.2)
//! → the engine's wall-clock forms, and the [MS-ASDTYPE] §2.7.6 TZI blob → a
//! fixed-offset zone (P2 Task 2).
//!
//! ## Why a fixed-offset fold
//!
//! EAS stores UTC instants plus the originator's TZI zone ([MS-ASDTYPE]
//! §2.3.2: "the stored UTC corresponds to the first occurrence of the
//! series"). The engine time model is IANA-only, and a TZI blob carries only
//! the bias and DST transition rules — **no zone name** — so there is no
//! Windows→IANA tzdb to resolve it against (the CLDR table the Graph adapter
//! uses keys on names). Guessing an IANA zone from offsets risks a zone with
//! different transition history, so the fold is honest instead: the offset
//! **in effect at the event's start** (per-instant TZI arithmetic below)
//! becomes the event's fixed offset, named by the IANA `Etc/GMT±H`
//! fixed-offset zones (POSIX-inverted sign: UTC+8 is `Etc/GMT-8`). The
//! instant round-trips exactly, the first occurrence renders at the local
//! wall clock the organizer saw, and later occurrences of a DST-zone series
//! hold the start's offset year-round — the documented degradation ("fold to
//! fixed-offset semantics, keep the rule structurally": the raw blob survives
//! verbatim in `extended["eas/timezone"]`).
//!
//! A fractional-hour offset (no `Etc` zone exists at e.g. +05:30) folds to
//! UTC — the instant stays true; a present-but-unparseable blob folds to
//! **floating** (the bias is unknown, never guessed); no `Timezone` element
//! at all folds to UTC (the digits already are UTC).

use engine_core::time::{LocalDateTime, TimeZoneId};
use time::{Date, Month, PrimitiveDateTime, Time};

use super::model::{TimeZoneBlob, TziRule, TziTimeZone};

/// The epoch fallback for a missing/unparseable StartTime — [MS-ASCAL]
/// §3.2.4.4 has the server fill real values at creation, so wire items here
/// are already degenerate; the event is kept, loudly, at the epoch.
pub(super) fn epoch_wall() -> LocalDateTime {
    LocalDateTime::new(1970, 1, 1, 0, 0, 0).unwrap_or_else(|_| unreachable!("the epoch is valid"))
}

/// Parses a wire datetime ([MS-ASDTYPE] §2.7.2 Compact DateTime, plus the
/// defensive RFC 3339 form the parse layer admits) into its **UTC wall
/// clock** — the digits' value, which is UTC by spec. `None` when malformed.
pub(super) fn parse_wire_utc(value: &str) -> Option<LocalDateTime> {
    let digits: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'T' || *c == '-' || *c == ':')
        .collect();
    let bytes = digits.as_bytes();
    let (year, month, day, hour, minute, second) = if bytes.len() == 15 {
        (
            year_of(&digits[0..4])?,
            two_digits(&digits[4..6])?,
            two_digits(&digits[6..8])?,
            two_digits(&digits[9..11])?,
            two_digits(&digits[11..13])?,
            two_digits(&digits[13..15])?,
        )
    } else if bytes.len() == 19 {
        (
            year_of(&digits[0..4])?,
            two_digits(&digits[5..7])?,
            two_digits(&digits[8..10])?,
            two_digits(&digits[11..13])?,
            two_digits(&digits[14..16])?,
            two_digits(&digits[17..19])?,
        )
    } else {
        log::warn!("calendar conversion: unparseable datetime {value:?}");
        return None;
    };
    LocalDateTime::new(year, month, day, hour, minute, second)
        .map_err(|e| {
            log::warn!("calendar conversion: datetime {value:?} out of range: {e}");
            e
        })
        .ok()
}

/// Four ASCII digits as a year, `None` when non-numeric.
fn year_of(s: &str) -> Option<i32> {
    s.parse().ok()
}

/// Two ASCII digits as a month/day/hour/etc. field, `None` when non-numeric
/// (the range check is `LocalDateTime::new`'s — strictness beyond the ABNF is
/// the converter's job).
fn two_digits(s: &str) -> Option<u8> {
    s.parse().ok()
}

/// Applies a whole-minute offset to a wall clock (`wall + offset`).
pub(super) fn apply_offset(wall: LocalDateTime, offset_minutes: i32) -> LocalDateTime {
    let primitive = to_primitive(wall);
    let shifted = if offset_minutes >= 0 {
        primitive + time::Duration::minutes(i64::from(offset_minutes))
    } else {
        primitive - time::Duration::minutes(i64::from(-offset_minutes))
    };
    from_primitive(shifted)
}

/// The engine wall clock → the `time` crate's zoneless form (crate-local
/// arithmetic; the engine's own `as_primitive` is `pub(crate)`-internal).
pub(super) fn to_primitive(wall: LocalDateTime) -> PrimitiveDateTime {
    let date = Date::from_calendar_date(
        wall.year(),
        Month::try_from(wall.month()).unwrap_or(Month::January),
        wall.day(),
    )
    .unwrap_or_else(|_| unreachable!("an engine wall clock holds a real date"));
    let time = Time::from_hms(wall.hour(), wall.minute(), wall.second())
        .unwrap_or_else(|_| unreachable!("an engine wall clock holds a real time"));
    PrimitiveDateTime::new(date, time)
}

/// The `time` crate's zoneless form → the engine wall clock.
pub(super) fn from_primitive(primitive: PrimitiveDateTime) -> LocalDateTime {
    LocalDateTime::new(
        primitive.year(),
        primitive.month() as u8,
        primitive.day(),
        primitive.hour(),
        primitive.minute(),
        primitive.second(),
    )
    .unwrap_or_else(|_| unreachable!("a shifted wall clock stays in range for real events"))
}

// ============================================================================
// TZI offset arithmetic (the per-instant fold)
// ============================================================================

/// The local−UTC offset (minutes) in effect at `utc` under the TZI rules:
/// flat zones are constant; rule zones compare the most recent
/// standard/daylight onset at or before the instant, which handles
/// southern-hemisphere zones whose DST straddles the year boundary.
pub(super) fn offset_minutes_at(tzi: &TziTimeZone, utc: LocalDateTime) -> i32 {
    match (&tzi.standard, &tzi.daylight) {
        (None, None) => -tzi.base_bias_minutes,
        // A lone rule is a pathological blob (a real zone carries the pair);
        // its offset applies always.
        (Some(rule), None) | (None, Some(rule)) => {
            -(tzi.base_bias_minutes + rule.bias_offset_minutes)
        }
        (Some(std), Some(dl)) => {
            let std_offset = -(tzi.base_bias_minutes + std.bias_offset_minutes);
            let dl_offset = -(tzi.base_bias_minutes + dl.bias_offset_minutes);
            let year = utc.year();
            let mut best_std: Option<LocalDateTime> = None;
            let mut best_dl: Option<LocalDateTime> = None;
            for probe_year in [year - 1, year, year + 1] {
                // A transition's wall clock reads per the offset in effect
                // BEFORE it (the Windows TZI convention): the standard onset
                // is expressed in daylight time and vice versa.
                let std_onset = rule_onset_utc(probe_year, std, dl_offset);
                if std_onset <= utc {
                    best_std = Some(best_std.map_or(std_onset, |p| p.max(std_onset)));
                }
                let dl_onset = rule_onset_utc(probe_year, dl, std_offset);
                if dl_onset <= utc {
                    best_dl = Some(best_dl.map_or(dl_onset, |p| p.max(dl_onset)));
                }
            }
            match (best_std, best_dl) {
                (Some(s), Some(d)) if d >= s => dl_offset,
                (Some(_), _) => std_offset,
                (None, Some(_)) => dl_offset,
                (None, None) => {
                    log::warn!(
                        "calendar conversion: no TZI transition onset found before the event; \
                         defaulting to the standard offset"
                    );
                    std_offset
                }
            }
        }
    }
}

/// UTC wall clock of one yearly transition onset: the SYSTEMTIME fields
/// resolved to the `day_occurrence`-th `day_of_week` of `month` in `year`,
/// shifted by the offset that reads at that wall clock (the pre-transition
/// offset).
fn rule_onset_utc(year: i32, rule: &TziRule, prior_offset_minutes: i32) -> LocalDateTime {
    let day = nth_weekday_day(
        year,
        u32::from(rule.month),
        rule.day_of_week,
        rule.day_occurrence,
    );
    // The TZI parser validated the rule's ranges (month 1-12, hour ≤ 23,
    // minute ≤ 59), so these narrowings are total for any decoded rule.
    let month = u8::try_from(rule.month).unwrap_or(1);
    let hour = u8::try_from(rule.hour).unwrap_or(0);
    let minute = u8::try_from(rule.minute).unwrap_or(0);
    let wall = LocalDateTime::new(year, month, day, hour, minute, 0)
        .unwrap_or_else(|_| unreachable!("a validated TZI rule resolves to a real date"));
    // utc = wall − offset.
    apply_offset(wall, -prior_offset_minutes)
}

/// The day-of-month of the `occurrence`-th `day_of_week` (0=Sunday) in
/// `month`/`year`; `occurrence` 5 means the LAST such weekday.
pub(super) fn nth_weekday_day(year: i32, month: u32, day_of_week: u16, occurrence: u16) -> u8 {
    let month = Month::try_from(u8::try_from(month).unwrap_or(1)).unwrap_or(Month::January);
    let days_in_month = time::util::days_in_month(month, year);
    if (1..=4).contains(&occurrence) {
        // From the month head: the first matching weekday, +7 per occurrence.
        let first = Date::from_calendar_date(year, month, 1)
            .unwrap_or_else(|_| unreachable!("the month head is valid"));
        let delta = (i64::from(day_of_week) - sunday_index(first.weekday())).rem_euclid(7);
        let day = 1 + delta + 7 * i64::from(occurrence - 1);
        u8::try_from(day.min(i64::from(days_in_month))).unwrap_or(days_in_month)
    } else {
        // 5 = the last occurrence: walk back from the month tail.
        let last = Date::from_calendar_date(year, month, days_in_month)
            .unwrap_or_else(|_| unreachable!("the month tail is valid"));
        let delta = (sunday_index(last.weekday()) - i64::from(day_of_week)).rem_euclid(7);
        u8::try_from(i64::from(days_in_month) - delta).unwrap_or(days_in_month)
    }
}

/// A `time::Weekday` as the SYSTEMTIME wDayOfWeek index (0=Sunday..6=Saturday).
fn sunday_index(weekday: time::Weekday) -> i64 {
    match weekday {
        time::Weekday::Sunday => 0,
        time::Weekday::Monday => 1,
        time::Weekday::Tuesday => 2,
        time::Weekday::Wednesday => 3,
        time::Weekday::Thursday => 4,
        time::Weekday::Friday => 5,
        time::Weekday::Saturday => 6,
    }
}

// ============================================================================
// The fold itself
// ============================================================================

/// How one event's wire UTC values become engine wall clocks: a fixed offset
/// chosen at the event's start (see the module docs), plain UTC, or floating
/// when the TZI blob would not parse.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TimeFold {
    /// `wall = utc + offset`, zoned by the fixed-offset IANA name.
    Fixed {
        /// The offset in effect at the event's start (local−UTC, minutes).
        offset_minutes: i32,
        /// The `Etc/GMT±H` fixed-offset zone reading that offset.
        zone: TimeZoneId,
    },
    /// No `Timezone` element: the digits are UTC and stay UTC (`Etc/UTC`).
    Utc,
    /// A `Timezone` element whose blob did not parse: floating (no zone, no
    /// offsets — the bias is unknown, never guessed).
    Floating,
}

impl TimeFold {
    /// Chooses the fold for one event from its `Timezone` blob and start:
    /// a parsed TZI contributes the offset in effect at the start (a DST
    /// zone contributes ITS start-instant offset — the fixed fold); whole
    /// hours name an `Etc` zone, fractional hours degrade to UTC (the
    /// instant stays true).
    pub(super) fn choose(blob: Option<&TimeZoneBlob>, start_utc: LocalDateTime) -> Self {
        let Some(blob) = blob else {
            return Self::Utc;
        };
        let Some(tzi) = blob.parsed.as_ref() else {
            log::warn!(
                "calendar conversion: Timezone blob present but unparseable; folding floating \
                 (the parse layer already warned about the blob)"
            );
            return Self::Floating;
        };
        let offset = offset_minutes_at(tzi, start_utc);
        if offset.rem_euclid(60) != 0 {
            log::warn!(
                "calendar conversion: TZI offset {offset}m is fractional (no Etc fixed-offset \
                 zone); folding UTC — the instant stays true, the wall clock reads UTC"
            );
            return Self::Utc;
        }
        match fixed_zone_name(offset) {
            Some(name) => Self::Fixed {
                offset_minutes: offset,
                zone: name,
            },
            None => Self::Utc,
        }
    }

    /// The wall clock a wire UTC value folds to under this fold. Every value
    /// of one event folds by the SAME offset (the zone is fixed by
    /// construction — folding other instants per-instant would produce values
    /// the fixed zone cannot express).
    pub(super) fn wall(&self, utc: LocalDateTime) -> LocalDateTime {
        match self {
            Self::Fixed { offset_minutes, .. } => apply_offset(utc, *offset_minutes),
            Self::Utc | Self::Floating => utc,
        }
    }

    /// The zone a timed start carries under this fold (`None` = floating).
    pub(super) fn zone(&self) -> Option<TimeZoneId> {
        match self {
            Self::Fixed { zone, .. } => Some(zone.clone()),
            Self::Utc => Some(TimeZoneId::utc()),
            Self::Floating => None,
        }
    }
}

/// The IANA fixed-offset zone name for a whole-hour offset (POSIX-inverted
/// sign: +8h is `Etc/GMT-8`, −5h is `Etc/GMT+5`; 0 is `Etc/GMT`). `None` for
/// fractional offsets — the `Etc` family exists at whole hours only.
fn fixed_zone_name(offset_minutes: i32) -> Option<TimeZoneId> {
    let hours = offset_minutes / 60;
    let name = match hours {
        0 => "Etc/GMT".to_owned(),
        h if h > 0 => format!("Etc/GMT-{h}"),
        h => format!("Etc/GMT+{}", -h),
    };
    TimeZoneId::iana(name).ok()
}
