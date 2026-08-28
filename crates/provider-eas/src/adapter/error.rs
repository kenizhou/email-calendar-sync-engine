// SPDX-License-Identifier: MPL-2.0
//! [`EasError`] → [`ProviderError`] classification — the EAS side of the seam
//! every adapter owns (`provider-graph`'s `error.rs` is the code precedent).
//!
//! Callers branch on [`FailureClass`] and never on the provider kind
//! (`providers.md`); the original protocol error stays reachable through the
//! `source` chain. The in-transport recovery layers (449→Provision,
//! 401-OAuth→refresh, 451→redirect) run *inside* `EasClient` before anything
//! reaches this module — what arrives here is what they could not absorb.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

use crate::{client::EasError, status::RecoveryAction};

/// Converts a protocol failure into the engine's classified
/// [`ProviderError`], keeping the original as its `source`.
pub(super) fn provider_error(err: EasError) -> ProviderError {
    let class = failure_class(&err);
    ProviderError::new(class, err.to_string()).with_source(err)
}

/// The engine-neutral class an `EasError` maps to.
fn failure_class(err: &EasError) -> FailureClass {
    match err {
        // A connect/timeout/request failure is transient (the Graph
        // transport-error precedent).
        EasError::Transport(_) => FailureClass::Retryable,
        EasError::HttpStatus { status, .. } => http_class(*status),
        // Bytes that do not decode as the protocol requires are a permanent
        // mismatch — resending the same request cannot fix them (the Graph
        // Json/Protocol precedent).
        EasError::Wbxml(_) | EasError::UnexpectedRoot { .. } => FailureClass::Permanent,
        // In-body command statuses classify through the per-family
        // `RecoveryAction` table the crate already owns (`status.rs`) — one
        // source of truth, not a second table here. The Sync family carries
        // its own variant (Sync 3 = invalid key → `NeedsResync`, Sync 5/16 =
        // transient → `Retryable`, Sync 6 = conversion error, not transient
        // → `Permanent`); the family-untagged `CommandStatus` arm serves the
        // families whose verbs have not landed adapter slices yet and
        // classifies through the FolderSync table — today that is exactly
        // the FolderSync verb itself.
        EasError::SyncStatus { status, .. } => {
            class_of_action(crate::status::recovery_action_for_sync(*status))
        }
        EasError::CommandStatus { status, .. } => {
            class_of_action(crate::status::recovery_action_for_folder_sync(*status))
        }
        EasError::InvalidRequest(_) => FailureClass::InvalidState,
        EasError::Auth(_) => FailureClass::Authentication,
    }
}

/// The engine class a `RecoveryAction` resolves to — the shared tail of every
/// family classifier above. Resync-shaped actions (reset the sync key, run
/// FolderSync) map to `NeedsResync` (the orchestrator drops the cursor and
/// re-runs); the retry-shaped ones to `Retryable`; everything else surfaces
/// permanently.
fn class_of_action(action: RecoveryAction) -> FailureClass {
    match action {
        RecoveryAction::ResetSyncKey | RecoveryAction::RunFolderSync => FailureClass::NeedsResync,
        RecoveryAction::RetryProvision
        | RecoveryAction::RefreshToken
        | RecoveryAction::FollowRedirect
        | RecoveryAction::RetryTransient => FailureClass::Retryable,
        // `Ok` never reaches the classifier (only non-success statuses are
        // converted to errors) — a healthy action on an error path is a
        // contradiction that must not be silently retried.
        RecoveryAction::Ok | RecoveryAction::SurfaceAuth | RecoveryAction::SurfacePermanent => {
            FailureClass::Permanent
        }
    }
}

