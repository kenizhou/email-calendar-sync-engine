// SPDX-License-Identifier: MPL-2.0
//! Adapter skeleton scenarios: `EasAdapter` stood on the `Provider` trait —
//! the `connection_info` shape before and after the OPTIONS first contact,
//! and the EAS scope overrides. The wire side rides the real transport
//! against the mock server; the trait side is the thing under test.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::SyncScope,
};
use engine_provider::{Capabilities, HttpVersion, Provider as _};
use provider_eas::adapter::EasAdapter;

use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn account() -> AccountId {
    AccountId::try_from("acct-eas-1").unwrap()
}

fn folder() -> MailboxId {
    MailboxId::try_from("fid-inbox").unwrap()
}

/// An adapter bound to the mock server's endpoint, per the
/// `GraphProvider::new`-bound-to-one-folder precedent.
fn adapter_at(server: &MockServer) -> EasAdapter {
    EasAdapter::new(client_at(&server.eas_url()), folder())
}

/// The pre-connect shape: nothing has gone out, so nothing is claimed and
/// nothing is observed. Capabilities are the **verb ladder** — this task
/// lands connection facts and scopes only, so nothing is advertised; each
/// bit turns on when the verb honoring it lands, never before.
#[tokio::test]
async fn pre_connect_connection_info_reports_no_transport_facts() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(200)) as Handler);
    let adapter = adapter_at(&server);

    let info = adapter.connection_info();
    assert_eq!(
        info.capabilities,
        Capabilities::none(),
        "T2 ladder: no verb has landed, so nothing may be advertised"
    );
    assert_eq!(
        info.tls_version, None,
        "reqwest never reports the negotiated TLS version"
    );
    assert_eq!(
        info.http_version, None,
        "no exchange yet — the OPTIONS first contact has not happened"
    );
    assert_eq!(
        info.concurrent_fetches, 1,
        "no measured EAS ceiling yet — the ConnectionInfo default"
    );
    assert_eq!(
        adapter.protocol_version(),
        None,
        "nothing negotiated before the OPTIONS exchange"
    );
    assert_eq!(
        server.count(),
        0,
        "reading connection_info is free: no request may go out"
    );
}

/// The OPTIONS exchange is EAS's session-discovery step: after it, the
/// negotiated protocol version is applied to the client (every later command
/// carries it as `MS-ASProtocolVersion`) and `connection_info` reflects the
/// HTTP version the first contact spoke — the JMAP/CalDAV connect-time
/// precedent (Graph, with no discovery step, stays `None` until its first
/// fetch). Capabilities still follow the verb ladder, not the server's
/// OPTIONS answer.
#[tokio::test]
async fn options_negotiation_populates_connection_info() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        assert_eq!(req.method, "OPTIONS", "negotiate must send HTTP OPTIONS");
        MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "2.5,12.1,14.0,14.1,16.0,16.1")
            .with_header(
                "MS-ASProtocolCommands",
                "Sync,SendMail,Provision,FolderSync,Ping",
            )
    }) as Handler);
    let mut adapter = adapter_at(&server);

    let negotiated = adapter.negotiate().await.expect("OPTIONS negotiation");
    assert_eq!(
        negotiated, "16.1",
        "the last client-known server entry wins"
    );
    assert_eq!(
        adapter.protocol_version(),
        Some("16.1"),
        "the adapter holds the negotiated version (never ConnectionInfo — a host must not branch on it)"
    );

    let info = adapter.connection_info();
    assert_eq!(
        info.http_version,
        Some(HttpVersion::Http1_1),
        "the first contact observed the transport's HTTP version"
    );
    assert_eq!(
        info.capabilities,
        Capabilities::none(),
        "capabilities are the verb ladder, not the server's advertised command list"
    );

    // The OPTIONS request itself carried the Basic credential (the
    // session_options precedent): negotiation is an authenticated exchange.
    let captured = server.request(1);
    let auth = captured.header("authorization").expect("Authorization");
    assert!(auth.starts_with("Basic "), "Basic auth header, got {auth}");
    assert_eq!(
        captured.header("user-agent"),
        Some("KylinsMail/1.0-harness")
    );
}

/// A server advertising only versions below every one this client speaks
/// cannot be negotiated with — an explicit connect-time failure, never a
/// silent fall back to the configured default version.
#[tokio::test]
async fn negotiate_without_a_common_version_fails() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "2.5,12.1")
            .with_header("MS-ASProtocolCommands", "Sync")
    }) as Handler);
    let mut adapter = adapter_at(&server);

    let err = adapter.negotiate().await.expect_err("no common version");
    let msg = err.to_string();
    assert!(
        msg.contains("2.5") && msg.contains("14.1"),
        "error must name both sides of the failed intersection: {msg}"
    );
    assert_eq!(
        adapter.protocol_version(),
        None,
        "a failed negotiation records nothing"
    );
}

/// The scope overrides name the T1 EAS variants: the FolderSync hierarchy as
/// the per-account container scope, the bound folder's collection as the
/// email scope — the IMAP/Graph per-folder binding precedent (the
/// cross-folder fan-out is the orchestrator's job).
#[tokio::test]
async fn scopes_are_eas_shaped() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(200)) as Handler);
    let adapter = adapter_at(&server);

    assert_eq!(
        adapter.mailbox_scope(&account()),
        SyncScope::EasFolderList { account: account() }
    );
    assert_eq!(
        adapter.email_scope(&account()),
        SyncScope::EasFolder {
            account: account(),
            folder: folder()
        }
    );
    assert_eq!(
        server.count(),
        0,
        "scopes are facts about the binding, not the wire"
    );
}
