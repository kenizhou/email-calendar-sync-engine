//! JMAP mail submission: `Email/set` draft creation + `EmailSubmission/set` with
//! `onSuccessUpdateEmail` (RFC 8621 §7).
//!
//! A send is the canonical two-step JMAP flow. First a "resolve context" request
//! reads the account's Drafts/Sent mailbox ids and submission identity (their ids
//! are server-assigned and cannot be templated into a `mailboxIds` map key, so
//! they must be known as literals first). Then one request creates the draft, then
//! submits it referencing the just-created email by creation id (`#draft`), and
//! files it via `onSuccessUpdateEmail` (move Drafts→Sent, clear `$draft`).
//!
//! This is only the provider side effect; durability and idempotency are the
//! caller's outbox (`engine-sync`). The pre-generated `Message-ID` is echoed in the
//! receipt so the sent copy reconciles when it syncs back (`store-and-sync.md`).

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Mailbox, MailboxRole},
};
use engine_provider::{Draft, SubmissionReceipt};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    mail::mailbox_from_json,
    request::{Request, capability},
    source_envelope,
    submit_body::body,
    sync_ops::objects,
};

/// The server-assigned ids a submission needs as literals.
struct SubmitContext {
    drafts: String,
    sent: String,
    identity: String,
}

/// Sends `draft`: resolves context, uploads any attachment blobs, then creates +
/// submits + files it.
pub(crate) async fn send(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    draft: &Draft,
) -> Result<SubmissionReceipt, JmapError> {
    let context = resolve_context(executor, mail_account, submission_account).await?;
    // Attachment bytes must be uploaded first: the draft references each by the
    // server-assigned `blobId` (RFC 8620 §6.1), which can only be known after upload.
    let blob_ids = upload_attachments(executor, mail_account, draft).await?;

    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mut email_create = Map::new();
    email_create.insert("draft".to_owned(), build_draft(&context, draft, &blob_ids));
    let email_set = req.invoke(
        "Email/set",
        json!({ "accountId": mail_account, "create": email_create }),
    );
    let (submission_create, on_success) = build_submission(&context, draft);
    let mut submission_map = Map::new();
    submission_map.insert("sub".to_owned(), submission_create);
    let submission_set = req.invoke(
        "EmailSubmission/set",
        json!({
            "accountId": submission_account,
            "create": submission_map,
            "onSuccessUpdateEmail": on_success,
        }),
    );

    let resp = executor.execute(&req).await?;
    parse_receipt(
        resp.result(&email_set)?,
        resp.result(&submission_set)?,
        &draft.message_id,
    )
}

/// Uploads every draft attachment's bytes, returning the `blobId`s in attachment
/// order (RFC 8620 §6.1). A no-op for an attachment-free draft.
///
/// # Errors
///
/// [`JmapError::Session`] if the draft has attachments but the server advertised no
/// `uploadUrl`, or the classified failure of an upload.
async fn upload_attachments(
    executor: &dyn Executor,
    mail_account: &str,
    draft: &Draft,
) -> Result<Vec<String>, JmapError> {
    if draft.attachments.is_empty() {
        return Ok(Vec::new());
    }
    let url = executor
        .session()
        .upload_url()
        .ok_or_else(|| JmapError::session("server advertised no uploadUrl; cannot attach"))?
        .replace("{accountId}", mail_account);
    let mut blob_ids = Vec::with_capacity(draft.attachments.len());
    for attachment in &draft.attachments {
        blob_ids.push(
            executor
                .upload(&url, &attachment.media_type, &attachment.content)
                .await?,
        );
    }
    Ok(blob_ids)
}

/// Reads the Drafts/Sent mailbox ids and the submission identity id in one request.
async fn resolve_context(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
) -> Result<SubmitContext, JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mailboxes = req.invoke("Mailbox/get", json!({ "accountId": mail_account }));
    let identities = req.invoke("Identity/get", json!({ "accountId": submission_account }));
    let resp = executor.execute(&req).await?;

    let mailbox_list = objects(resp.result(&mailboxes)?, mailbox_from_json)?;
    Ok(SubmitContext {
        drafts: role_id(&mailbox_list, &MailboxRole::Drafts)?,
        sent: role_id(&mailbox_list, &MailboxRole::Sent)?,
        identity: first_identity(resp.result(&identities)?)?,
    })
}

/// Finds the id of the mailbox with `role`.
fn role_id(mailboxes: &[Mailbox], role: &MailboxRole) -> Result<String, JmapError> {
    mailboxes
        .iter()
        .find(|m| m.role.as_ref() == Some(role))
        .map(|m| m.id.as_str().to_owned())
        .ok_or_else(|| JmapError::session(format!("account has no {role} mailbox")))
}

