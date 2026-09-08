//! The `eas-sync` command: one EAS account driven through the engine's own
//! sync — the T8 acceptance path ("drive an EAS account from the engine
//! CLI").
//!
//! Like `dav` before it, this exists so a verdict printed here is the
//! verdict a host would get: the command builds real
//! [`EasAdapter`](provider_eas::EasAdapter)s and hands them to
//! [`engine_sync::sync_mail`] — the engine's own fan-out (mailbox scope,
//! thread repair, inbox-first bounded folder passes, recipient backfill) —
//! against a real [`SqliteStore`]. Discovery, adapters, negotiation, and
//! the rounds loop are the only CLI-owned parts.
//!
//! ## The flow
//!
//! 1. **Negotiate** one client (`OPTIONS` → the shared version, the `EasAdapter::negotiate` dance
//!    performed client-side so every folder adapter clones the negotiated client — clones carry the
//!    version).
//! 2. **Discover** the folder list through the adapter's own `sync_mailboxes` (one bootstrap
//!    FolderSync, unapplied — the engine's mailbox pass inside `sync_mail` is the applied one).
//! 3. **Fan out**: one bound-folder adapter per selected folder, then `sync_mail` — the store
//!    persists each folder's cursor, so round 2 against the same `--db` is the incremental pass by
//!    construction (the P0 exit: full + incremental through one command).

use core::time::Duration;

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::SyncUpdate,
};
use engine_provider::{Provider as _, ScopeSync};
use engine_sync::{IgnoreCommits, MailSyncReport, StreamTuning, sync_mail};
use provider_eas::{
    CLIENT_KNOWN_PROTOCOL_VERSIONS, EasAdapter,
    client::{EasClient, pick_protocol_version},
    types::EasConfig,
};
use store_sqlite::SqliteStore;

use crate::{CliError, WORKER};

/// The lease TTL for the sync scopes this command claims. The harness clock
/// is fixed, so any positive window holds every claim for the run — the
/// same reasoning as `ingest`'s TTL. Shared with the PIM arms
/// (`eas_pim.rs`), which claim under the same discipline.
pub(crate) const LEASE_TTL: Duration = Duration::from_mins(5);

/// The EAS device identity this diagnostic client presents ([MS-ASHTTP]:
/// alphanumeric, ≤16 chars).
const DEVICE_ID: &str = "ENGINECLIEAS01";

/// Where a connection's credentials come from: explicit flags, falling
/// back to the `EAS_LIVE_*` environment gates (the same convention as the
/// crate's live suites, so the P0 live run and the CI offline run share one
/// command surface).
pub(crate) struct EasTarget {
    /// The `Microsoft-Server-ActiveSync` endpoint.
    pub url: String,
    /// The Basic-auth identity.
    pub user: String,
    /// The account password / app password.
    pub password: String,
    /// The Basic-auth identity when it differs from the EAS `User` param
    /// (the on-prem lab shape: auth realm ≠ mailbox address). Resolved
    /// from `EAS_LIVE_USERNAME` only — the live suites' gate, unset in
    /// every other shape so identity equals `user`.
    pub auth_user: Option<String>,
    /// Whether to trust any certificate (the lab server's self-signed
    /// cert). Only compilable with the `eas-insecure-tls` diagnostic
    /// feature.
    pub insecure: bool,
}

