// SPDX-License-Identifier: MPL-2.0
//! Write-direction conversion: the engine's neutral shapes → the EAS write
//! model [`CalendarEventWrite`] (P2 Task 3) — the write twin of
//! `convert.rs`.
//!
//! ## The fold, reversed
//!
//! The read side folds wire UTC + TZI into a **fixed-offset** zone
//! (`convert_time.rs` — the D6 fold: no zone name rides a TZI blob, so the
//! honest zone is the start-instant offset named by `Etc/GMT±H`). The write
//! side folds back through the same arithmetic: a zoned value accepts
//! exactly the fixed-offset family (plus `Etc/UTC`), a floating value pins
//! to UTC (the wire cannot express "no zone"), and an all-day date folds to
//! UTC midnights ([MS-ASDTYPE] §2.3.1). A named-DST zone start is refused —
//! resolving it needs tzdata no adapter carries, and a guessed offset is a
//! silently moved meeting. The TZI blob written is always the flat
//! fixed-offset one ([`build_fixed_offset_tzi_base64`]) — the write side
//! never re-derives DST rules, so a DST blob read in degrades to its
//! start-instant offset on the way back out (one consistent degradation,
//! both directions).
//!
//! ## The document discipline
//!
//! An EAS upsync `Change` is safest as a **complete** `ApplicationData`:
//! whether a given server ghosts omitted elements or replaces the item
//! wholesale, every field the model carries is present either way. So a
//! patch rebuilds the whole master from `base` + the patch overlay — the
//! EAS-native facts that have no first-class `Event` field ride back from
//! `extended["eas/*"]` (busy-status, sensitivity), the attendee list rides
//! without the organizer (server-managed — live-probed Status 6, see
//! `calendar_write`'s module docs), and the series' exceptions ride the
//! `Exceptions` container rebuilt from the base's structural overrides.
//! What cannot be rebuilt faithfully is **refused**: an alert with no
//! `Reminder` form, a rule union, an override patch carrying fields the
//! read side never produces. Never a silent flattening.
//!
//! ## Instances
//!
//! EAS has no per-occurrence object: an occurrence edit or delete is a
//! `Change` of the **master** whose `Exceptions` container carries the
//! occurrence as a modified exception ([MS-ASCAL] §2.2.2.21) or a deleted
//! marker (§2.2.2.16 — the EXDATE form). The master's other overrides ride
//! untouched, so a series edit never costs the user a per-occurrence change
//! by this adapter's own construction (the `OverrideSurvival::kept()`
//! claim's evidence).

use engine_core::{
    calendar::{Alert, Event, Participant, ParticipantRole},
    time::{CalendarDateTime, Duration, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDraft, EventPatch, ProviderError, ProviderResult, TextEdit};
use serde_json::Value;

pub(crate) use super::convert_write_exceptions::{write_exception, write_occurrence_deleted};
use super::{
    convert_time::{apply_offset, from_primitive, to_primitive},
    convert_write_exceptions::exceptions_of,
    convert_write_recurrence::rule_to_wire,
    model::CalendarAttendee,
};
use crate::calendar_write::{CalendarEventWrite, build_fixed_offset_tzi_base64};

// ============================================================================
// The fold primitives
// ============================================================================

/// The fixed local−UTC offset (minutes) an `Etc/GMT±H`-family zone reads,
/// `None` for everything else (named-DST zones, custom zones). `Etc/UTC`
/// and plain `UTC` count as the zero-offset family.
pub(super) fn fixed_offset(zone: &TimeZoneId) -> Option<i32> {
    let name = zone.as_str();
    if name == "Etc/UTC" || name == "UTC" {
        return Some(0);
    }
    let rest = name.strip_prefix("Etc/GMT")?;
    if rest.is_empty() {
        return Some(0);
    }
    // POSIX-inverted sign: `Etc/GMT-8` is UTC+8, `Etc/GMT+5` is UTC−5.
    let (sign, hours) = rest.split_at(1);
    let hours: i32 = hours.parse().ok()?;
    let magnitude = hours.checked_mul(60)?;
    match sign {
        "-" => Some(magnitude),
        "+" => Some(-magnitude),
        _ => None,
    }
}

/// How one event's neutral values map onto the wire: the fixed offset every
/// time value folds by, and the all-day flag ([MS-ASCAL] §2.2.2.1).
#[derive(Debug, Clone, Copy)]
pub(super) struct WireClock {
    pub(super) offset_minutes: i32,
    pub(super) all_day: bool,
}

impl WireClock {
    /// Chooses the fold from the event's start form. An all-day start folds
    /// at UTC (all-day bounds are UTC midnights by construction); a zoned
    /// start must sit in the fixed-offset family; a floating start pins to
    /// UTC.
    pub(super) fn of(start: &CalendarDateTime) -> ProviderResult<Self> {
        match start {
            CalendarDateTime::Date(_) => Ok(Self {
                offset_minutes: 0,
                all_day: true,
            }),
            CalendarDateTime::Floating(_) => Ok(Self {
                offset_minutes: 0,
                all_day: false,
            }),
            CalendarDateTime::Zoned { zone, .. } => Ok(Self {
                offset_minutes: fixed_offset(zone).ok_or_else(|| {
                    ProviderError::permanent(format!(
                        "the EAS calendar write folds times through a fixed-offset TZI only; \
                         the zone {} needs tzdata to resolve — send the event's wall clock in \
                         an Etc/GMT±H zone (or UTC) instead",
                        zone.as_str()
                    ))
                })?,
                all_day: false,
            }),
        }
    }

    /// A neutral value's wall clock (an all-day date contributes its
    /// midnight — the form the read side keys and reads one at).
    pub(super) fn wall(value: &CalendarDateTime) -> LocalDateTime {
        match value {
            CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => *local,
            CalendarDateTime::Date(date) => {
                LocalDateTime::new(date.year(), date.month(), date.day(), 0, 0, 0)
                    .unwrap_or_else(|_| unreachable!("a CalendarDate always holds a valid date"))
            }
        }
    }

    /// A neutral value's wire UTC digits ([MS-ASDTYPE] §2.7.2: the wire
    /// carries UTC; `utc = wall − offset`).
    pub(super) fn utc(self, value: &CalendarDateTime) -> LocalDateTime {
        apply_offset(Self::wall(value), -self.offset_minutes)
    }
}

/// The [MS-ASDTYPE] §2.7.2 Compact DateTime string of a UTC wall clock.
pub(super) fn compact_utc(wall: &LocalDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        wall.year(),
        wall.month(),
        wall.day(),
        wall.hour(),
        wall.minute(),
        wall.second()
    )
}