/// The first identity id (the default From identity).
fn first_identity(result: &Value) -> Result<String, JmapError> {
    result
        .get("list")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|identity| identity.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| JmapError::session("account has no submission identity"))
}

/// Builds the `Email/set` create object for the draft, referencing the uploaded
/// attachment `blob_ids` (one per `draft.attachments`, in order).
fn build_draft(context: &SubmitContext, draft: &Draft, blob_ids: &[String]) -> Value {
    let mut mailbox_ids = Map::new();
    mailbox_ids.insert(context.drafts.clone(), Value::Bool(true));
    let (body_structure, body_values) = body(draft, blob_ids);
    json!({
        "mailboxIds": mailbox_ids,
        "keywords": { "$draft": true, "$seen": true },
        "from": [address(&draft.from)],
        "to": draft.to.iter().map(address).collect::<Vec<_>>(),
        "subject": draft.subject,
        "messageId": [draft.message_id.as_str()],
        "bodyStructure": body_structure,
        "bodyValues": body_values,
    })
}

/// Builds the `EmailSubmission/set` create object and the `onSuccessUpdateEmail`
/// patch that files the sent copy.
fn build_submission(context: &SubmitContext, draft: &Draft) -> (Value, Value) {
    let create = json!({
        "emailId": "#draft",
        "identityId": context.identity,
        "envelope": {
            "mailFrom": { "email": draft.from.email },
            "rcptTo": draft.to.iter().map(|a| json!({ "email": a.email })).collect::<Vec<_>>(),
        },
    });
    let mut patch = Map::new();
    patch.insert(format!("mailboxIds/{}", context.drafts), Value::Null);
    patch.insert(format!("mailboxIds/{}", context.sent), Value::Bool(true));
    patch.insert("keywords/$draft".to_owned(), Value::Null);
    let mut on_success = Map::new();
    on_success.insert("#sub".to_owned(), Value::Object(patch));
    (create, Value::Object(on_success))
}

/// A JMAP `EmailAddress` object, omitting a null display name.
fn address(addr: &EmailAddress) -> Value {
    match &addr.name {
        Some(name) => json!({ "name": name, "email": addr.email }),
        None => json!({ "email": addr.email }),
    }
}

/// Extracts the sent email's key, mapping a `SetError` on either create into a
/// classified [`JmapError`].
fn parse_receipt(
    email_result: &Value,
    submission_result: &Value,
    message_id: &MessageIdHeader,
) -> Result<SubmissionReceipt, JmapError> {
    let email_id = created_id(email_result, "draft")
        .ok_or_else(|| set_error(email_result, "draft", "Email/set"))?;
    if created_id(submission_result, "sub").is_none() {
        return Err(set_error(submission_result, "sub", "EmailSubmission/set"));
    }
    let key = ProviderKey::new(email_id)
        .map_err(|e| JmapError::protocol(format!("bad created email id: {e}")))?;
    // Filing is the server's own `onSuccessUpdateEmail`, in the same request that submitted
    // — so a submission that succeeded filed the copy. The one shape this does not cover:
    // the implicit `Email/set` can report the move `notUpdated` on its own, and the copy
    // then stays in Drafts. That response is a third entry sharing the submission's call id
    // and is not read here, so it would pass as `Filed`. Unlike a lost IMAP `APPEND` the
    // message is still in the account and still syncs, which is why it has not forced the
    // extra plumbing.
    Ok(SubmissionReceipt::filed(key, message_id.clone()))
}

/// The id of an object created under `creation_id`, if the create succeeded.
fn created_id<'a>(result: &'a Value, creation_id: &str) -> Option<&'a str> {
    result
        .get("created")
        .and_then(|created| created.get(creation_id))
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
}

