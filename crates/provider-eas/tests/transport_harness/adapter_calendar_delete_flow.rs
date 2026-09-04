// SPDX-License-Identifier: MPL-2.0
//! The delete scenarios, the refusals, and the capability ladder of the
//! adapter calendar write slice (P2 Task 3) - split from
//! `adapter_calendar_write_flow.rs` at the 500-line cap; the fixtures and
//! request-walking helpers come from there.

use std::sync::Arc;

use engine_core::ids::{EventId, MailboxId, Uid};
use engine_provider::{
    Capabilities, DeleteTarget, EventDeletion, EventEdit, EventPatch, Occurrence, OverrideSurvival,
    PatchTarget, Provider as _, WriteGuard,
};
use provider_eas::{
    calendar::{CAL_DELETED, CAL_EXCEPTION_START_TIME, PAGE_CALENDAR},
    commands::{AS_DELETE, AS_SERVER_ID, PAGE_AIRSYNC},
};

use super::{
    adapter_calendar_flow::{account, adapter_at},
    adapter_calendar_write_flow::{
        count_of, draft, item_status, seed, seed_response, series_event, stamp, text_of,
        upsync_response, zoned,
    },
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// A series delete rides the wire `Delete` naming the ServerId; the clean
/// response (no item status — §2.2.3.154's success shape) resolves Ok.
#[tokio::test]
async fn a_series_delete_rides_the_wire_delete() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                assert_eq!(count_of(req, PAGE_AIRSYNC, AS_DELETE), 1);
                assert_eq!(text_of(req, PAGE_AIRSYNC, AS_SERVER_ID), "srv:ev-9");
                MockResponse::wbxml(&upsync_response("ev-key-3", vec![]))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    adapter
        .delete_event(
            &account(),
            Some(&series_event()),
            &EventDeletion::of(&series_event()),
        )
        .await
        .expect("the Delete lands");
}

/// An occurrence delete rides a `Change` of the master whose exception list
/// carries the deleted marker for the target occurrence.
#[tokio::test]
async fn an_occurrence_delete_marks_the_exception_deleted() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => {
                assert_eq!(
                    count_of(req, PAGE_CALENDAR, CAL_EXCEPTION_START_TIME),
                    2,
                    "the pre-existing exclusion plus the new marker"
                );
                assert_eq!(
                    count_of(req, PAGE_CALENDAR, CAL_DELETED),
                    2,
                    "both read as deleted markers"
                );
                MockResponse::wbxml(&upsync_response("ev-key-3", vec![]))
            }
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    let base = series_event();
    adapter
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(
                &base,
                Occurrence::starting(zoned("2026-09-08", "09:00:00")),
                stamp(),
            ),
        )
        .await
        .expect("the exception delete lands");
}

/// An occurrence delete without the base series refuses `InvalidState` —
/// the rewrite needs the master's own fields (the trait's CalDAV-shaped
/// rule, stated for EAS too).
#[tokio::test]
async fn an_occurrence_delete_without_the_base_refuses() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = adapter_at(&server);
    let deletion = EventDeletion {
        event: EventId::try_from("srv:ev-9").unwrap(),
        uid: Uid::new("uid-standup").unwrap(),
        guard: None,
        target: DeleteTarget::Occurrence {
            occurrence: Occurrence::starting(zoned("2026-09-08", "09:00:00")),
            stamp: stamp(),
        },
    };
    let err = adapter
        .delete_event(&account(), None, &deletion)
        .await
        .expect_err("the base is required");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert_eq!(server.count(), 0);
}

/// Already-gone is success: a per-item status 8 ("object not found")
/// resolves Ok — the trait's idempotent-delete rule.
#[tokio::test]
async fn an_already_gone_delete_is_success() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_req: &CapturedRequest, ordinal: usize| match ordinal {
            1 => MockResponse::wbxml(&seed_response("ev-key-2")),
            2 => MockResponse::wbxml(&upsync_response(
                "ev-key-3",
                vec![item_status("srv:ev-9", "8", false)],
            )),
            _ => MockResponse::bare(500),
        }) as Handler,
    );
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    adapter
        .delete_event(
            &account(),
            Some(&series_event()),
            &EventDeletion::of(&series_event()),
        )
        .await
        .expect("status 8 is the idempotent success");
}

