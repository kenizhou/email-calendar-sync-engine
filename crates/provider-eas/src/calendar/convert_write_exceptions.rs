// SPDX-License-Identifier: MPL-2.0
//! The exception half of the write conversion: the base structural
//! overrides onto the wire `Exceptions` list, and the two instance verbs
//! that ride it - an occurrence patch (`write_exception`) and an occurrence
//! delete (`write_occurrence_deleted`), both a `Change` of the master whose
//! container carries the target occurrence (P2 Task 3; the design notes
//! live in `convert_write` module docs).
//!
//! Every override rides the master document - `Excluded` as the deleted
//! marker ([MS-ASCAL] 2.2.2.16, the EXDATE form), `Patch` as a modified
//! exception (2.2.2.21) carrying exactly the fields the override names. An
//! override patch carrying anything the read side never produces refuses:
//! re-emitting it would silently drop the difference.

use engine_core::{
    calendar::{Event, Recurrence, RecurrenceOverride},
    time::{Duration, LocalDateTime, TimeZoneId},
};
use engine_provider::{EventPatch, Occurrence, ProviderError, ProviderResult};
use serde_json::Value;

use super::{
    convert_time::apply_offset,
    convert_write::{
        WireClock, check_form, compact_utc, fixed_offset, plus_duration, text_of, unused_stamp,
        validate, write_from_series,
    },
    model::CalendarException,
};
use crate::calendar_write::CalendarEventWrite;

// ============================================================================
// Exceptions (instance writes)
// ============================================================================

/// The base's structural overrides → the wire `Exceptions` list. Every
/// override rides: `Excluded` as the deleted marker, `Patch` as a modified
/// exception carrying exactly the fields the override names. An override
/// whose patch carries anything the read side never produces refuses —
/// re-emitting it would silently drop the difference.
pub(super) fn exceptions_of(
    base: &Event,
    clock: WireClock,
) -> ProviderResult<Vec<CalendarException>> {
    let Some(recurrence) = base.recurrence.as_ref() else {
        return Ok(Vec::new());
    };
    structural_checks(recurrence)?;
    let mut out = Vec::with_capacity(recurrence.overrides.len());
    for (key, override_value) in &recurrence.overrides {
        let marker = compact_utc(&apply_offset(*key, -clock.offset_minutes));
        match override_value {
            RecurrenceOverride::Excluded => out.push(CalendarException {
                deleted: true,
                exception_start_time: Some(marker),
                ..CalendarException::default()
            }),
            RecurrenceOverride::Patch(patch) => {
                if let Some(exception) = exception_from_patch(patch, marker, *key, clock)? {
                    out.push(exception);
                }
            }
        }
    }
    Ok(out)
}

/// The whole-series structural guards: one rule, no excluded rules, no
/// second rule union — the EAS container carries exactly one pattern.
fn structural_checks(recurrence: &Recurrence) -> ProviderResult<()> {
    if recurrence.rules.len() > 1 || !recurrence.excluded_rules.is_empty() {
        return Err(ProviderError::permanent(
            "the EAS calendar wire carries exactly one recurrence pattern; a rule union or an \
             exclusion rule has no container",
        ));
    }
    Ok(())
}

