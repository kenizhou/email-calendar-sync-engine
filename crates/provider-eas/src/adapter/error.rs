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
        // source of truth, not a second table here. Only FolderSync flows
        // through the adapter today; the Sync family's classifier wires up
        // with the message verbs.
        EasError::CommandStatus { status, .. } => {
            match crate::status::recovery_action_for_folder_sync(*status) {
                RecoveryAction::ResetSyncKey => FailureClass::NeedsResync,
                RecoveryAction::RetryProvision => FailureClass::Retryable,
                _ => FailureClass::Permanent,
            }
        }
        EasError::InvalidRequest(_) => FailureClass::InvalidState,
        EasError::Auth(_) => FailureClass::Authentication,
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
}