/// The Compact DateTime string of a resolved instant (`UNTIL` bounds).
pub(super) fn compact_utc_of_instant(instant: &UtcDateTime) -> String {
    compact_utc(
        &LocalDateTime::new(
            instant.year(),
            instant.month(),
            instant.day(),
            instant.hour(),
            instant.minute(),
            instant.second(),
        )
        .unwrap_or_else(|_| unreachable!("a UtcDateTime always holds a valid wall clock")),
    )
}

/// Enforces the time **form** rule (the trait's silent-corruption guard): a
/// patched value must keep the base's variant, and a zoned value the same
/// fixed offset — anything else is refused, not converted.
pub(super) fn check_form(
    base: &CalendarDateTime,
    other: &CalendarDateTime,
    what: &str,
) -> ProviderResult<()> {
    let same = match (base, other) {
        (CalendarDateTime::Date(_), CalendarDateTime::Date(_))
        | (CalendarDateTime::Floating(_), CalendarDateTime::Floating(_)) => true,
        (CalendarDateTime::Zoned { zone: a, .. }, CalendarDateTime::Zoned { zone: b, .. }) => {
            fixed_offset(a).is_some_and(|x| Some(x) == fixed_offset(b))
        }
        _ => false,
    };
    if same {
        Ok(())
    } else {
        Err(ProviderError::permanent(format!(
            "the EAS calendar write refuses a time-form change on {what}: the patched value \
             must keep the event's existing form (a zoned event in its own fixed-offset zone, \
             an all-day event a date) — shift the wall clock, never the form"
        )))
    }
}

/// `wall + duration` through the `time` crate (the engine wall clock has no
/// arithmetic of its own). Sub-second precision is refused — the wire's
/// datetimes are second-resolution and a silent truncation would move the
/// event.
pub(super) fn plus_duration(
    wall: LocalDateTime,
    duration: &Duration,
) -> ProviderResult<LocalDateTime> {
    if duration.nanoseconds() != 0 {
        return Err(ProviderError::permanent(
            "the EAS calendar write carries second-resolution times; a sub-second duration \
             cannot ride without being silently truncated",
        ));
    }
    let total = (duration.days().saturating_mul(86_400)).saturating_add(duration.seconds());
    let total = i64::try_from(total).unwrap_or(i64::MAX);
    let shifted = to_primitive(wall) + ::time::Duration::seconds(total);
    Ok(from_primitive(shifted))
}

// ============================================================================
// Drafts (create)
// ============================================================================

