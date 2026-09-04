// SPDX-License-Identifier: MPL-2.0
//! Live engine-cli PIM acceptance: `eas-sync --kind calendar` /
//! `--kind contacts` driven end to end against the shared EAS account —
//! the P2 counterpart of the offline `engine_cli_pim_flow` proofs, and the
//! same two-layer gating as every live test here (`#[ignore]` plus a no-op
//! when the gates are unset; see `live_eas.rs`'s run instructions).
//!
//! The calendar test also runs the `--create` round-trip — the one live
//! evidence the calendar WRITE path has (the offline harness pins the wire
//! shape; this proves the real server acks the Add and the re-sync
//! backfills the ServerId). The probe event is removed afterwards through
//! the adapter's own delete verb: the CLI has no delete surface, and the
//! contacts write smoke's self-cleaning discipline applies. A leftover
//! from a crashed run is not re-deleted (the uid is deterministic but the
//! ServerId is per-run) — remove `engine-cli probe` by hand if that
//! happens.

use super::*;

/// Whether any required live gate is unset (the caller then skips).
fn gates_unset() -> bool {
    std::env::var("EAS_LIVE_URL").is_err()
        || std::env::var("EAS_LIVE_USER").is_err()
        || std::env::var("EAS_LIVE_PASSWORD").is_err()
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// Live calendar acceptance: full + incremental rounds and the
/// create→re-sync round-trip, all through the CLI command against a fresh
/// store.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn engine_cli_calendar_full_incremental_and_create_round_trip_live() {
    if gates_unset() {
        eprintln!("live gates unset (EAS_LIVE_URL/USER/PASSWORD) — skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let mut call = args(&[
        "eas-sync",
        "--db",
        db.to_str().unwrap(),
        "--account",
        "acct-live-pim",
        "--kind",
        "calendar",
        "--rounds",
        "2",
        "--create",
        "--horizon-start",
        "2026-08-01",
        "--horizon-end",
        "2026-10-01",
    ]);
    if std::env::var("EAS_LIVE_INSECURE").is_ok() {
        call.push("--insecure".to_owned());
    }
    let out = engine_cli::run(&call)
        .await
        .expect("the live calendar run succeeds");
    eprintln!("{out}");
    assert!(out.contains("(calendar): "), "the header: {out}");
    assert!(out.contains("round 2/2"), "both rounds ran: {out}");
    assert!(
        out.contains("occurrences "),
        "the materialization summary: {out}"
    );
    assert!(out.contains("created "), "the probe create landed: {out}");

    // Self-cleaning: the `created <ServerId> (uid <uid>)` line names what
    // to remove.
    let created = out
        .lines()
        .find(|l| l.starts_with("created "))
        .expect("the create line rides the report");
    let server_id = created
        .trim_start_matches("created ")
        .split(' ')
        .next()
        .expect("the ServerId follows `created`");
    let uid = created
        .split("(uid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the uid rides the create line");
    remove_probe(server_id, uid).await;
}

/// Live contacts acceptance: the full discovery + card sync + people
/// rebuild, through the CLI command against a fresh store.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn engine_cli_contacts_full_live() {
    if gates_unset() {
        eprintln!("live gates unset (EAS_LIVE_URL/USER/PASSWORD) — skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eas.sqlite");
    let mut call = args(&[
        "eas-sync",
        "--db",
        db.to_str().unwrap(),
        "--account",
        "acct-live-pim",
        "--kind",
        "contacts",
        "--rounds",
        "1",
    ]);
    if std::env::var("EAS_LIVE_INSECURE").is_ok() {
        call.push("--insecure".to_owned());
    }
    let out = engine_cli::run(&call)
        .await
        .expect("the live contacts run succeeds");
    eprintln!("{out}");
    assert!(out.contains("(contacts): "), "the header: {out}");
    assert!(out.contains("books       +"), "the discovery line: {out}");
    assert!(out.contains("people "), "the people count: {out}");
}

/// Removes the `--create` probe event through the adapter's own delete
/// verb: a distinct device identity (the CLI owns `ENGINECLIEAS01`'s
/// partnership), one seeding `sync_events` pass to warm the collection
/// ledger (a cold ledger refuses writes), then the series delete.
async fn remove_probe(server_id: &str, uid: &str) {
    use engine_core::ids::{AccountId, CalendarId, EventId, Uid};
    use engine_provider::{DeleteTarget, EventDeletion, Provider as _};

    let Some(mut config) = live_config() else {
        return;
    };
    config.device_id = "KYLINSLIVETEST3".to_string();
    let probe = live_client(config.clone());
    let options = probe.options().await.expect("live OPTIONS round-trip");
    let negotiated =
        pick_protocol_version(&options.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("a common protocol version with the server");
    config.protocol_version = negotiated;
    let mut client = live_client(config);

    // The probe landed in the first discovered calendar (the CLI syncs
    // discovery order).
    let folders = client.folder_sync("0").await.expect("live FolderSync");
    let calendar = folders
        .changes
        .iter()
        .find(|f| f.folder_type == Some(8))
        .expect("a Calendar-class folder (type 8)");
    let adapter = provider_eas::EasAdapter::calendar_adapter(
        client,
        CalendarId::try_from(calendar.server_id.as_str()).expect("a ServerId keys a CalendarId"),
    );
    let account = AccountId::try_from("acct-live-pim").unwrap();
    adapter
        .sync_events(&account, None)
        .await
        .expect("the ledger-seeding pass");
    adapter
        .delete_event(
            &account,
            None,
            &EventDeletion {
                event: EventId::try_from(server_id).expect("the acked ServerId keys an EventId"),
                uid: Uid::new(uid).expect("the probe uid is a uid"),
                guard: None,
                target: DeleteTarget::Series,
            },
        )
        .await
        .expect("the probe event is removed");
}