/// Turns a `notCreated` `SetError` (RFC 8620 §5.3) into a classified method error.
fn set_error(result: &Value, creation_id: &str, method: &str) -> JmapError {
    let error_type = result
        .get("notCreated")
        .and_then(|nc| nc.get(creation_id))
        .and_then(|err| err.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    JmapError::Method {
        call_id: method.to_owned(),
        error_type,
    }
}

/// Sends caller-rendered `source` bytes **verbatim** — never re-rendered (the bytes
/// may already be signed or encrypted) — as one `Email/import` + `EmailSubmission/set`
/// pair (RFC 8621 §4.10, §7). Where the draft path re-renders structured fields, this
/// path ships the bytes themselves: they are uploaded as a single `message/rfc822`
/// blob, imported DIRECTLY into Sent (the provider files its own copy, so the host
/// must not file a second one), and submitted by creation-id reference (`#src`).
///
/// Everything the wire needs is read out of the bytes BEFORE the first request —
/// the `Message-ID` (the receipt echoes it; the sent copy reconciles by it), the
/// trailing line terminator, the envelope `MAIL FROM` (the first `From` addr-spec),
/// and the `RCPT TO` set: `recipients` verbatim when non-empty, else derived from
/// the bytes' own `To`/`Cc`/`Bcc` ([`crate::source_envelope`]). Bytes this seam
/// cannot send are refused with no request ever sent.
///
/// # Errors
///
/// A permanent-classified [`JmapError`] for unsendable bytes (no `Message-ID` or
/// `From`, no trailing line terminator, no envelope recipient);
/// [`JmapError::Session`] for a missing Sent mailbox, identity, or `uploadUrl`; the
/// classified failure of either method call otherwise.
pub(crate) async fn send_source(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    source: &[u8],
    recipients: &[String],
) -> Result<SubmissionReceipt, JmapError> {
    let Some(message_id) = engine_rfc5322::parse_message_id(source) else {
        return Err(JmapError::protocol(
            "the submitted bytes carry no Message-ID; the caller must stamp one \
             before submitting (the sent copy reconciles by it)",
        ));
    };
    if !source.ends_with(b"\n") {
        return Err(JmapError::protocol(
            "the submitted bytes do not end in a line terminator",
        ));
    }
    let mail_from = source_envelope::mail_from(source).ok_or_else(|| {
        JmapError::protocol(
            "the submitted bytes carry no From address; the envelope sender cannot \
             be derived from them",
        )
    })?;
    let rcpt_to = if recipients.is_empty() {
        source_envelope::derive_recipients(source)
    } else {
        recipients.to_vec()
    };
    if rcpt_to.is_empty() {
        return Err(JmapError::protocol(
            "the submission names no envelope recipient: `recipients` is empty and \
             the bytes carry no To, Cc or Bcc address",
        ));
    }

    let context = resolve_context(executor, mail_account, submission_account).await?;
    let blob_id = upload_source(executor, mail_account, source).await?;

    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mut mailbox_ids = Map::new();
    mailbox_ids.insert(context.sent.clone(), Value::Bool(true));
    let mut import_create = Map::new();
    import_create.insert(
        "src".to_owned(),
        json!({ "blobId": blob_id, "mailboxIds": Value::Object(mailbox_ids) }),
    );
    let import = req.invoke(
        "Email/import",
        json!({ "accountId": mail_account, "emails": import_create }),
    );
    let mut submission_create = Map::new();
    submission_create.insert(
        "sub".to_owned(),
        json!({
            "emailId": "#src",
            "identityId": context.identity,
            "envelope": {
                "mailFrom": { "email": mail_from },
                "rcptTo": rcpt_to.iter().map(|email| json!({ "email": email })).collect::<Vec<_>>(),
            },
        }),
    );
    let submission = req.invoke(
        "EmailSubmission/set",
        json!({ "accountId": submission_account, "create": submission_create }),
    );

    let resp = executor.execute(&req).await?;
    parse_source_receipt(resp.result(&import)?, resp.result(&submission)?, message_id)
}

/// Uploads the message bytes as one blob, returning the server-assigned `blobId`
/// (RFC 8620 §6.1) the `Email/import` then references.
///
/// # Errors
///
/// [`JmapError::Session`] if the server advertised no `uploadUrl`, or the
/// classified failure of the upload.
async fn upload_source(
    executor: &dyn Executor,
    mail_account: &str,
    source: &[u8],
) -> Result<String, JmapError> {
    let url = executor
        .session()
        .upload_url()
        .ok_or_else(|| JmapError::session("server advertised no uploadUrl; cannot import"))?
        .replace("{accountId}", mail_account);
    executor.upload(&url, "message/rfc822", source).await
}

/// Extracts the imported email's key, mapping a `SetError` on either create into a
/// classified [`JmapError`].
fn parse_source_receipt(
    import_result: &Value,
    submission_result: &Value,
    message_id: MessageIdHeader,
) -> Result<SubmissionReceipt, JmapError> {
    let email_id = created_id(import_result, "src")
        .ok_or_else(|| set_error(import_result, "src", "Email/import"))?;
    if created_id(submission_result, "sub").is_none() {
        return Err(set_error(submission_result, "sub", "EmailSubmission/set"));
    }
    let key = ProviderKey::new(email_id)
        .map_err(|e| JmapError::protocol(format!("bad created email id: {e}")))?;
    // The import landed the object directly in Sent, so a submission that succeeded
    // filed the copy — the draft path's `onSuccessUpdateEmail` answer, with no
    // implicit update to read back and no second copy for the host to file.
    Ok(SubmissionReceipt::filed(key, message_id))
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