impl EasTarget {
    /// Resolves the connection: each field from `--url/--user/--password`
    /// or its `EAS_LIVE_*` variable.
    pub(crate) fn resolve(
        url: Option<&str>,
        user: Option<&str>,
        password: Option<&str>,
        insecure: bool,
    ) -> Result<Self, CliError> {
        let missing = |flag: &str, var: &str| {
            CliError::Usage(format!(
                "eas-sync needs --{flag} (or the {var} environment variable)"
            ))
        };
        Ok(Self {
            url: url
                .map(str::to_owned)
                .or_else(|| std::env::var("EAS_LIVE_URL").ok())
                .ok_or_else(|| missing("url", "EAS_LIVE_URL"))?,
            user: user
                .map(str::to_owned)
                .or_else(|| std::env::var("EAS_LIVE_USER").ok())
                .ok_or_else(|| missing("user", "EAS_LIVE_USER"))?,
            password: password
                .map(str::to_owned)
                .or_else(|| std::env::var("EAS_LIVE_PASSWORD").ok())
                .ok_or_else(|| missing("password", "EAS_LIVE_PASSWORD"))?,
            auth_user: std::env::var("EAS_LIVE_USERNAME").ok(),
            insecure,
        })
    }

    /// The TLS config: bundled roots, or — behind the diagnostic feature —
    /// the test-only accept-any config for the lab server's self-signed
    /// cert.
    // The `Err` arm exists only in builds WITHOUT the diagnostic feature,
    // so a feature-on compile (provider-eas's dev graph) sees an all-`Ok`
    // body — the wrap is load-bearing in the shipped shape.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the refusal arm is compiled out only with eas-insecure-tls"
    )]
    fn tls(&self) -> Result<engine_tls::TlsClientConfig, CliError> {
        if self.insecure {
            #[cfg(feature = "eas-insecure-tls")]
            {
                return Ok(engine_tls::TlsClientConfig::dangerous_accept_any());
            }
            #[cfg(not(feature = "eas-insecure-tls"))]
            {
                return Err(CliError::Usage(
                    "--insecure needs a build with --features eas-insecure-tls \
                     (the lab server's self-signed cert; production builds never \
                     ship that path)"
                        .to_owned(),
                ));
            }
        }
        Ok(engine_tls::TlsClientConfig::bundled())
    }
}

/// Runs the account's mail sync `rounds` times against `store`.
///
/// `folders` selects a subset of the discovered mail folders by ServerId
/// (empty = all). Returns the rendered report; a pass with any failed
/// scope returns it as [`CliError::Eas`] instead, so the shell sees the
/// failure without losing the report.
pub(crate) async fn eas_sync(
    store: &SqliteStore<crate::ManualClock>,
    account: &AccountId,
    target: &EasTarget,
    folders: &[MailboxId],
    rounds: usize,
    tuning: StreamTuning,
) -> Result<String, CliError> {
    let mut client = configured_client(target)?;

    // The `EasAdapter::negotiate` dance, client-side: one OPTIONS, the
    // shared version applied to the client every later command carries.
    let version = negotiate(&mut client).await?;

    // Discovery through the adapter's own verb (the class filtering is
    // theirs): one bootstrap FolderSync, unapplied. The bound folder of a
    // discovery adapter is inert — `sync_mailboxes` never reads it.
    let discovery = EasAdapter::new(
        client.clone(),
        MailboxId::try_from("0").expect("a non-empty discovery placeholder"),
    );
    let ScopeSync { update, .. } = discovery.sync_mailboxes(account, None).await?;
    let discovered: Vec<MailboxId> = match update {
        SyncUpdate::Snapshot { objects, .. } => objects.into_iter().map(|m| m.id).collect(),
        SyncUpdate::Delta { changed, .. } => changed.into_iter().map(|m| m.id).collect(),
    };
    let selected = selected_folders(&discovered, folders)?;

    let mut out = format!(
        "eas-sync {}: {} folder(s), protocol {version}\n",
        account.as_str(),
        selected.len()
    );
    let mut failed = false;
    for round in 1..=rounds {
        use std::fmt::Write as _;
        let _ = writeln!(out, "round {round}/{rounds}");
        let adapters: Vec<EasAdapter> = selected
            .iter()
            .map(|folder| EasAdapter::new(client.clone(), folder.clone()))
            .collect();
        let report = sync_mail(
            &adapters,
            store,
            account,
            engine_store::WorkerId::new(WORKER),
            LEASE_TTL,
            tuning,
            &IgnoreCommits,
        )
        .await;
        failed |= render(&mut out, &report);
    }
    if failed {
        Err(CliError::Eas(out))
    } else {
        Ok(out)
    }
}

