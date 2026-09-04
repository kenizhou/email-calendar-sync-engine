// SPDX-License-Identifier: MPL-2.0
//! Adapter MeetingResponse scenarios (P2 Task 4): `rsvp_event_from_invite`
//! over the offline mock server — the wire golden (CollectionId from the
//! invite email's folder, RequestId its ServerId, UserResponse 1/2/3,
//! SendResponse gated by the negotiated protocol version), the ack/status
//! decode paths, the event-addressed `rsvp_event` refusal, and the RSVP
//! controls the capability ladder advertises per negotiated version. The
//! request-tree and golden-bytes shapes live in `tests/commands_meeting.rs`.

use std::sync::Arc;

use engine_core::{
    calendar::Event,
    ids::{EventId, MailboxId, MessageId, Uid},
    mail::Message,
    membership::Memberships,
    version::RevisionTokens,
};
use engine_provider::{EventRsvp, Provider as _, RsvpResponse};
use provider_eas::adapter::EasAdapter;
// The MeetingResponse page-8 tokens the assertions decode (`commands/meeting`).
use provider_eas::commands::{
    MREQ_COLLECTION_ID, MREQ_INSTANCE_ID, MREQ_REQUEST_ID, MREQ_SEND_RESPONSE, MREQ_STATUS,
    MREQ_USER_RESPONSE, PAGE_MREQ,
};

use super::{
    adapter_calendar_flow::{account, adapter_at},
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The invite email as a mail sync hands it back: a ServerId id, filed in the
/// **inbox** — not the folder this adapter is bound to, which is the whole
/// point of addressing the collection from the message.
fn invite_email() -> Message {
    Message::new(
        MessageId::try_from("srv:mail-77").unwrap(),
        Memberships::of_one(MailboxId::try_from("fid-inbox").unwrap()),
    )
}

/// The neutral answer an invitation-matched facade builds: no stored base, so
/// no guard, and the event id the parsed invitation carried (a placeholder the
/// next events pass reconciles away by uid).
fn rsvp(response: RsvpResponse, notify: bool) -> EventRsvp {
    EventRsvp {
        event: EventId::try_from("imip:uid-standup").unwrap(),
        uid: Uid::new("uid-standup").unwrap(),
        attendee: "info@test.local".to_owned(),
        response,
        comment: None,
        notify_organizer: notify,
        guard: None,
    }
}

/// A MeetingResponse response: `MeetingResponse > Result > Status`.
fn meeting_ack(status: &str) -> MockResponse {
    use provider_eas::commands::{MREQ_MEETING_RESPONSE, MREQ_RESULT, WbxmlElement};
    let tree = WbxmlElement::container(
        PAGE_MREQ,
        MREQ_MEETING_RESPONSE,
        vec![WbxmlElement::container(
            PAGE_MREQ,
            MREQ_RESULT,
            vec![WbxmlElement::text(PAGE_MREQ, MREQ_STATUS, status)],
        )],
    );
    MockResponse::wbxml(&tree)
}

/// A calendar-bound adapter past its OPTIONS negotiation (the handler side:
/// every test's first round is the OPTIONS exchange handing it a version).
async fn negotiated_at(server: &MockServer) -> EasAdapter {
    let mut adapter = adapter_at(server);
    adapter
        .negotiate()
        .await
        .expect("the OPTIONS negotiation succeeds");
    adapter
}

/// The OPTIONS response handing the adapter `versions`.
fn options(versions: &str) -> MockResponse {
    MockResponse::bare(200)
        .with_header("MS-ASProtocolVersions", versions)
        .with_header("MS-ASProtocolCommands", "Sync,MeetingResponse")
}

// ---------------------------------------------------------------------------
// The wire golden
// ---------------------------------------------------------------------------

/// The one-request happy path: the answer addresses the invite EMAIL — the
/// CollectionId is the message's own folder (the inbox), the RequestId its
/// ServerId, the UserResponse the answer's wire code, `SendResponse` present
/// only on a modern server asked to notify, and no `InstanceId` at all (the
/// neutral RSVP carries no occurrence target).
#[tokio::test]
async fn the_answer_addresses_the_invite_email_in_its_own_folder() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("16.1"),
            2 => {
                assert_eq!(req.cmd().as_deref(), Some("MeetingResponse"));
                meeting_ack("1")
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    let receipt = adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Accepted, true),
        )
        .await
        .expect("the answer lands");
    assert_eq!(receipt.event.as_str(), "imip:uid-standup");
    assert_eq!(receipt.uid.as_str(), "uid-standup");
    assert!(
        receipt.revisions.is_empty(),
        "MeetingResponse reports no revision"
    );
    assert_eq!(
        receipt.reply_delivery,
        engine_provider::ReplyDelivery::NotReported
    );

    let req = server.request(2);
    assert_eq!(text(&req, MREQ_USER_RESPONSE), "1");
    assert_eq!(
        text(&req, MREQ_COLLECTION_ID),
        "fid-inbox",
        "the CollectionId is the invite email's folder, never the bound calendar"
    );
    assert_eq!(text(&req, MREQ_REQUEST_ID), "srv:mail-77");
    assert_eq!(
        count(&req, MREQ_SEND_RESPONSE),
        1,
        "16.1 + notify emits SendResponse"
    );
    assert_eq!(
        count(&req, MREQ_INSTANCE_ID),
        0,
        "the neutral RSVP carries no occurrence"
    );
}

