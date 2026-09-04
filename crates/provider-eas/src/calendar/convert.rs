// SPDX-License-Identifier: MPL-2.0
//! [`CalendarEventProps`] → the engine's neutral [`Event`] (P2 Task 2, the
//! read-side conversion seam — pure, no IO, never panics on wire data).
//!
//! ## Mapping
//!
//! Identity: `id` = the item **ServerId** (the store row key), `uid` = the
//! EAS `UID` (the GlobalObjectId join key, [MS-ASEMAIL] §3.1.4.7) falling
//! back to the ServerId — never an invented identity. Membership: the
//! calendar folder's `CalendarId` (the `folder` parameter — the Sync
//! `CollectionId` the adapter is bound to).
//!
//! Times: EAS carries UTC instants plus the originator's TZI zone
//! ([MS-ASDTYPE] §2.3.2). The fold policy is `convert_time`'s (fixed-offset
//! `Etc/GMT±H`, UTC, or floating); an all-day event folds to `DATE` values
//! (§2.2.2.1 — the date parts of the UTC stamps, the wire END the exclusive
//! next midnight), duration `DTEND − DTSTART` in wall-clock terms. A missing
//! StartTime degrades to the epoch (the server fills real values at
//! creation, §3.2.4.4) — the item is kept, loudly.
//!
//! Status/privacy/free-busy: `BusyStatus` 0 → `free`, 1-4 → `busy`
//! (§2.2.2.9); `Sensitivity` 0→public, 1/2→private, 3→secret (§2.2.2.41);
//! the `MeetingStatus` cancelled bit (C, value 4 — wire values {5,7,13,15})
//! → the `cancelled` tombstone. Reminder minutes → a display alert before
//! the start. Organizer/attendees → participants (owner/attendee roles;
//! `AttendeeStatus` {0,2,3,4,5} → needs-action/tentative/accepted/declined/
//! needs-action). Recurrence + exceptions: `convert_recurrence`.
//!
//! The EAS-native facts with no first-class field (`BusyStatus` beyond
//! free/busy, `MeetingStatus`, `Sensitivity`, and the raw TZI blob — the
//! full structural zone) survive under the adapter's `eas/*` extended
//! namespace.

use std::collections::BTreeSet;

use engine_core::{
    calendar::{
        Alert, Event, EventStatus, FreeBusyStatus, Location, Participant, ParticipantRole,
        ParticipationStatus, Privacy, Trigger,
    },
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, Duration, LocalDateTime, UtcDateTime},
};
use serde_json::json;

use super::{
    convert_recurrence::recurrence_from_props,
    convert_time::{TimeFold, epoch_wall, parse_wire_utc},
    model::CalendarEventProps,
};

/// The adapter's extended-property namespace (the mail slices' convention):
/// the EAS-native facts that have no first-class `Event` field.
const EXTENDED_NAMESPACE: &str = "eas";

/// The MeetingStatus cancelled bit ([MS-ASCAL] §2.2.2.28 M/R/C flags — C is
/// value 4; wire values {5,7,13,15} carry it).
const MEETING_STATUS_CANCELLED: u8 = 0b100;

