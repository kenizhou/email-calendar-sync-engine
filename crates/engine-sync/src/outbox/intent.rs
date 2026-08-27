//! The tagged envelope every outbox op's payload travels in.
//!
//! A [`PendingOp`](engine_core::write::PendingOp) payload is the only record of
//! what an interrupted write was meant to do — the drainer replays it — so the
//! payload must name its **verb**, not leave the dispatcher to infer it from the
//! payload's shape (`store-and-sync.md`). [`OutboxIntent`] is that name: every
//! outbox driver serializes its op's payload through this envelope, and the
//! `verb` tag is the one thing a drainer dispatches on.
//!
//! The envelope lives in `engine-sync`, not `engine-core`, because it names
//! provider-layer intent types (`MailEdit`, the event writes): `engine-core`
//! cannot depend on `engine-provider` (north-star), while this crate already
//! depends on both. [`SubmitPayload`] stays in `engine-core` — the pure
//! render-vs-resend contract — and is embedded by the `submit_mail` verb.

use engine_core::{
    contact::{ContactDraft, ContactPatch},
    ids::ContactId,
    write::SubmitPayload,
};
use engine_provider::{
    Draft, EventDeletion, EventDraft, EventEdit, EventRsvp, EventWrite, MailEdit, MessageReport,
};
use serde::{Deserialize, Serialize};

/// The durable intent of ANY outbox pending op, tagged for drainer dispatch.
///
/// One variant per outbox driver verb; each carries exactly the intent the
/// driver was handed (never the base it was read at — a retry re-applies to a
/// freshly fetched one) under a single named field. That field is deliberate:
/// serde's internally tagged representation cannot carry a newtype variant
/// around a non-map value (`ContactId` serializes as a bare string), and a
/// named field gives every verb the same wire shape — `{"verb": …, "<field>":
/// …}` — instead of letting an inner enum's own variant names (`MailEdit`)
/// surface beside the tag. The `verb` tag is closed: an unknown verb fails to
/// decode rather than decoding as a silent no-op, because a drainer dispatches
/// on it.
#[allow(
    clippy::large_enum_variant,
    reason = "a wire contract, not a stored value: one intent is built per write and \
              serialized immediately, so boxing a variant would add an allocation to \
              every enqueue without shrinking anything that outlives the call"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum OutboxIntent {
    /// Send mail — the engine renders a draft, or re-sends already-rendered
    /// bytes, per the payload's own `kind` tag (see [`SubmitPayload`]).
    SubmitMail {
        /// The tagged mail-submission intent.
        payload: SubmitPayload<Draft>,
    },
    /// Mutate one already-synced message (mark-read/flag, move, delete).
    EditMail {
        /// The mail mutation to apply.
        edit: MailEdit,
    },
    /// Report one already-synced message as junk / not junk / phishing.
    ReportMessage {
        /// The report to file.
        report: MessageReport,
    },
    /// Create one contact card.
    CreateContact {
        /// The card to create, and where.
        draft: ContactDraft,
    },
    /// Patch one contact card.
    PatchContact {
        /// The card the patch targets — the intent names its target, so a
        /// replay re-reads the base by id instead of inferring it.
        contact: ContactId,
        /// The targeted changes to apply.
        patch: ContactPatch,
    },
    /// Delete one contact card.
    DeleteContact {
        /// The card to delete.
        contact: ContactId,
    },
    /// Create one event.
    CreateEvent {
        /// The event to create.
        draft: EventDraft,
    },
    /// Patch one stored event.
    PatchEvent {
        /// Which occurrence, and what changed.
        edit: EventEdit,
    },
    /// Replace one event's whole stored document (the iMIP RSVP path).
    PutEventDoc {
        /// The document to store.
        write: EventWrite,
    },
    /// Answer one invitation.
    RsvpEvent {
        /// The answer to record.
        rsvp: EventRsvp,
    },
    /// Delete one event (or one of its occurrences).
    DeleteEvent {
        /// The event (or occurrence) to delete.
        deletion: EventDeletion,
    },
}