/// The three answers map onto the protocol's UserResponse codes: 1 accept,
/// 2 tentative, 3 decline.
#[tokio::test]
async fn the_three_answers_map_onto_the_wire_codes() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("16.1"),
            2..=4 => meeting_ack("1"),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    for response in [
        RsvpResponse::Accepted,
        RsvpResponse::Tentative,
        RsvpResponse::Declined,
    ] {
        adapter
            .rsvp_event_from_invite(&account(), &invite_email(), None, &rsvp(response, true))
            .await
            .expect("the answer lands");
    }

    let codes: Vec<String> = [2, 3, 4]
        .into_iter()
        .map(|ordinal| text(&server.request(ordinal), MREQ_USER_RESPONSE))
        .collect();
    assert_eq!(codes, vec!["1", "2", "3"]);
}

// ---------------------------------------------------------------------------
// SendResponse across protocol versions
// ---------------------------------------------------------------------------

/// A quiet answer on a modern server omits the element — the client choice the
/// 16.x token exists to carry.
#[tokio::test]
async fn a_quiet_answer_on_a_modern_server_omits_send_response() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("16.1"),
            2 => {
                assert_eq!(req.cmd().as_deref(), Some("MeetingResponse"));
                meeting_ack("1")
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Declined, false),
        )
        .await
        .expect("the quiet answer lands");
    assert_eq!(
        count(&server.request(2), MREQ_SEND_RESPONSE),
        0,
        "quiet omits SendResponse"
    );
}

/// A quiet answer on a 14.1 server is refused before the wire: the token is
/// unregistered there, so silence cannot be requested — the controls said so,
/// and the adapter honours its own advertisement.
#[tokio::test]
async fn a_quiet_answer_on_an_old_server_is_refused_before_the_wire() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("14.1"),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    let err = adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Declined, false),
        )
        .await
        .expect_err("14.1 has no SendResponse token");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert_eq!(server.count(), 1, "only the OPTIONS round went out");
}

/// A loud answer on a 14.1 server still goes out — without the element the
/// older protocol cannot carry. The server emails the organizer per its own
/// default; nothing is asked in-band.
#[tokio::test]
async fn a_loud_answer_on_an_old_server_omits_the_unregistered_token() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("14.1"),
            2 => meeting_ack("1"),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Accepted, true),
        )
        .await
        .expect("the loud answer lands");
    let req = server.request(2);
    assert_eq!(
        count(&req, MREQ_SEND_RESPONSE),
        0,
        "the 14.1 wire never carries the token"
    );
    assert_eq!(text(&req, MREQ_USER_RESPONSE), "1");
}

// ---------------------------------------------------------------------------
// Status decode and the refusals
// ---------------------------------------------------------------------------

/// A non-1 Result Status surfaces as a classified error naming the code (4 =
/// server error, [MS-ASCMD] 2.2.3.177.9) — permanent, not a retry.
#[tokio::test]
async fn a_failed_result_status_surfaces_named_and_classified() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_: &CapturedRequest, ordinal: usize| match ordinal {
            1 => options("16.1"),
            2 => meeting_ack("4"),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = negotiated_at(&server).await;

    let err = adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Accepted, true),
        )
        .await
        .expect_err("status 4 fails the answer");
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(
        err.detail().contains('4'),
        "names the status: {}",
        err.detail()
    );
}