/// Converts an [`EventDraft`] into the wire model.
///
/// # Errors
///
/// Refuses [`engine_core::error::FailureClass::Permanent`] for a start/end
/// form mismatch and a named-DST zone (see the module docs); a recurrence
/// without an EAS form refuses the same way
/// (`convert_write_recurrence`).
pub(crate) fn write_from_draft(draft: &EventDraft) -> ProviderResult<CalendarEventWrite> {
    let clock = WireClock::of(&draft.start)?;
    check_form(&draft.start, &draft.end, "the draft's end")?;
    let mut write = CalendarEventWrite {
        start_time: compact_utc(&clock.utc(&draft.start)),
        end_time: compact_utc(&clock.utc(&draft.end)),
        all_day_event: clock.all_day,
        time_zone_base64: build_fixed_offset_tzi_base64(clock.offset_minutes),
        subject: (!draft.summary.is_empty()).then(|| draft.summary.clone()),
        body_plain: draft.description.clone(),
        location: draft.location.clone(),
        ..CalendarEventWrite::default()
    };
    if let Some(recurrence) = &draft.recurrence {
        write.recurrence = Some(rule_to_wire(
            &recurrence.rule,
            recurrence.until.as_ref(),
            &until_of(clock),
        )?);
    }
    validate(write)
}

// ============================================================================
// The master document (series patch / exception carriers)
// ============================================================================

/// Rebuilds the master's complete wire document from `base` + the patch
/// overlay (the document discipline — see the module docs). The recurrence
/// and exceptions ride from the base unless the patch replaces the rule.
///
/// # Errors
///
/// Refuses `Permanent` for every shape the wire cannot carry faithfully
/// (form changes, named-DST zones, unrepresentable rules/overrides/alerts).
pub(crate) fn write_from_series(
    base: &Event,
    patch: &EventPatch,
) -> ProviderResult<CalendarEventWrite> {
    let clock = WireClock::of(&base.start)?;
    if let Some(start) = patch.start_edit() {
        check_form(&base.start, start, "the patched start")?;
    }
    if let Some(end) = patch.end_edit() {
        check_form(&base.start, end, "the patched end")?;
    }
    // Effective bounds: the patched start (else the base's); the patched end
    // (else the moved start + the base's length — patching a start alone
    // moves the event, patching the end alone resizes it).
    let start = patch.start_edit().unwrap_or(&base.start).clone();
    let end_wall = match patch.end_edit() {
        Some(end) => WireClock::wall(end),
        None => plus_duration(WireClock::wall(&start), &base.duration)?,
    };
    let mut write = CalendarEventWrite {
        start_time: compact_utc(&clock.utc(&start)),
        end_time: compact_utc(&apply_offset(end_wall, -clock.offset_minutes)),
        all_day_event: clock.all_day,
        time_zone_base64: build_fixed_offset_tzi_base64(clock.offset_minutes),
        subject: subject_of(base, patch),
        body_plain: text_of(patch.description_edit(), base.description.as_deref()),
        location: text_of(
            patch.location_edit(),
            base.locations.first().and_then(|l| l.name.as_deref()),
        ),
        organizer_email: None,
        organizer_name: None,
        sensitivity: extended_u8(base, "sensitivity"),
        busy_status: extended_u8(base, "busy-status"),
        reminder_minutes: alert_minutes(&base.alerts)?,
        attendees: attendees_of(&base.participants),
        recurrence: None,
        exceptions: Vec::new(),
    };
    write.recurrence = match patch.recurrence_edit() {
        Some(engine_provider::RecurrenceEdit::Clear) => None,
        Some(engine_provider::RecurrenceEdit::Set(recurrence)) => Some(rule_to_wire(
            &recurrence.rule,
            recurrence.until.as_ref(),
            &until_of(clock),
        )?),
        // The base's own rule — an `Until` bound derives through the fixed
        // offset, exactly (no resolved instant rides a stored event).
        None => match base.recurrence.as_ref().and_then(|r| r.rules.first()) {
            Some(rule) => Some(rule_to_wire(rule, None, &until_of(clock))?),
            None => None,
        },
    };
    // A cleared rule takes the series' occurrences — and with them every
    // exception — away; nothing remains to override.
    if patch
        .recurrence_edit()
        .is_some_and(|e| matches!(e, engine_provider::RecurrenceEdit::Clear))
        || base.recurrence.is_none()
    {
        write.exceptions = Vec::new();
    } else if write.recurrence.is_some() {
        write.exceptions = exceptions_of(base, clock)?;
    }
    validate(write)
}

/// The master's subject: the patch's, else the base title (empty → absent).
fn subject_of(base: &Event, patch: &EventPatch) -> Option<String> {
    let title = patch.summary_edit().unwrap_or(base.title.as_str());
    (!title.is_empty()).then(|| title.to_owned())
}