/// Maps an HTTP status that escaped the transport's own recovery layers onto
/// the engine classes, mirroring `recovery_action_for_http`: auth failures
/// are `Authentication`, throttling is `RateLimited`, and the server-error /
/// provision-demand shapes are `Retryable` — the engine's poll loop owns the
/// retry. A hop-capped 451 redirect cycle is `Permanent`: the server is
/// broken, and resending walks the same loop.
///
/// `HttpStatus.retry_after` (an absolute epoch) is deliberately not converted
/// into `ProviderError::retry_after` (a relative window): the conversion
/// needs a clock and the class alone already routes the poll loop.
fn http_class(status: u16) -> FailureClass {
    match status {
        401 | 403 => FailureClass::Authentication,
        429 => FailureClass::RateLimited,
        449 | 500..=599 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    }
}

/// The surfaced error for a non-success Sync collection status — through the
/// family-tagged variant, so it classifies via the Sync table with the
/// protocol failure kept as the `source` chain. Shared by the stream verb
/// and the upsync (`SetKeywords`) edit path.
pub(super) fn sync_status_error(status: u32) -> ProviderError {
    provider_error(EasError::SyncStatus {
        status,
        message: format!(
            "Sync failed: {}",
            crate::commands::common_status_message(status)
                .unwrap_or("collection status not success")
        ),
    })
}

/// The surfaced error for a SendMail in-body status ([MS-ASCMD]
/// §2.2.3.162): SendMail success is an empty body, so a body CAN only carry
/// a failure. Classifies through the SendMail family table (`status.rs`).
pub(super) fn compose_status_error(status: u32) -> ProviderError {
    let class = class_of_action(crate::status::recovery_action_for_send_mail(status));
    ProviderError::new(class, format!("SendMail failed: in-body status {status}"))
}

