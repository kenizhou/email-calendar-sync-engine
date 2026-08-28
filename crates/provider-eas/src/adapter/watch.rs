// SPDX-License-Identifier: MPL-2.0
//! The `Watch` session over EAS `Ping` ([MS-ASPing]): one bound folder,
//! long-polled, mapped onto the engine's two-event contract.
//!
//! ## Mapping
//!
//! **Status 2 ("Changes") → [`WatchEvent::Changed`]** — and a non-empty
//! changed-folder list answers `Changed` whatever the status label says
//! (the list only exists when changes occurred — live evidence
//! 2026-08-03, the mislabel defense). **Status 1 ("Expired") →
//! [`WatchEvent::KeepAlive`]** — the socket survived the whole hold, which
//! is also the signal to grow the heartbeat. Returning an event leaves the
//! session watching; the next `next()` is another Ping round.
//!
//! **Status 5** (requested interval out of range) is absorbed before it
//! can become an error: `EasClient::ping` retries once CARRYING the
//! server's interval (asserted on the wire in the harness scenarios) and
//! surfaces it as `adopted_heartbeat`; the watcher tunes to the adopted
//! value clamped into its band, and the round classifies by the retry's
//! own answer.
//!
//! **Error statuses** classify through the Ping family table
//! ([`ping_status_error`](super::error::ping_status_error)): 7 (hierarchy
//! changed) is `NeedsResync`, the rest permanent. **Transport drops** tune
//! the heartbeat DOWN before surfacing retryable — long-held sockets die
//! in proxy/NAT idle zones, and a shorter hold survives them (the host
//! owns the reconnect policy; the Watch module docs' division).
//!
//! ## The heartbeat band (self-tuning, adapter-internal)
//!
//! Ported from the Kylins client's `sync/eas_source/heartbeat.rs`
//! (live-proven there since 2026-08): floor 300 s / cap 900 s / step
//! 300 s. Clean expiry grows, a transport drop shrinks, a server
//! directive sets exactly (clamped). The cap sits at 15 min because the
//! test Exchange HOLDS a ping without interrupting it for a new change
//! (live evidence 2026-08-04) — a change inside a long hold only surfaces
//! at the hold's end, so a longer hold buys nothing. Tuning is
//! adapter-internal policy (the trait has no seam for it); hosts wanting
//! it across restarts read [`EasPingWatcher::heartbeat_secs`] and restore
//! with [`EasPingWatcher::set_heartbeat_secs`] when building the session.

use engine_core::ids::MailboxId;
use engine_provider::{ProviderError, ProviderResult, Watch, WatchEvent};

use super::error::{ping_status_error, provider_error};
use crate::{
    client::{EasClient, EasError},
    types::{PingCollection, PingRequest},
};

/// Heartbeat floor (seconds) — also the default a fresh session starts at.
/// 5 min: the minimum that keeps the long-poll meaningfully "push" rather
/// than a rapid poll.
const PING_HEARTBEAT_FLOOR_SECS: u32 = 300;

/// Heartbeat ceiling (seconds, 15 min) — comfortably under the EAS server
/// max (~3540 s) and under typical proxy/NAT idle-kill zones; see the
/// module docs for why growing past it buys nothing.
const PING_HEARTBEAT_CAP_SECS: u32 = 900;

/// The tuning step (seconds) applied per outcome.
const PING_HEARTBEAT_STEP_SECS: u32 = 300;

/// What one Ping cycle told the tuner (the Kylins `PingOutcome`, verbatim
/// semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingOutcome {
    /// The heartbeat elapsed cleanly (status 1, no network trouble) — the
    /// socket survived the whole interval, so it can grow.
    CleanExpiry,
    /// The connection died before the heartbeat elapsed — pull the
    /// interval back.
    NetworkTimeout,
    /// The server advertised an interval (status 5) — set it exactly,
    /// clamped into the band.
    ServerOverride(u32),
}

/// Self-tuning heartbeat, clamped into the band. Pure — the unit tests pin
/// the whole matrix.
#[must_use]
fn tune_heartbeat(current_secs: u32, outcome: PingOutcome) -> u32 {
    match outcome {
        PingOutcome::CleanExpiry => current_secs
            .saturating_add(PING_HEARTBEAT_STEP_SECS)
            .clamp(PING_HEARTBEAT_FLOOR_SECS, PING_HEARTBEAT_CAP_SECS),
        PingOutcome::NetworkTimeout => current_secs
            .saturating_sub(PING_HEARTBEAT_STEP_SECS)
            .clamp(PING_HEARTBEAT_FLOOR_SECS, PING_HEARTBEAT_CAP_SECS),
        PingOutcome::ServerOverride(secs) => {
            secs.clamp(PING_HEARTBEAT_FLOOR_SECS, PING_HEARTBEAT_CAP_SECS)
        }
    }
}

