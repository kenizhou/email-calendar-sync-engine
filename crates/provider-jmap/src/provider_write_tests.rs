//! Provider write tests: submission (context resolve → send, attachment upload,
//! missing upload URL) and mail edits (mark-seen, delete, set-error conflict) —
//! driven offline by the shared `provider_test_support` harness.

use std::sync::Arc;

use serde_json::json;

use super::{provider_test_support::*, *};

#[tokio::test]
async fn submit_email_resolves_context_then_sends() {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::Draft;

    // Two requests: resolve Drafts/Sent + identity, then create + submit.
    let p = provider(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ]);
    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Step 4 submission probe",
        "Hello",
    );
    let receipt = p.submit_email(&account(), &draft).await.unwrap();
    assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
    assert_eq!(
        receipt.message_id.as_str(),
        "step4-send-probe-0002@test.local"
    );
}

#[tokio::test]
async fn submit_email_uploads_attachment_bytes_before_sending() {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::{Draft, DraftAttachment};

    // The upload endpoint hands back a blobId, then the two-step send proceeds. Drive
    // `submit::send` directly so the fake's recorded uploads can be inspected after.
    let exec = FakeExecutor::new(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ])
    .with_upload_blob_ids(["blob-att-1"]);

    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "With attachment",
        "See attached.",
    )
    .with_attachment(DraftAttachment::attachment(
        "report.pdf",
        "application/pdf",
        vec![9, 8, 7],
    ));
    crate::submit::send(&exec, "c", "c", &draft).await.unwrap();

    // The attachment bytes were POSTed to the resolved (account-substituted) upload URL
    // with the right media type — before the Email/set that references the blob.
    let uploads = exec.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "http://127.0.0.1:18080/upload/c/");
    assert_eq!(uploads[0].1, "application/pdf");
    assert_eq!(uploads[0].2, vec![9, 8, 7]);
}