/// Converts one downsynced Calendar item into the engine's neutral `Event`.
///
/// `folder` is the calendar folder's ServerId (the Sync collection the item
/// arrived under — the event's calendar membership); `server_id` the item's
/// ServerId (the store row key). Pure and total: malformed values degrade
/// per-field with warnings (the parse layer's contract), never panic, never
/// drop the item.
pub(crate) fn calendar_event_from_props(
    folder: &str,
    server_id: &str,
    props: &CalendarEventProps,
) -> Event {
    let id = EventId::try_from(server_id).unwrap_or_else(|e| {
        log::warn!(
            "calendar conversion: ServerId {server_id:?} cannot key an event ({e}); the item \
             is kept under a placeholder key the store will reconcile away"
        );
        EventId::try_from("eas:unkeyed").unwrap_or_else(|_| unreachable!("a fixed valid key"))
    });
    let uid = props
        .uid
        .as_deref()
        .filter(|uid| !uid.is_empty())
        .map_or_else(
            || Uid::new(server_id).unwrap_or_else(|_| unreachable!("id already keyed above")),
            |uid| Uid::new(uid).unwrap_or_else(|_| unreachable!("checked non-empty by filter")),
        );
    let calendar = CalendarId::try_from(folder).unwrap_or_else(|e| {
        log::warn!(
            "calendar conversion: folder {folder:?} cannot key a calendar ({e}); membership \
             falls back to the placeholder the store reconciles"
        );
        CalendarId::try_from("eas:unbound").unwrap_or_else(|_| unreachable!("a fixed valid key"))
    });

    // The fold is chosen from the START's instant (a DST zone contributes
    // its start-instant offset — see convert_time's module docs).
    let start_utc = props
        .start_time
        .as_deref()
        .and_then(parse_wire_utc)
        .unwrap_or_else(|| {
            log::warn!(
                "calendar conversion: item {server_id} has no parseable StartTime; degrading \
                 to the epoch ([MS-ASCAL] §3.2.4.4 — the server fills real values at creation)"
            );
            epoch_wall()
        });
    let fold = TimeFold::choose(props.time_zone.as_ref(), start_utc);

    let start = timed_or_date_value(start_utc, &fold, props.all_day_event);
    let end = props.end_time.as_deref().and_then(parse_wire_utc);
    let mut event = Event::new(id, uid, Memberships::of_one(calendar), start.clone());
    event.duration = duration_of(&start, end, &fold, props.all_day_event, server_id);
    event.title = props.subject.clone().unwrap_or_default();
    event.description.clone_from(&props.body_plain);
    event.status = status_of(props.meeting_status);
    event.free_busy_status = free_busy_of(props.busy_status);
    event.privacy = privacy_of(props.sensitivity);
    event.updated = props
        .dtstamp
        .as_deref()
        .and_then(parse_wire_utc)
        .and_then(utc_of);
    if let Some(location) = &props.location {
        event.locations.push(Location::named(location.clone()));
    }
    if props.reminder_set
        && let Some(minutes) = props.reminder_minutes
        && let Some(alert) = reminder_alert(minutes)
    {
        event.alerts.push(alert);
    }
    event.participants = participants_of(props);
    event.recurrence = recurrence_from_props(props, &fold, props.all_day_event);

    // The EAS-native facts: BusyStatus beyond free/busy (tentative / OOF /
    // working-elsewhere all read "busy" above), MeetingStatus, Sensitivity
    // — and the raw TZI blob, the full structural zone (its decoded bias is
    // the convenient read side; the blob itself round-trips the rules).
    if let Some(busy) = props.busy_status {
        event
            .extended
            .set(format!("{EXTENDED_NAMESPACE}/busy-status"), json!(busy));
    }
    if let Some(meeting) = props.meeting_status {
        event.extended.set(
            format!("{EXTENDED_NAMESPACE}/meeting-status"),
            json!(meeting),
        );
    }
    if let Some(sensitivity) = props.sensitivity {
        event.extended.set(
            format!("{EXTENDED_NAMESPACE}/sensitivity"),
            json!(sensitivity),
        );
    }
    if let Some(blob) = &props.time_zone {
        if let Some(raw) = &blob.raw_base64 {
            event
                .extended
                .set(format!("{EXTENDED_NAMESPACE}/timezone"), json!(raw));
        }
        if let Some(tzi) = &blob.parsed {
            event.extended.set(
                format!("{EXTENDED_NAMESPACE}/timezone-bias"),
                json!(tzi.base_bias_minutes),
            );
        }
    }
    event
}

/// The engine start value for a wire UTC instant: an all-day event takes the
/// UTC stamp's date (§2.2.2.1 — all-day bounds arrive as UTC midnight); a
/// timed one the folded wall clock.
fn timed_or_date_value(utc: LocalDateTime, fold: &TimeFold, all_day: bool) -> CalendarDateTime {
    if all_day {
        let date = CalendarDate::new(utc.year(), utc.month(), utc.day())
            .unwrap_or_else(|_| unreachable!("the wall clock's date is valid"));
        return CalendarDateTime::Date(date);
    }
    let local = fold.wall(utc);
    match fold.zone() {
        Some(zone) => CalendarDateTime::Zoned { local, zone },
        None => CalendarDateTime::Floating(local),
    }
}

/// The event's length: `EndTime − StartTime` in wall-clock terms (the same
/// kind on both ends — [MS-ASDTYPE]'s nominal-day model). An all-day event's
/// wire END is the exclusive next midnight; a degenerate/absent end warns
/// and degrades (one whole day for all-day, zero for timed).
fn duration_of(
    start: &CalendarDateTime,
    end: Option<LocalDateTime>,
    fold: &TimeFold,
    all_day: bool,
    server_id: &str,
) -> Duration {
    let one_day = || {
        Duration::from_parts(0, 1, 0, 0, 0, 0)
            .unwrap_or_else(|_| unreachable!("one day is representable"))
    };
    let Some(end_utc) = end else {
        if all_day {
            log::debug!(
                "calendar conversion: all-day item {server_id} without an EndTime; one day"
            );
            return one_day();
        }
        return Duration::ZERO;
    };
    let end_value = timed_or_date_value(end_utc, fold, all_day);
    match start.duration_until(&end_value) {
        Ok(duration) => duration,
        Err(e) => {
            log::warn!(
                "calendar conversion: item {server_id} end does not follow its start ({e}); \
                 degrading the length (one day all-day, zero timed)"
            );
            if all_day { one_day() } else { Duration::ZERO }
        }
    }
}

