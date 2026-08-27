//! The outbox-mediated **mail** writes on `Engine` — submission (an engine-rendered
//! draft, or the caller's own final MIME bytes), edits and reporting a message as junk
//! / not junk / phishing — plus the pending-op state poll every write (mail and
//! calendar alike) is observed through. The calendar writes live in
//! `calendar_writes`, which additionally reconciles the store.

use engine_core::{
    ids::{AccountId, ProviderKey},
    write::PendingOpId,
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider};
use engine_store::{PendingOpState, StoreRead};
use engine_sync::{
    MailEditOutcome, ReportOutcome, SubmitOutcome, SyncError, edit_mail, report_message,
    submit_mail, submit_mail_source,
};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Submits `draft` for one account through the durable outbox: the draft is
    /// recorded as a pending op (idempotent by its `Message-ID`) **before** the
    /// provider send, so a crash or an ambiguous failure never loses or double-sends
    /// it (`north-star.md` Write Contract). Returns the sent message's key, its
    /// `Message-ID`, and the op id — pollable via [`Engine::pending_op_state`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the send fails: the op is first recorded
    /// `Failed` (with the failure class), or `NeedsConfirmation` for an ambiguous
    /// post-`DATA` SMTP loss — the outbox never blind-retries — and the error then
    /// returns. A store failure also surfaces as [`ApiError::Sync`].
    pub async fn submit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
    ) -> Result<SubmitOutcome, ApiError> {
        submit_mail(provider, &self.store, account, worker(), LEASE_TTL, draft)
            .await
            .map_err(map_sync_error)
    }

    /// Submits `source` — the caller's **own final MIME bytes** — through the same
    /// durable outbox as [`submit_mail`](Self::submit_mail): the bytes are recorded
    /// as a pending op (idempotent by the bytes' own `Message-ID`, sharing the Draft
    /// path's op namespace, so one message is one op through either path) **before**
    /// the provider send, and sent **verbatim** — never re-rendered. This is the
    /// host-crypto seam: a caller that renders its message and then signs or
    /// encrypts it (PGP/MIME, `multipart/signed`, S/MIME) submits the finished
    /// bytes here, because re-rendering them would strip the signature or break
    /// the envelope the recipient must verify.
    ///
    /// The caller's two obligations on the bytes themselves: **stamp the
    /// `Message-ID` before submitting** (the receipt, the op's keys and the
    /// reconciliation all hang off it — bytes without one are refused **before
    /// anything is enqueued**) and end them with a line terminator (SMTP's `DATA`
    /// terminator would otherwise corrupt the last line). `recipients` is the
    /// envelope: non-empty it is the exact `RCPT TO` set — where **Bcc** lives,
    /// delivered with no `Bcc` header ever entering the bytes; empty, the envelope
    /// derives from the bytes' own `To`/`Cc` headers (see
    /// [`Provider::submit_email_source`](engine_provider::Provider::submit_email_source)
    /// for the full Bcc semantics).
    ///
    /// `NeedsConfirmation` and reconciliation semantics are identical to
    /// [`submit_mail`](Self::submit_mail): an ambiguous post-`DATA` SMTP loss parks
    /// the op for confirmation — the outbox never blind-retries — and the sent copy
    /// reconciles by the `Message-ID` the caller stamped.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the bytes are refused before the enqueue (no
    /// `Message-ID`, no trailing line terminator) or the send fails (recorded
    /// `Failed` / `NeedsConfirmation` first). A store failure also surfaces as
    /// [`ApiError::Sync`].
    pub async fn submit_mail_source<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        source: &[u8],
        recipients: &[String],
    ) -> Result<SubmitOutcome, ApiError> {
        submit_mail_source(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            source,
            recipients,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Files the sender's copy of an **already-delivered** message, repairing a submission
    /// whose [`SubmitOutcome::sent_copy`] came back
    /// [`SentCopy::Unfiled`](engine_provider::SentCopy::Unfiled). Returns the filed copy's
    /// key.
    ///
    /// **Not outbox-mediated, deliberately.** The outbox exists so a side effect is never
    /// lost or repeated across a crash; this one sends nothing and the provider makes it
    /// idempotent by probing for the copy first, so a durable op would buy nothing and would
    /// add a second op recording the same submission. It is a repair a user asks for, and
    /// asking again is free.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the copy still could not be filed — the message stays
    /// sent either way, and the caller may offer the retry again.
    pub async fn file_sent_copy<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
    ) -> Result<ProviderKey, ApiError> {
        provider
            .file_sent_copy(account, draft)
            .await
            .map_err(|err| map_sync_error(SyncError::Provider(err)))
    }

    /// Applies a [`MailEdit`] to one of the account's messages through the durable
    /// outbox — mark-read/flag (`SetKeywords`), move to another folder
    /// (`MoveTo` — also the mechanism behind a Trash "delete", the host resolving the
    /// Trash mailbox), or permanent delete (`Delete`). The edit is recorded as a
    /// pending op (idempotent by `idempotency`) **before** the provider side effect,
    /// so a crash never loses it (`north-star.md` Write Contract). `idempotency` must
    /// be **unique per edit intent** — deriving it only from the target message would
    /// wrongly collapse mark-read then mark-unread into one op. Returns the resolved
    /// message key and the op id (pollable via [`Engine::pending_op_state`]).
    ///
    /// The next [`Engine::sync_mail`] reconciles the local rows to the new server
    /// state (a periodic snapshot, since IMAP deltas do not carry flag/expunge
    /// changes — `imap-smtp.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the edit fails: the op is first recorded
    /// `Failed` (a stale-target `Conflict` — e.g. an IMAP UID under a changed
    /// `UIDVALIDITY` — means re-sync then retry), and the error then returns. A store
    /// failure also surfaces as [`ApiError::Sync`].
    pub async fn edit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        edit: &MailEdit,
    ) -> Result<MailEditOutcome, ApiError> {
        edit_mail(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            edit,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Reports one of the account's messages to its provider as junk, not junk, or
    /// phishing, through the durable outbox. The message is also **filed** — into
    /// `report.destination`, which the caller resolves (the account's Junk mailbox, or
    /// its Inbox for a not-junk verdict), exactly as it resolves Trash for a delete.
    ///
    /// **Read
    /// [`Capabilities::mail_report`](engine_provider::Capabilities::mail_report) first.**
    /// It is `None` where the provider cannot report at all, and its
    /// [`verdicts`](engine_provider::ReportControls::verdicts) are not universal — Gmail
    /// has no phishing verdict, and an adapter asked for one it lacks **refuses** rather
    /// than filing it as junk. Its
    /// [`evidence`](engine_provider::ReportControls::evidence) says whether the provider
    /// acknowledges the report or merely receives a convention, which is what a caller
    /// needs before telling a user what reporting will achieve.
    ///
    /// `idempotency` must be **unique per report intent** — a key derived only from the
    /// target would collapse a junk report and a later not-junk correction of the same
    /// message into one op.
    ///
    /// The next [`Engine::sync_mail`] reconciles the local rows to the new server state.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the report fails: the op is first recorded `Failed`
    /// (with the failure class), and the error then returns. A store failure also
    /// surfaces as [`ApiError::Sync`].
    pub async fn report_message<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        report: &MessageReport,
    ) -> Result<ReportOutcome, ApiError> {
        report_message(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            report,
        )
        .await
        .map_err(map_sync_error)
    }

    /// The current lifecycle state of a pending outbox op — e.g. the one a
    /// [`submit_mail`](Self::submit_mail) returned — or `None` if no such op exists.
    /// A lease-free read, safe to poll for write progress and confirmation state.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn pending_op_state(
        &self,
        op: PendingOpId,
    ) -> Result<Option<PendingOpState>, ApiError> {
        Ok(self.store.pending_op_state(op).await?)
    }
}