/// The configured protocol client for `target` (credentials, device
/// identity, TLS) — the shared first step of every `eas-sync` arm.
pub(crate) fn configured_client(target: &EasTarget) -> Result<EasClient, CliError> {
    EasClient::new(
        EasConfig {
            url: target.url.clone(),
            username: target
                .auth_user
                .clone()
                .unwrap_or_else(|| target.user.clone()),
            user: target.user.clone(),
            password: target.password.clone(),
            device_id: DEVICE_ID.to_owned(),
            device_type: "EngineCli".to_owned(),
            user_agent: "engine-cli".to_owned(),
            ..EasConfig::default()
        },
        &target.tls()?,
    )
    .map_err(|e| CliError::Eas(e.to_string()))
}

/// One OPTIONS exchange: the server's advertised versions negotiated
/// against the client's, the shared version applied to `client` so every
/// later command carries it. Returns the negotiated version string.
pub(crate) async fn negotiate(client: &mut EasClient) -> Result<String, CliError> {
    let options = client
        .options()
        .await
        .map_err(|e| CliError::Eas(e.to_string()))?;
    let advertised = options.protocol_versions.join(", ");
    let version =
        pick_protocol_version(&advertised, &CLIENT_KNOWN_PROTOCOL_VERSIONS).ok_or_else(|| {
            CliError::Eas(format!(
                "the server's protocol versions [{advertised}] share none with the client \
                 ([{}])",
                CLIENT_KNOWN_PROTOCOL_VERSIONS.join(", ")
            ))
        })?;
    client.set_protocol_version(version.clone());
    Ok(version)
}

/// The folders to sync: all discovered, or the requested subset — a
/// requested id the server never named is an error, not a silent skip.
fn selected_folders(
    discovered: &[MailboxId],
    requested: &[MailboxId],
) -> Result<Vec<MailboxId>, CliError> {
    if requested.is_empty() {
        return Ok(discovered.to_vec());
    }
    for id in requested {
        if !discovered.contains(id) {
            return Err(CliError::Usage(format!(
                "--folder {} is not one of the server's mail folders",
                id.as_str()
            )));
        }
    }
    Ok(requested.to_vec())
}

/// Renders one round's report; returns whether any scope failed.
fn render(out: &mut String, report: &MailSyncReport) -> bool {
    use std::fmt::Write as _;
    let mut failed = false;
    if let Some(mailboxes) = &report.mailboxes {
        match mailboxes {
            Ok(applied) => {
                let _ = writeln!(
                    out,
                    "  mailboxes  +{} -{}",
                    applied.upserted, applied.tombstoned
                );
            }
            Err(err) => {
                failed = true;
                let _ = writeln!(out, "  mailboxes  FAILED: {err}");
            }
        }
    }
    for folder in &report.folders {
        match &folder.result {
            Ok(applied) => {
                let _ = writeln!(
                    out,
                    "  {:<10} +{} -{}",
                    scope_name(&folder.scope),
                    applied.upserted,
                    applied.tombstoned
                );
            }
            Err(err) => {
                failed = true;
                let _ = writeln!(out, "  {:<10} FAILED: {err}", scope_name(&folder.scope));
            }
        }
    }
    if let Err(err) = &report.account_steps {
        failed = true;
        let _ = writeln!(out, "  account steps FAILED: {err}");
    }
    failed
}

/// The folder a scope names (the EAS member scope carries its ServerId).
fn scope_name(scope: &engine_core::sync::SyncScope) -> String {
    match scope {
        engine_core::sync::SyncScope::EasFolder { folder, .. } => folder.as_str().to_owned(),
        _ => "scope".to_owned(),
    }
}