#[cfg(test)]
mod tests {
    use engine_core::{
        calendar::Event,
        contact::{ContactCard, ContactDraft, ContactKind, ContactPatch, FieldPatch},
        ids::{
            AddressBookId, CalendarId, ContactId, EventId, MailboxId, MessageIdHeader, ProviderKey,
            Uid,
        },
        mail::EmailAddress,
        membership::Memberships,
        raw::RawIcal,
        time::{CalendarDateTime, LocalDateTime},
        version::{ETag, RevisionTokens},
        write::SubmitPayload,
    };
    use engine_provider::{
        Draft, EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, EventWrite, MailEdit,
        MessageReport, PatchTarget, ReportVerdict, RsvpResponse,
    };
    use serde_json::json;

    use super::OutboxIntent;

    /// Every verb's intent, one row each — the two submission paths share
    /// `submit_mail`, so it has two rows. Each round-trips through a durable
    /// op's JSON payload under its exact `verb` tag: the wire contract the
    /// drainer decodes.
    #[test]
    fn every_intent_round_trips_under_its_exact_verb_tag() {
        let table = [
            (submit_mail_draft(), "submit_mail"),
            (submit_mail_source(), "submit_mail"),
            (edit_mail(), "edit_mail"),
            (report_message(), "report_message"),
            (create_contact(), "create_contact"),
            (patch_contact(), "patch_contact"),
            (delete_contact(), "delete_contact"),
            (create_event(), "create_event"),
            (patch_event(), "patch_event"),
            (put_event_doc(), "put_event_doc"),
            (rsvp_event(), "rsvp_event"),
            (delete_event(), "delete_event"),
        ];
        for (intent, verb) in table {
            let value = serde_json::to_value(&intent).unwrap();
            assert_eq!(value["verb"], json!(verb), "wrong verb tag for {intent:?}");
            assert_eq!(
                serde_json::from_value::<OutboxIntent>(value).unwrap(),
                intent,
                "{verb} did not survive the round-trip"
            );
        }
    }

    #[test]
    fn submit_mail_keeps_the_nested_kind_tag_the_recovery_dispatches_on() {
        // `verb` picks the driver; the payload's own `kind` tag picks the
        // recovery within it (render a draft vs re-send bytes verbatim). Both
        // must survive one encode — and the rendered-source payload carries its
        // envelope recipients, so a replay re-sends to the exact same set.
        let value = serde_json::to_value(submit_mail_source()).unwrap();
        assert_eq!(value["payload"]["kind"], json!("rendered_source"));
        assert_eq!(value["payload"]["recipients"], json!(["bcc@test.local"]));
    }

    #[test]
    fn an_unknown_verb_is_a_closed_dispatch_not_a_hint() {
        // A drainer dispatches on `verb`; an unknown one must fail to decode
        // rather than silently decode as a no-op.
        let unknown = serde_json::from_value::<OutboxIntent>(json!({ "verb": "teleport" }));
        assert!(unknown.is_err());
    }

    fn submit_mail_draft() -> OutboxIntent {
        OutboxIntent::SubmitMail {
            payload: SubmitPayload::Draft(mail_draft("send-1@test.local")),
        }
    }

    fn submit_mail_source() -> OutboxIntent {
        OutboxIntent::SubmitMail {
            payload: SubmitPayload::RenderedSource {
                rfc5322: b"Message-ID: <send-2@test.local>\r\n\r\nbody\r\n".to_vec(),
                recipients: vec!["bcc@test.local".to_owned()],
            },
        }
    }

    fn edit_mail() -> OutboxIntent {
        OutboxIntent::EditMail {
            edit: MailEdit::mark_seen(ProviderKey::new("msg-1").unwrap(), true),
        }
    }