#[tokio::test]
async fn submit_with_attachment_but_no_upload_url_is_a_session_error() {
    use engine_core::{error::FailureClass, ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::{Draft, DraftAttachment, Provider};

    // A server without an uploadUrl cannot take attachments — a clear, permanent error.
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(
        &json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c"
            },
            "apiUrl": "https://mail.test.local/jmap/"
        }),
        vec![],
    )));
    let draft = Draft::new(
        MessageIdHeader::new("m@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "x",
        "y",
    )
    .with_attachment(DraftAttachment::attachment(
        "r.pdf",
        "application/pdf",
        vec![1],
    ));
    let err = p.submit_email(&account(), &draft).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn submit_email_source_uploads_imports_and_submits() {
    // Two requests: resolve Drafts/Sent + identity, then Email/import + submit;
    // the upload in between hands back the blobId the import references.
    let exec = Arc::new(
        FakeExecutor::new(vec![
            fixture("submit_context_response.json"),
            fixture("submit_import_response.json"),
        ])
        .with_upload_blob_ids(["blob-src-1"]),
    );
    let p = JmapProvider::with_executor(Box::new(exec.clone()));

    let source: &[u8] = b"From: Alice <alice@test.local>\r\nTo: bob@test.local\r\n\
                          Message-ID: <src-probe@test.local>\r\nSubject: hi\r\n\r\nbody\r\n";
    let recipients = vec!["bob@test.local".to_owned()];
    let receipt = p
        .submit_email_source(&account(), source, &recipients)
        .await
        .unwrap();
    // The import files the Sent copy itself: same answer as the draft path.
    assert!(receipt.sent_copy.is_filed());
    assert_eq!(receipt.email_key.as_str(), "imp-1");
    assert_eq!(receipt.message_id.as_str(), "src-probe@test.local");

    // The bytes were POSTed verbatim, as message/rfc822, to the resolved
    // (account-substituted) upload URL — before the import that references the blob.
    let uploads = exec.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "http://127.0.0.1:18080/upload/c/");
    assert_eq!(uploads[0].1, "message/rfc822");
    assert_eq!(uploads[0].2, source);
    drop(uploads);

    // Request 2 carries both calls: the import lands the object DIRECTLY in Sent
    // (mailbox id "e" in the context fixture), the submission references it by
    // creation id and spells out the exact envelope.
    let requests = exec.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let calls = requests[1]["methodCalls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0][0], "Email/import");
    assert_eq!(calls[0][1]["emails"]["src"]["blobId"], "blob-src-1");
    assert_eq!(calls[0][1]["emails"]["src"]["mailboxIds"]["e"], json!(true));
    assert_eq!(calls[1][0], "EmailSubmission/set");
    let sub = &calls[1][1]["create"]["sub"];
    assert_eq!(sub["emailId"], "#src");
    assert_eq!(sub["identityId"], "b");
    assert_eq!(sub["envelope"]["mailFrom"]["email"], "alice@test.local");
    assert_eq!(
        sub["envelope"]["rcptTo"],
        json!([{ "email": "bob@test.local" }])
    );
}

#[tokio::test]
async fn submit_email_source_derives_envelope_from_to_cc_bcc_deduped() {
    // Empty recipients → derive: To + Cc + Bcc addr-specs, first occurrence kept,
    // de-duplicated case-insensitively (CAROL@ then carol@ collapse to one).
    let exec = Arc::new(
        FakeExecutor::new(vec![
            fixture("submit_context_response.json"),
            fixture("submit_import_response.json"),
        ])
        .with_upload_blob_ids(["blob-src-1"]),
    );
    let p = JmapProvider::with_executor(Box::new(exec.clone()));

    let source: &[u8] = b"From: alice@test.local\r\n\
                          To: Bob <bob@test.local>, CAROL@test.local\r\n\
                          Cc: carol@test.local\r\n\
                          Bcc: dave@test.local\r\n\
                          Message-ID: <src-derive@test.local>\r\n\r\nbody\r\n";
    p.submit_email_source(&account(), source, &[])
        .await
        .unwrap();

    let requests = exec.requests.lock().unwrap();
    let sub = &requests[1]["methodCalls"][1][1]["create"]["sub"];
    assert_eq!(
        sub["envelope"]["rcptTo"],
        json!([
            { "email": "bob@test.local" },
            { "email": "CAROL@test.local" },
            { "email": "dave@test.local" }
        ])
    );
    assert_eq!(sub["envelope"]["mailFrom"]["email"], "alice@test.local");
}

#[tokio::test]
async fn submit_email_source_refuses_missing_message_id_before_dial() {
    use engine_core::error::FailureClass;

    // No Message-ID → permanent refusal, and NOT A SINGLE REQUEST was sent.
    let (p, exec) = recording(vec![]);
    let source: &[u8] = b"From: alice@test.local\r\nTo: bob@test.local\r\n\r\nbody\r\n";
    let err = p
        .submit_email_source(&account(), source, &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn submit_email_source_refuses_missing_from_before_dial() {
    use engine_core::error::FailureClass;

    // No From address → the envelope MAIL FROM cannot be derived: permanent, pre-dial.
    let (p, exec) = recording(vec![]);
    let source: &[u8] =
        b"To: bob@test.local\r\nMessage-ID: <src-nofrom@test.local>\r\n\r\nbody\r\n";
    let err = p
        .submit_email_source(&account(), source, &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn submit_email_source_refuses_missing_trailing_newline() {
    use engine_core::error::FailureClass;

    // An unterminated last line is refused before any request goes out.
    let (p, exec) = recording(vec![]);
    let source: &[u8] = b"From: alice@test.local\r\nTo: bob@test.local\r\n\
                          Message-ID: <src-nonl@test.local>\r\n\r\nbody";
    let err = p
        .submit_email_source(&account(), source, &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn submit_email_source_refuses_empty_derived_envelope() {
    use engine_core::error::FailureClass;

    // No recipients given and no To/Cc/Bcc to derive from → permanent, pre-dial.
    let (p, exec) = recording(vec![]);
    let source: &[u8] =
        b"From: alice@test.local\r\nMessage-ID: <src-norcpt@test.local>\r\n\r\nbody\r\n";
    let err = p
        .submit_email_source(&account(), source, &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn edit_mail_marks_seen_through_the_real_set_flow() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    // A writable mail account advertises mail writes.
    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "updated": { "eaaaaab": null } }, "0"]]
    })]);
    assert!(p.connection_info().capabilities.mail_writes());

    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .unwrap();
    // The JMAP id is stable across the edit — the receipt echoes it.
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_delete_destroys_via_set() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "destroyed": ["eaaaaab"] }, "0"]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::delete(key.clone()))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_set_error_surfaces_as_a_conflict() {
    use engine_core::{error::FailureClass, ids::ProviderKey};
    use engine_provider::MailEdit;

    // The target was destroyed server-side since it synced: a `notFound` SetError.
    let p = provider(vec![json!({
        "methodResponses": [[
            "Email/set",
            { "notUpdated": { "eaaaaab": { "type": "notFound" } } },
            "0"
        ]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let err = p
        .edit_mail(&account(), &MailEdit::set_flagged(key, true))
        .await
        .unwrap_err();
    // Conflict → the caller re-syncs (tombstoning the gone message), then retries.
    assert_eq!(err.class(), FailureClass::Conflict);
}