/// A `Ping` watch session for the adapter's bound folder — the EAS answer
/// to the IMAP `ImapWatcher` dedicated-connection shape: the session OWNS a
/// clone of the negotiated client (clones share the pooled transport and
/// carry the negotiated protocol version), so it never contends the
/// adapter's verb lock for its long holds. Built with
/// [`EasAdapter::watcher`](super::EasAdapter::watcher); the trait has no
/// watch accessor (the IMAP/JMAP concrete-type handout precedent — the
/// fork record in `eas.md`).
///
/// `Send` but not `Sync`, exactly as the trait wants: one driver, `&mut`
/// per call.
#[derive(Debug)]
pub struct EasPingWatcher {
    /// The negotiated client clone this session's Ping rounds ride.
    client: EasClient,
    /// The bound folder — one session watches one collection.
    folder: MailboxId,
    /// The tuned heartbeat the next round carries.
    heartbeat: u32,
}

impl EasPingWatcher {
    /// Builds the session at the band floor — the
    /// [`EasAdapter::watcher`](super::EasAdapter::watcher) path.
    pub(crate) fn new(client: EasClient, folder: MailboxId) -> Self {
        Self {
            client,
            folder,
            heartbeat: PING_HEARTBEAT_FLOOR_SECS,
        }
    }

    /// The heartbeat the next Ping round will carry — what a host persists
    /// to keep the tuning across restarts.
    #[must_use]
    pub fn heartbeat_secs(&self) -> u32 {
        self.heartbeat
    }

    /// Restores a persisted heartbeat, clamped into the band (the restore
    /// side of the persist-and-restart flow).
    pub fn set_heartbeat_secs(&mut self, secs: u32) {
        self.heartbeat = secs.clamp(PING_HEARTBEAT_FLOOR_SECS, PING_HEARTBEAT_CAP_SECS);
    }
}

#[async_trait::async_trait]
impl Watch for EasPingWatcher {
    async fn next(&mut self) -> ProviderResult<WatchEvent> {
        let result = match self
            .client
            .ping(&PingRequest {
                heartbeat_interval: self.heartbeat,
                monitored_collections: vec![PingCollection {
                    collection_id: self.folder.as_str().to_owned(),
                    class: "Email".to_owned(),
                }],
            })
            .await
        {
            Ok(result) => result,
            // A transport death before the hold elapsed is the idle-kill
            // signature — tune down, then let the host's reconnect policy
            // act on the surfaced retryable error.
            Err(e @ EasError::Transport(_)) => {
                self.heartbeat = tune_heartbeat(self.heartbeat, PingOutcome::NetworkTimeout);
                return Err(provider_error(e));
            }
            Err(e) => return Err(provider_error(e)),
        };

        // The interval the server handed back this round (the status-5
        // retry's adoption, or the raw status-5 directive). A directive
        // outranks the clean-expiry growth — growing immediately after the
        // server spoke would fight it.
        let directive = result.adopted_heartbeat.or({
            matches!(result.status.as_str(), "5")
                .then_some(result.heartbeat_interval)
                .flatten()
        });
        if let Some(adopted) = directive {
            self.heartbeat = tune_heartbeat(self.heartbeat, PingOutcome::ServerOverride(adopted));
        }

        // The changed-folder list is a change signal in its own right,
        // whatever the status label says (the mislabel defense).
        if result.status == "Changes" || !result.folders.is_empty() {
            return Ok(WatchEvent::Changed);
        }
        if result.status == "Expired" {
            if directive.is_none() {
                self.heartbeat = tune_heartbeat(self.heartbeat, PingOutcome::CleanExpiry);
            }
            return Ok(WatchEvent::KeepAlive);
        }
        if result.status == "5" && directive.is_some() {
            // The client's adopted retry surfaced the 5 itself — the
            // directive is already tuned in; hold with it.
            return Ok(WatchEvent::KeepAlive);
        }
        // Any other status is a protocol failure: numeric ones classify
        // through the Ping family table, anything else is uninterpretable.
        match result.status.parse::<u32>() {
            Ok(status) => Err(ping_status_error(status)),
            Err(_) => Err(ProviderError::permanent(format!(
                "Ping answered an uninterpretable status {}",
                result.status
            ))),
        }
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