/// The event-addressed verb refuses honestly: EAS answers from the invitation
/// message, and a stored event names nothing the protocol can address.
#[tokio::test]
async fn the_event_addressed_rsvp_refuses_pointing_at_the_invitation_facade() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = adapter_at(&server);
    let mut base = Event::new(
        EventId::try_from("srv:ev-9").unwrap(),
        Uid::new("uid-standup").unwrap(),
        Memberships::of_one(engine_core::ids::CalendarId::try_from("fid-cal-1").unwrap()),
        engine_core::time::CalendarDateTime::utc("2026-08-11T09:00:00".parse().unwrap()),
    );
    base.revisions = RevisionTokens::default();

    let err = adapter
        .rsvp_event(&account(), &base, &rsvp(RsvpResponse::Accepted, true))
        .await
        .expect_err("the event-addressed verb is not the EAS path");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert!(
        err.detail().contains("rsvp_invitation"),
        "the refusal names the supported facade: {}",
        err.detail()
    );
    assert_eq!(server.count(), 0, "nothing went out");
}

/// An unbound adapter refuses the invitation verb too — the RSVP capability
/// lands with the calendar binding, exactly as the read and write bits did.
#[tokio::test]
async fn an_unbound_adapter_refuses_the_invitation_answer() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = EasAdapter::new(
        client_at(&server.eas_url()),
        MailboxId::try_from("fid-inbox").unwrap(),
    );

    let err = adapter
        .rsvp_event_from_invite(
            &account(),
            &invite_email(),
            None,
            &rsvp(RsvpResponse::Accepted, true),
        )
        .await
        .expect_err("no calendar binding, no calendar family");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert!(
        adapter
            .connection_info()
            .capabilities
            .calendar_rsvp()
            .is_none(),
        "the capability never advertises what the binding gates"
    );
}

// ---------------------------------------------------------------------------
// The RSVP controls the capability ladder advertises
// ---------------------------------------------------------------------------

/// The controls follow the negotiated version: `SendResponse` is 16.0/16.1
/// ([MS-ASWBXML] 2.1.2.1.9), so only there may the user choose silence; a
/// comment is carried nowhere in the MeetingResponse schema on any version;
/// and the guard is absent — the request names the email and nothing else.
#[tokio::test]
async fn rsvp_controls_follow_the_negotiated_protocol_version() {
    super::harness::init_logger();
    for (versions, suppress) in [("16.1", true), ("16.0", true), ("14.1", false)] {
        let server =
            MockServer::http(
                Arc::new(move |_: &CapturedRequest, ordinal: usize| match ordinal {
                    1 => options(versions),
                    _ => MockResponse::bare(500),
                }) as Handler,
            );
        let mut adapter = adapter_at(&server);
        adapter.negotiate().await.expect("negotiation");

        let controls = adapter
            .connection_info()
            .capabilities
            .calendar_rsvp()
            .unwrap_or_else(|| {
                panic!("{versions}: the bound adapter advertises the RSVP controls")
            });
        assert!(
            !controls.comment,
            "{versions}: no note rides MeetingResponse"
        );
        assert_eq!(controls.suppress_notification, suppress, "{versions}");
        assert_eq!(
            controls.guard,
            engine_provider::WriteGuard::Absent,
            "{versions}"
        );
    }

    // Pre-negotiation the adapter knows no version: the conservative shape,
    // never a client-choice it cannot yet carry on the wire.
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let controls = adapter_at(&server)
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("the bound adapter advertises the RSVP controls");
    assert!(
        !controls.suppress_notification,
        "no negotiation, no silence promise"
    );
}

// ---------------------------------------------------------------------------
// Local decode helpers over a captured request (page 8)
// ---------------------------------------------------------------------------

fn text(req: &CapturedRequest, token: u8) -> String {
    let tree = req.wbxml_tree().expect("the request is WBXML");
    let mut out = Vec::new();
    walk(&tree, token, &mut out);
    out.into_iter()
        .next()
        .expect("the element rides the request")
}

fn count(req: &CapturedRequest, token: u8) -> usize {
    let tree = req.wbxml_tree().expect("the request is WBXML");
    walk_count(&tree, token)
}

fn walk(el: &provider_eas::commands::WbxmlElement, token: u8, out: &mut Vec<String>) {
    if (el.page, el.token) == (PAGE_MREQ, token)
        && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
    {
        out.push(t.clone());
    }
    for child in &el.children {
        walk(child, token, out);
    }
}

fn walk_count(el: &provider_eas::commands::WbxmlElement, token: u8) -> usize {
    usize::from(el.token == token)
        + el.children
            .iter()
            .map(|c| walk_count(c, token))
            .sum::<usize>()
}