/// One override patch → a modified exception. `None` (skipped at debug)
/// when the patch carries no exception-shaped field at all — a no-op
/// override, not data.
fn exception_from_patch(
    patch: &engine_core::patch::PatchObject,
    marker: String,
    key: LocalDateTime,
    clock: WireClock,
) -> ProviderResult<Option<CalendarException>> {
    let mut exception = CalendarException {
        exception_start_time: Some(marker),
        ..CalendarException::default()
    };
    if let Some(value) = patch.get("status") {
        let _ = value;
        return Err(ProviderError::permanent(
            "a cancelled-occurrence override has no EAS exception form (MeetingStatus is \
             server-managed); delete the occurrence instead",
        ));
    }
    let start = match patch.get("start") {
        Some(Value::String(wall)) => Some(wall.parse::<LocalDateTime>().map_err(|e| {
            ProviderError::permanent(format!("an override start that does not parse: {e}"))
        })?),
        Some(_) => {
            return Err(ProviderError::permanent(
                "an override start must be a wall-clock string",
            ));
        }
        None => None,
    };
    if let Some(Value::String(zone)) = patch.get("timeZone") {
        let zone = TimeZoneId::iana(zone.clone()).map_err(|_| {
            ProviderError::permanent("an override timeZone that is not a zone name")
        })?;
        if fixed_offset(&zone) != Some(clock.offset_minutes) {
            return Err(ProviderError::permanent(
                "an override carrying a different zone than its series would be silently \
                 re-timed; the EAS exception has no zone of its own",
            ));
        }
    }
    let mut moved = false;
    if let Some(wall) = start {
        exception.start_time = Some(compact_utc(&apply_offset(wall, -clock.offset_minutes)));
        moved = true;
    }
    if let Some(Value::String(text)) = patch.get("duration") {
        let duration = text.parse::<Duration>().map_err(|e| {
            ProviderError::permanent(format!("an override duration that does not parse: {e}"))
        })?;
        let anchor = exception
            .start_time
            .as_deref()
            .map_or_else(|| apply_offset(key, -clock.offset_minutes), uncompact);
        let end = plus_duration(anchor, &duration)?;
        // The anchor is already UTC digits; the end folds the same way.
        exception.end_time = Some(compact_utc(&end));
        moved = true;
    }
    if let Some(Value::String(title)) = patch.get("title") {
        exception.subject = Some(title.clone());
        moved = true;
    }
    if let Some(Value::String(body)) = patch.get("description") {
        exception.body_plain = Some(body.clone());
        moved = true;
    }
    if let Some(value) = patch.get("locations") {
        exception.location = Some(location_name(value)?);
        moved = true;
    }
    for (field, _) in patch.iter() {
        if !matches!(
            field.as_str(),
            "start" | "timeZone" | "duration" | "title" | "description" | "locations"
        ) {
            return Err(ProviderError::permanent(format!(
                "an override patch field {field:?} has no EAS exception form; refusing rather \
                 than silently dropping it"
            )));
        }
    }
    if !moved {
        log::debug!("calendar write: an override with no exception-shaped fields; skipping it");
        return Ok(None);
    }
    Ok(Some(exception))
}

/// A projected `locations` map → its single name (the `loc` id the read
/// side's `OverrideBuilder` synthesizes). Anything richer refuses.
fn location_name(value: &Value) -> ProviderResult<String> {
    let name = value
        .get("loc")
        .and_then(|entry| entry.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProviderError::permanent(
                "an override location without a name has no EAS form (the wire carries plain \
                 text); coordinates and multi-location maps cannot ride",
            )
        })?;
    Ok(name.to_owned())
}

/// A Compact DateTime string back into a wall clock (override anchors).
fn uncompact(wire: &str) -> LocalDateTime {
    let (date, time) = wire
        .trim_end_matches('Z')
        .split_once('T')
        .unwrap_or((wire, "000000"));
    let num = |slice: &str, default: i32| slice.parse().unwrap_or(default);
    let clamp = |value: i32| u8::try_from(value).unwrap_or(0);
    LocalDateTime::new(
        num(&date[0..4], 0),
        clamp(num(&date[4..6], 1)),
        clamp(num(&date[6..8], 1)),
        clamp(num(&time[0..2], 0)),
        clamp(num(&time[2..4], 0)),
        clamp(num(&time[4..6], 0)),
    )
    .unwrap_or_else(|_| unreachable!("the string was compact_utc's own output"))
}