/// MeetingStatus → the engine status: the cancelled bit is a tombstone;
/// everything else is confirmed ([MS-ASCAL] §2.2.2.28 has no tentative).
fn status_of(meeting_status: Option<u8>) -> EventStatus {
    match meeting_status {
        Some(status) if status & MEETING_STATUS_CANCELLED != 0 => EventStatus::Cancelled,
        _ => EventStatus::Confirmed,
    }
}

/// BusyStatus → free/busy ([MS-ASCAL] §2.2.2.9: 0=Free; 1=Tentative,
/// 2=Busy, 3=OOF, 4=Working-elsewhere all block time; the raw byte survives
/// in `extended` for the distinctions the engine does not model).
fn free_busy_of(busy_status: Option<u8>) -> FreeBusyStatus {
    match busy_status {
        Some(0) => FreeBusyStatus::Free,
        _ => FreeBusyStatus::Busy,
    }
}

/// Sensitivity → privacy (§2.2.2.41: 0=Normal→public, 1=Personal→private,
/// 2=Private→private, 3=Confidential→secret — the iCalendar CLASS mapping).
fn privacy_of(sensitivity: Option<u8>) -> Privacy {
    match sensitivity {
        Some(1 | 2) => Privacy::Private,
        Some(3) => Privacy::Secret,
        _ => Privacy::Public,
    }
}

/// Reminder minutes → a display alert that long before the start
/// (§2.2.2.38); a value beyond the engine's duration range warns and drops
/// the alert (never the item).
fn reminder_alert(minutes: u32) -> Option<Alert> {
    let seconds = u64::from(minutes).checked_mul(60)?;
    match Duration::from_parts(0, 0, 0, 0, seconds, 0) {
        Ok(duration) => Some(Alert::display(Trigger::before_start(duration))),
        Err(e) => {
            log::warn!(
                "calendar conversion: Reminder {minutes}m out of range ({e}); dropping the alert"
            );
            None
        }
    }
}

/// Organizer + attendees → participants ([MS-ASCAL] §2.2.2.3-§2.2.2.5): the
/// organizer takes the owner role (not awaiting its own reply, already
/// accepted); each attendee the attendee role with its `AttendeeStatus`
/// ({0,2,3,4,5}: unknown/not-responded → needs-action, 2 → tentative,
/// 3 → accepted, 4 → declined; out-of-enum warns and reads unknown). An
/// attendee without an address cannot be scheduled and is skipped, loudly.
fn participants_of(props: &CalendarEventProps) -> Vec<Participant> {
    let mut participants = Vec::new();
    if props.organizer_email.is_some() || props.organizer_name.is_some() {
        let mut roles = BTreeSet::new();
        roles.insert(ParticipantRole::Owner);
        participants.push(Participant {
            name: props.organizer_name.clone(),
            email: props
                .organizer_email
                .clone()
                .filter(|email| !email.is_empty()),
            kind: None,
            roles,
            participation_status: ParticipationStatus::Accepted,
            expect_reply: false,
            comment: None,
            sent_by: None,
        });
    }
    for attendee in &props.attendees {
        if attendee.email.is_empty() {
            log::warn!(
                "calendar conversion: attendee {:?} without an Email address cannot be \
                 scheduled; skipping it",
                attendee.name
            );
            continue;
        }
        let mut participant = Participant::attendee(attendee.email.clone());
        participant.name.clone_from(&attendee.name);
        participant.participation_status = attendee_status(attendee.status);
        participants.push(participant);
    }
    participants
}

/// `AttendeeStatus` → the neutral participation status (§2.2.2.5).
fn attendee_status(status: Option<u8>) -> ParticipationStatus {
    match status {
        Some(2) => ParticipationStatus::Tentative,
        Some(3) => ParticipationStatus::Accepted,
        Some(4) => ParticipationStatus::Declined,
        Some(0 | 5) | None => ParticipationStatus::NeedsAction,
        Some(other) => {
            log::warn!(
                "calendar conversion: AttendeeStatus {other} outside [MS-ASCAL] §2.2.2.5 \
                 {{0,2,3,4,5}}; reading unknown as needs-action"
            );
            ParticipationStatus::NeedsAction
        }
    }
}

/// A wall clock read as UTC → the engine's `UtcDateTime` (the `updated`
/// stamp; `DtStamp` is a UTC last-modified per §2.2.2.18).
fn utc_of(wall: LocalDateTime) -> Option<UtcDateTime> {
    UtcDateTime::new(
        wall.year(),
        wall.month(),
        wall.day(),
        wall.hour(),
        wall.minute(),
        wall.second(),
    )
    .ok()
}