    fn report_message() -> OutboxIntent {
        OutboxIntent::ReportMessage {
            report: MessageReport::new(
                ProviderKey::new("msg-1").unwrap(),
                ReportVerdict::Junk,
                MailboxId::try_from("Junk").unwrap(),
            ),
        }
    }

    fn create_contact() -> OutboxIntent {
        OutboxIntent::CreateContact {
            draft: ContactDraft {
                address_book: AddressBookId::try_from("personal").unwrap(),
                card: contact_card("card-1"),
            },
        }
    }

    fn patch_contact() -> OutboxIntent {
        let patch = ContactPatch {
            kind: Some(FieldPatch::Set(ContactKind::Organization)),
            ..ContactPatch::default()
        };
        OutboxIntent::PatchContact {
            contact: ContactId::new(ProviderKey::new("card-1").unwrap()),
            patch,
        }
    }

    fn delete_contact() -> OutboxIntent {
        OutboxIntent::DeleteContact {
            contact: ContactId::new(ProviderKey::new("card-1").unwrap()),
        }
    }

    fn create_event() -> OutboxIntent {
        OutboxIntent::CreateEvent {
            draft: event_draft("evt-1@test.local"),
        }
    }

    fn patch_event() -> OutboxIntent {
        OutboxIntent::PatchEvent {
            edit: EventEdit::new(
                &stored_event("/cal/default/evt-2.ics", "evt-2@test.local"),
                PatchTarget::Series,
                EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap()).summary("Renamed"),
            ),
        }
    }

    fn put_event_doc() -> OutboxIntent {
        OutboxIntent::PutEventDoc {
            write: EventWrite::replacing(
                &stored_event("/cal/default/evt-3.ics", "evt-3@test.local"),
                RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
            ),
        }
    }

    fn rsvp_event() -> OutboxIntent {
        OutboxIntent::RsvpEvent {
            rsvp: EventRsvp::to(
                &stored_event("/cal/default/evt-4.ics", "evt-4@test.local"),
                "alice@test.local",
                RsvpResponse::Accepted,
            ),
        }
    }

    fn delete_event() -> OutboxIntent {
        OutboxIntent::DeleteEvent {
            deletion: EventDeletion::of(&stored_event(
                "/cal/default/evt-5.ics",
                "evt-5@test.local",
            )),
        }
    }

    fn mail_draft(message_id: &str) -> Draft {
        Draft::new(
            MessageIdHeader::new(message_id).unwrap(),
            EmailAddress::new("alice@test.local"),
            vec![EmailAddress::new("bob@test.local")],
            "Subject",
            "Body",
        )
    }

    fn contact_card(id: &str) -> ContactCard {
        ContactCard::new(
            ContactId::new(ProviderKey::new(id).unwrap()),
            Memberships::of_one(AddressBookId::try_from("personal").unwrap()),
        )
    }

    fn event_draft(uid: &str) -> EventDraft {
        EventDraft::new(
            CalendarId::new(ProviderKey::new("/cal/default/").unwrap()),
            Uid::new(uid).unwrap(),
            "Sprint planning",
            at(9),
            at(10),
            "2026-07-14T10:00:00Z".parse().unwrap(),
        )
    }

    /// A stored event as the caller read it — the base every guarded write in
    /// these fixtures is built from.
    fn stored_event(href: &str, uid: &str) -> Event {
        let mut event = Event::new(
            EventId::try_from(href).unwrap(),
            Uid::new(uid).unwrap(),
            Memberships::of_one(CalendarId::new(ProviderKey::new("/cal/default/").unwrap())),
            at(9),
        );
        event.raw_ical = Some(RawIcal::new(format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR"
        )));
        event.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
        event
    }

    fn at(hour: u8) -> CalendarDateTime {
        CalendarDateTime::utc(
            format!("2026-08-01T{hour:02}:00:00")
                .parse::<LocalDateTime>()
                .unwrap(),
        )
    }
}
