// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the watch slice (`watch.rs`) — the `#[path]` split the
//! repo uses to hold the 500-line cap. The three wire states, the retry
//! threading, and the error statuses live in the harness
//! `adapter_watch_flow` scenarios; these pin the pure tuning band.

use super::*;

/// The band matrix: growth caps, drops floor, directives clamp — the
/// Kylins live-proven semantics verbatim.
#[test]
fn the_heartbeat_band_grows_caps_floors_and_clamps() {
    use PingOutcome::*;
    assert_eq!(tune_heartbeat(300, CleanExpiry), 600, "a step up");
    assert_eq!(tune_heartbeat(900, CleanExpiry), 900, "the cap holds");
    assert_eq!(tune_heartbeat(600, NetworkTimeout), 300, "a step down");
    assert_eq!(tune_heartbeat(300, NetworkTimeout), 300, "the floor holds");
    assert_eq!(
        tune_heartbeat(600, ServerOverride(60)),
        300,
        "a low directive clamps to the floor"
    );
    assert_eq!(
        tune_heartbeat(600, ServerOverride(3540)),
        900,
        "a high directive clamps to the cap"
    );
    assert_eq!(
        tune_heartbeat(600, ServerOverride(700)),
        700,
        "an in-band directive sets exactly"
    );
}

/// The restore path clamps too — a stale persisted value cannot leave the
/// band.
#[test]
fn a_restored_heartbeat_clamps_into_the_band() {
    let mut watch = EasPingWatcher::new(
        crate::client::EasClient::new(
            crate::types::EasConfig::default(),
            &engine_tls::TlsClientConfig::bundled(),
        )
        .expect("offline client builds"),
        engine_core::ids::MailboxId::try_from("fid-inbox").unwrap(),
    );
    assert_eq!(watch.heartbeat_secs(), PING_HEARTBEAT_FLOOR_SECS);
    watch.set_heartbeat_secs(480);
    assert_eq!(watch.heartbeat_secs(), 480);
    watch.set_heartbeat_secs(u32::MAX);
    assert_eq!(watch.heartbeat_secs(), PING_HEARTBEAT_CAP_SECS);
    watch.set_heartbeat_secs(0);
    assert_eq!(watch.heartbeat_secs(), PING_HEARTBEAT_FLOOR_SECS);
}