/// An instance patch → the master's document carrying the target occurrence
/// as a modified exception (the master's own fields and every other
/// override ride untouched).
///
/// # Errors
///
/// Refuses `InvalidState` when the patch also edits the recurrence (an
/// occurrence has no rule of its own) or the base is not a series; the
/// `Permanent` set of [`write_from_series`] otherwise.
pub(crate) fn write_exception(
    base: &Event,
    occurrence: &Occurrence,
    patch: &EventPatch,
) -> ProviderResult<CalendarEventWrite> {
    if patch.recurrence_edit().is_some() {
        return Err(ProviderError::invalid_state(
            "a per-instance edit cannot change the recurrence rule — only a series edit can \
             (pass the patch to PatchTarget::Series)",
        ));
    }
    let series = base
        .recurrence
        .as_ref()
        .ok_or_else(|| ProviderError::invalid_state("an instance edit needs a series to edit"))?;
    structural_checks(series)?;
    let clock = WireClock::of(&base.start)?;
    check_form(&base.start, &occurrence.start, "the occurrence's start")?;
    let mut write = write_from_series(base, &EventPatch::new(patch.stamp()))?;
    let exception = instance_exception(base, occurrence, patch, clock)?;
    let marker = exception.exception_start_time.clone().unwrap_or_default();
    write
        .exceptions
        .retain(|e| e.exception_start_time != Some(marker.clone()));
    write.exceptions.push(exception);
    validate(write)
}

/// An occurrence delete → the master's document with the target occurrence
/// folded into the deleted-marker exception (the EAS EXDATE form,
/// [MS-ASCAL] §2.2.2.16), replacing whatever override it had.
///
/// # Errors
///
/// Refuses `InvalidState` when the base is not a series; the `Permanent`
/// set of [`write_from_series`] otherwise.
pub(crate) fn write_occurrence_deleted(
    base: &Event,
    occurrence: &Occurrence,
) -> ProviderResult<CalendarEventWrite> {
    let series = base
        .recurrence
        .as_ref()
        .ok_or_else(|| ProviderError::invalid_state("an occurrence delete needs a series"))?;
    structural_checks(series)?;
    let clock = WireClock::of(&base.start)?;
    check_form(&base.start, &occurrence.start, "the occurrence's start")?;
    let mut write = write_from_series(base, &EventPatch::new(unused_stamp()))?;
    let marker = compact_utc(&clock.utc(&occurrence.start));
    write
        .exceptions
        .retain(|e| e.exception_start_time.as_deref() != Some(marker.as_str()));
    write.exceptions.push(CalendarException {
        deleted: true,
        exception_start_time: Some(marker),
        ..CalendarException::default()
    });
    validate(write)
}

/// The target occurrence's modified exception from the patch: start and end
/// both whenever either moves (an exception without its end is ambiguous
/// server-side — the Android calendar writer sends both), the text fields
/// exactly as patched.
fn instance_exception(
    base: &Event,
    occurrence: &Occurrence,
    patch: &EventPatch,
    clock: WireClock,
) -> ProviderResult<CalendarException> {
    let mut exception = CalendarException {
        exception_start_time: Some(compact_utc(&clock.utc(&occurrence.start))),
        ..CalendarException::default()
    };
    if let Some(start) = patch.start_edit() {
        check_form(&base.start, start, "the patched start")?;
    }
    if let Some(end) = patch.end_edit() {
        check_form(&base.start, end, "the patched end")?;
    }
    if patch.start_edit().is_some() || patch.end_edit().is_some() {
        let start = patch.start_edit().unwrap_or(&occurrence.start).clone();
        let end_wall = match patch.end_edit() {
            Some(end) => WireClock::wall(end),
            None => plus_duration(WireClock::wall(&start), &base.duration)?,
        };
        exception.start_time = Some(compact_utc(&clock.utc(&start)));
        exception.end_time = Some(compact_utc(&apply_offset(end_wall, -clock.offset_minutes)));
    }
    if let Some(summary) = patch.summary_edit() {
        exception.subject = Some(summary.to_owned());
    }
    if let Some(edit) = patch.description_edit() {
        exception.body_plain = Some(text_of(Some(edit), None).unwrap_or_default());
    }
    if let Some(edit) = patch.location_edit() {
        exception.location = Some(text_of(Some(edit), None).unwrap_or_default());
    }
    Ok(exception)
}