/// A text property under the three-state rule: the patch's Set/Clear, else
/// the base value. A Clear writes the explicit empty string — omitting the
/// element would let a ghosting server keep the old value.
pub(super) fn text_of(edit: Option<&TextEdit>, base: Option<&str>) -> Option<String> {
    match edit {
        Some(TextEdit::Set(value)) => Some(value.clone()),
        Some(TextEdit::Clear) => Some(String::new()),
        None => base.map(str::to_owned),
    }
}

/// An `extended["eas/<fact>"]` u8 (the EAS-native facts the read side
/// stashed), when present.
fn extended_u8(base: &Event, fact: &str) -> Option<u8> {
    base.extended
        .get(&format!("eas/{fact}"))
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
}

/// The base's alerts → the one `Reminder` ([MS-ASCAL] §2.2.2.38): no alert
/// maps to none, a single whole-minute display-before-start alert to its
/// minutes, anything else refuses rather than silently dropping a reminder
/// the user set.
fn alert_minutes(alerts: &[Alert]) -> ProviderResult<Option<u32>> {
    match alerts {
        [] => Ok(None),
        [alert] => {
            let engine_core::calendar::Trigger::Offset {
                offset,
                relative_to,
            } = &alert.trigger
            else {
                return Err(ProviderError::permanent(
                    "the EAS calendar write maps one whole-minute display reminder only; an \
                     alert with a non-offset trigger has no Reminder form",
                ));
            };
            if alert.action != engine_core::calendar::AlertAction::Display {
                return Err(ProviderError::permanent(
                    "the EAS calendar write maps display reminders only; this alert's action \
                     has no Reminder form",
                ));
            }
            if !offset.is_before()
                || !matches!(relative_to, engine_core::calendar::RelativeTo::Start)
            {
                return Err(ProviderError::permanent(
                    "the EAS Reminder is minutes before the start; an alert anchored elsewhere \
                     has no wire form",
                ));
            }
            let magnitude = offset.magnitude();
            if magnitude.nanoseconds() != 0 || magnitude.seconds() % 60 != 0 {
                return Err(ProviderError::permanent(
                    "the EAS Reminder is whole minutes; a sub-minute alert cannot ride without \
                     being silently re-timed",
                ));
            }
            let minutes = (magnitude.days() * 86_400 + magnitude.seconds()) / 60;
            u32::try_from(minutes)
                .map(Some)
                .map_err(|_| ProviderError::permanent("the reminder exceeds the wire's range"))
        }
        _ => Err(ProviderError::permanent(
            "the EAS wire carries one Reminder; an event with several alerts cannot ride \
             without dropping one",
        )),
    }
}

/// The participants → the wire attendee list: every attendee-role
/// participant with an address, by email and name — the serializer's full
/// expressible set ([MS-ASCAL] §2.2.2.3; `AttendeeStatus` is server-owned
/// and never written). The organizer (owner role) is **not** written —
/// server-managed, live-probed Status 6. A participant without an address
/// cannot be scheduled and is skipped loudly (the read side's own rule).
fn attendees_of(participants: &[Participant]) -> Vec<CalendarAttendee> {
    participants
        .iter()
        .filter(|p| p.roles.contains(&ParticipantRole::Owner))
        .for_each(|_| {
            log::debug!("calendar write: the organizer is server-managed on EAS and never written");
        });
    participants
        .iter()
        .filter(|p| !p.roles.contains(&ParticipantRole::Owner))
        .filter_map(|p| {
            let Some(email) = p.email.as_deref().filter(|e| !e.is_empty()) else {
                log::warn!(
                    "calendar write: a participant without an address cannot be scheduled; \
                     skipping it"
                );
                return None;
            };
            Some(CalendarAttendee {
                name: p.name.clone(),
                email: email.to_owned(),
                status: None,
            })
        })
        .collect()
}

/// The `UNTIL` fold for rules that arrive without a resolved instant: the
/// event's wall clock → the wire's UTC digits through the fixed offset.
fn until_of(clock: WireClock) -> impl Fn(LocalDateTime) -> String {
    move |wall| compact_utc(&apply_offset(wall, -clock.offset_minutes))
}

/// The pre-flight gate, mapped into the engine's error shape.
pub(super) fn validate(write: CalendarEventWrite) -> ProviderResult<CalendarEventWrite> {
    write
        .validate()
        .map_err(|e| ProviderError::permanent(format!("the EAS calendar write is invalid: {e}")))?;
    Ok(write)
}

/// A placeholder stamp for the internal rebuild calls that never write one
/// (the wire's `DtStamp` is server-managed — it never rides at all).
pub(super) fn unused_stamp() -> UtcDateTime {
    UtcDateTime::new(1970, 1, 1, 0, 0, 0)
        .unwrap_or_else(|_| unreachable!("the epoch is a valid instant"))
}
