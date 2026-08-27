// SPDX-License-Identifier: MPL-2.0
//! Harness client construction: TLS configs, the reserved-name `EasConfig`,
//! and the test-local base64 encoder. Split from `fixtures.rs` (the
//! 500-line file ceiling), same `#[path]` module family.

use std::sync::OnceLock;

use provider_eas::{client::EasClient, types::EasConfig};

/// TLS config for the plain-HTTP scenarios (trust is never exercised over
/// http — this only satisfies the constructor; the `provider-graph` `tls()`
/// convention).
pub(crate) fn tls_http() -> &'static engine_tls::TlsClientConfig {
    static TLS: OnceLock<engine_tls::TlsClientConfig> = OnceLock::new();
    TLS.get_or_init(engine_tls::TlsClientConfig::bundled)
}

/// TLS config that accepts the mock server's self-signed cert — the
/// `dangerous-testing` feature, exactly as the gated live tests use it.
pub(crate) fn tls_accept_any() -> &'static engine_tls::TlsClientConfig {
    static TLS: OnceLock<engine_tls::TlsClientConfig> = OnceLock::new();
    TLS.get_or_init(engine_tls::TlsClientConfig::dangerous_accept_any)
}

/// An `EasClient` pointed at the mock server, Basic auth, reserved-name
/// identifiers only (`AGENTS.md` fixture rule).
pub(crate) fn client_at(base_url: &str) -> EasClient {
    EasClient::new(test_config(base_url), tls_http()).expect("harness client build")
}

/// Same as [`client_at`] but trusting the mock's self-signed TLS cert.
pub(crate) fn tls_client_at(base_url: &str) -> EasClient {
    EasClient::new(test_config(base_url), tls_accept_any()).expect("harness TLS client build")
}

/// The harness `EasConfig`: every identifier a reserved name.
pub(crate) fn test_config(base_url: &str) -> EasConfig {
    EasConfig {
        url: base_url.to_owned(),
        username: "user@example.test".into(),
        user: String::new(),
        password: "app-password".into(),
        protocol_version: "16.1".into(),
        device_id: "TESTDEVICE01".into(),
        device_type: "KylinsMail".into(),
        user_agent: "KylinsMail/1.0-harness".into(),
        policy_key: String::new(),
        auth_type: String::new(),
        auth: None,
    }
}

/// Install the harness's capturing logger (debug level). Called as the
/// first statement of the wire-level tests: `log::debug!/info!/warn!`
/// ARGUMENTS (the wire dumps, the redaction previews) only evaluate when a
/// logger is installed, so the tests that exercise them must run with one
/// from their first statement — otherwise the covered lines wobble with
/// test-parallel scheduling.
pub(crate) fn init_logger() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .is_test(true)
            .try_init();
    });
}