/// `put_event` is refused `InvalidState`, naming the supported verb — the
/// rejecting default the trait explicitly allows for a transport whose
/// update is already a patch.
#[tokio::test]
async fn put_event_is_refused_naming_the_patch_verb() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = adapter_at(&server);
    let write = engine_provider::EventWrite::unconditional(
        EventId::try_from("srv:ev-9").unwrap(),
        Uid::new("uid-standup").unwrap(),
        engine_core::raw::RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
    );
    let err = adapter
        .put_event(&account(), &write)
        .await
        .expect_err("EAS has no document PUT");
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    assert!(
        err.detail().contains("patch_event"),
        "names the path: {}",
        err.detail()
    );
    assert_eq!(server.count(), 0);
}

/// An unbound adapter refuses every calendar verb, and its capabilities
/// never advertised the family — the honest ladder.
#[tokio::test]
async fn an_unbound_adapter_refuses_the_write_verbs() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = provider_eas::adapter::EasAdapter::new(
        super::harness::client_at(&server.eas_url()),
        MailboxId::try_from("fid-inbox").unwrap(),
    );
    for class in [
        adapter
            .create_event(&account(), &draft())
            .await
            .expect_err("create refuses")
            .class(),
        adapter
            .patch_event(
                &account(),
                &series_event(),
                &EventEdit::new(
                    &series_event(),
                    PatchTarget::Series,
                    EventPatch::new(stamp()).summary("Renamed"),
                ),
            )
            .await
            .expect_err("patch refuses")
            .class(),
        adapter
            .delete_event(
                &account(),
                Some(&series_event()),
                &EventDeletion::of(&series_event()),
            )
            .await
            .expect_err("delete refuses")
            .class(),
    ] {
        assert_eq!(class, engine_core::error::FailureClass::InvalidState);
    }
    assert_eq!(server.count(), 0);
    assert_eq!(
        adapter.connection_info().capabilities,
        Capabilities::none()
            .with_mail()
            .with_message_source()
            .with_mail_writes()
            .with_submission()
            .with_scheduling_submission(),
        "an unbound adapter advertises exactly the mail family"
    );
}

/// The verb ladder: a calendar-bound adapter advertises `calendar_writes`
/// with the honest guard — `WriteGuard::Absent` (EAS Sync Change carries no
/// server revision tokens; last-write-wins) — and the by-construction
/// `OverrideSurvival::kept()` (a series Replace re-emits every exception
/// from the base it was given).
#[tokio::test]
async fn the_calendar_binding_flips_the_write_bit_with_the_absent_guard() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let adapter = adapter_at(&server);
    let caps = adapter.connection_info().capabilities;
    assert_eq!(caps.calendar_write_guard(), Some(WriteGuard::Absent));
    assert_eq!(
        caps.override_survival(),
        Some(OverrideSurvival::kept()),
        "the adapter's own write path preserves overrides by construction"
    );
}

/// An empty patch is a no-op receipt — no wire round at all.
#[tokio::test]
async fn an_empty_patch_is_a_noop_receipt() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        assert_eq!(ordinal, 1, "only the seeding pass runs");
        assert_eq!(req.cmd().as_deref(), Some("Sync"));
        MockResponse::wbxml(&seed_response("ev-key-2"))
    }) as Handler);
    let adapter = adapter_at(&server);
    seed(&adapter).await;
    let base = series_event();
    let edit = EventEdit::new(&base, PatchTarget::Series, EventPatch::new(stamp()));
    let receipt = adapter
        .patch_event(&account(), &base, &edit)
        .await
        .expect("an empty patch needs no wire round");
    assert_eq!(receipt.event.as_str(), "srv:ev-9");
    assert_eq!(server.count(), 1, "no second request went out");
}