/// The surfaced error for a MoveItems per-move failure ([MS-ASCMD]
/// §2.2.3.177.10 — the table whose success code is 3, not 1). The locked
/// shapes are transient (this adapter moves one item at a time, so status 5
/// can only be the item lock); the addressing failures are structural —
/// resending the same move walks into the same answer.
pub(super) fn move_status_error(status: u32) -> ProviderError {
    let class = match status {
        5 | 7 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    };
    ProviderError::new(
        class,
        format!(
            "MoveItems failed: status {status} — {}",
            crate::commands::move_items_status_message(status)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of(err: &EasError) -> FailureClass {
        failure_class(err)
    }

    #[test]
    fn transport_failures_are_retryable() {
        assert_eq!(
            class_of(&EasError::Transport("connect timed out".into())),
            FailureClass::Retryable
        );
    }

    #[test]
    fn http_statuses_map_to_engine_classes() {
        assert_eq!(
            class_of(&EasError::HttpStatus {
                status: 401,
                body: String::new(),
                retry_after: None,
                x_ms_location: None,
            }),
            FailureClass::Authentication
        );
        assert_eq!(
            class_of(&EasError::HttpStatus {
                status: 403,
                body: String::new(),
                retry_after: None,
                x_ms_location: None,
            }),
            FailureClass::Authentication
        );
        assert_eq!(
            class_of(&EasError::HttpStatus {
                status: 429,
                body: String::new(),
                retry_after: None,
                x_ms_location: None,
            }),
            FailureClass::RateLimited
        );
        assert_eq!(
            class_of(&EasError::HttpStatus {
                status: 503,
                body: String::new(),
                retry_after: None,
                x_ms_location: None,
            }),
            FailureClass::Retryable
        );
        // A 451 that escaped the hop cap is a redirect cycle — permanent.
        assert_eq!(
            class_of(&EasError::HttpStatus {
                status: 451,
                body: String::new(),
                retry_after: None,
                x_ms_location: Some("https://new.example.test".into()),
            }),
            FailureClass::Permanent
        );
    }

    #[test]
    fn folder_sync_statuses_follow_the_family_classifier() {
        // 9 (hierarchy out of date) is the EAS cannotCalculateChanges.
        assert_eq!(
            class_of(&EasError::CommandStatus {
                status: 9,
                message: "FolderSync failed: folder hierarchy out of date".into(),
            }),
            FailureClass::NeedsResync
        );
        // 142 escaped the transport's one provision retry — still retry-shaped.
        assert_eq!(
            class_of(&EasError::CommandStatus {
                status: 142,
                message: "FolderSync failed: device not provisioned".into(),
            }),
            FailureClass::Retryable
        );
        // 108 (device id invalid) and unknown codes are permanent.
        for status in [108, 999] {
            assert_eq!(
                class_of(&EasError::CommandStatus {
                    status,
                    message: "FolderSync failed".into(),
                }),
                FailureClass::Permanent
            );
        }
    }

    /// The Sync family classifies through its own table ([MS-ASCMD] "Status
    /// (Sync)"): 3 = invalid synchronization key → the "0" re-bootstrap
    /// recovery (`NeedsResync` when surfaced — the stream recovers internally
    /// first); 5/16 = transient server/retry errors → `Retryable`; 6 =
    /// client/server conversion error, explicitly NOT transient →
    /// `Permanent`. The same codes under FolderSync mean different things —
    /// the reason the family carries its own variant.
    #[test]
    fn sync_statuses_follow_the_sync_family_classifier() {
        assert_eq!(
            class_of(&EasError::SyncStatus {
                status: 3,
                message: "Sync failed: invalid synchronization key".into(),
            }),
            FailureClass::NeedsResync
        );
        assert_eq!(
            class_of(&EasError::SyncStatus {
                status: 12,
                message: "Sync failed: folder hierarchy changed".into(),
            }),
            FailureClass::NeedsResync
        );
        for status in [5, 16] {
            assert_eq!(
                class_of(&EasError::SyncStatus {
                    status,
                    message: "Sync failed: server error".into(),
                }),
                FailureClass::Retryable,
                "Sync {status} is transient"
            );
        }
        for status in [4, 6, 8] {
            assert_eq!(
                class_of(&EasError::SyncStatus {
                    status,
                    message: "Sync failed".into(),
                }),
                FailureClass::Permanent,
                "Sync {status} is not recoverable by resending"
            );
        }
    }

    #[test]
    fn codec_and_request_errors_are_permanent_and_invalid_state() {
        assert_eq!(
            class_of(&EasError::InvalidRequest("bad ids".into())),
            FailureClass::InvalidState
        );
        assert_eq!(
            class_of(&EasError::Auth("refresh grant dead".into())),
            FailureClass::Authentication
        );
    }

    #[test]
    fn the_protocol_error_stays_reachable_as_source() {
        let original = EasError::CommandStatus {
            status: 108,
            message: "FolderSync failed: device ID missing or invalid format".into(),
        };
        let provider = provider_error(original);
        assert_eq!(provider.class(), FailureClass::Permanent);
        assert!(
            provider.detail().contains("108"),
            "the surfaced detail carries the protocol failure: {}",
            provider.detail()
        );
        assert!(std::error::Error::source(&provider).is_some());
    }

    /// SendMail's in-body statuses classify through the SendMail family
    /// table: 132 is the transient server-unavailable shape; the auth and
    /// unknown shapes surface permanently through the shared funnel (the
    /// `SurfaceAuth` action deliberately rides `Permanent` there — the
    /// funnel's standing decision, not per-verb).
    #[test]
    fn send_mail_statuses_classify_per_the_compose_table() {
        assert_eq!(compose_status_error(132).class(), FailureClass::Retryable);
        assert_eq!(compose_status_error(130).class(), FailureClass::Permanent);
        assert_eq!(compose_status_error(999).class(), FailureClass::Permanent);
    }

    /// MoveItems' per-move failures: the item-locked shapes retry; the
    /// addressing failures (including the anomalous bare success-code-3
    /// without a DstMsgId) are structural.
    #[test]
    fn move_statuses_split_locked_from_structural() {
        for status in [5, 7] {
            assert_eq!(
                move_status_error(status).class(),
                FailureClass::Retryable,
                "MoveItems {status} is the item-locked shape"
            );
        }
        for status in [1, 2, 3, 4, 6] {
            assert_eq!(
                move_status_error(status).class(),
                FailureClass::Permanent,
                "MoveItems {status} is structural"
            );
        }
    }
}
