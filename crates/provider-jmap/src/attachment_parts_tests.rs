//! Unit tests for the per-part attachment fetch (`crate::attachment_parts`),
//! driven offline by the shared `provider_test_support` harness: a scripted
//! `Email/get` `bodyStructure` response plus a scripted blob download, with
//! the recorded requests/download URLs as the evidence of which path ran.

use std::sync::Arc;

use engine_core::{
    attachment::{Attachment, AttachmentMeta},
    mail::AttachmentPartId,
};
use serde_json::{Value, json};

use super::{provider_test_support::*, *};
use crate::{JmapError, request::capability};

/// The whole-source fallback's extraction fixture: one `application/pdf`
/// attachment (`report.pdf`, decoded bytes `PDF`) after a text body — the
/// same shape engine-mime's own extractor tests pin.
const MIME_SOURCE: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
    --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
    --m\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
    Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
    Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
    --m--\r\n";

/// A message row whose stored attachment metadata names one pdf part — the
/// tuple the per-part path matches against the `bodyStructure`.
fn message_with_pdf_part() -> engine_core::mail::Message {
    let mut message = message_with_blob("m1", "bMsg");
    message.attachments.push(Attachment::File {
        meta: AttachmentMeta {
            name: Some("report.pdf".to_owned()),
            media_type: Some("application/pdf".to_owned()),
            size: Some(3),
        },
        blob: None,
    });
    message
}

/// An `Email/get` response carrying a `multipart/mixed` bodyStructure whose
/// sub-parts the test supplies (`[text body, supplied parts...]`).
fn body_structure_response(sub_parts: &Value) -> Value {
    json!({
        "methodResponses": [["Email/get", {
            "accountId": "c",
            "state": "s1",
            "list": [{
                "id": "m1",
                "bodyStructure": {
                    "partId": Value::Null,
                    "blobId": Value::Null,
                    "type": "multipart/mixed",
                    "size": 0,
                    "subParts": sub_parts
                }
            }],
            "notFound": []
        }, "0"]],
        "sessionState": "s1"
    })
}

fn text_body_part() -> Value {
    json!({"partId": "1", "blobId": "bTxt", "type": "text/plain", "size": 4, "name": Value::Null})
}

fn pdf_part(blob_id: &str) -> Value {
    json!({
        "partId": "2", "blobId": blob_id, "type": "application/pdf", "size": 3,
        "name": "report.pdf", "disposition": "attachment"
    })
}

/// A provider over a fake the test keeps a handle to, with the blob-download
/// body served — `provider_test_support::recording` without the download half.
fn recording_with_download(
    responses: Vec<Value>,
    body: &[u8],
) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::new(responses).with_download_body(body));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

#[tokio::test]
async fn attachment_fetch_downloads_the_matched_part() {
    let (provider, exec) = recording_with_download(
        vec![body_structure_response(&json!([
            text_body_part(),
            pdf_part("bPdf")
        ]))],
        b"PDF",
    );
    let bytes = provider
        .fetch_attachment_part(
            &account(),
            &message_with_pdf_part(),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap();
    assert_eq!(bytes, b"PDF");
    // Exactly one download, of the matched part's blob — never the whole source.
    let downloads = exec.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains("/bPdf/"), "{}", downloads[0]);
    // The one request was the bodyStructure probe.
    let (using, method, args) = exec.sole_call();
    assert!(using.contains(&capability::MAIL.to_owned()));
    assert_eq!(method, "Email/get");
    assert_eq!(args["ids"], json!(["m1"]));
    assert_eq!(args["properties"], json!(["bodyStructure"]));
}

#[tokio::test]
async fn attachment_fetch_ambiguous_tuple_falls_back_to_whole_source() {
    // Two parts with the SAME (contentType, name, size) tuple: picking one
    // would be a guess, so the whole source must be fetched instead.
    let (provider, exec) = recording_with_download(
        vec![body_structure_response(&json!([
            text_body_part(),
            pdf_part("bPdf1"),
            json!({
                "partId": "3", "blobId": "bPdf2", "type": "application/pdf", "size": 3,
                "name": "report.pdf", "disposition": "attachment"
            }),
        ]))],
        MIME_SOURCE,
    );
    let bytes = provider
        .fetch_attachment_part(
            &account(),
            &message_with_pdf_part(),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap();
    // The whole-source path ran: the bytes came out of the source, and the
    // one download is the message blob, not a part blob.
    assert_eq!(bytes, b"PDF");
    let downloads = exec.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains("/bMsg/"), "{}", downloads[0]);
}

#[tokio::test]
async fn attachment_fetch_no_match_falls_back() {
    // No bodyStructure part carries the stored tuple (the server renamed it).
    let (provider, exec) = recording_with_download(
        vec![body_structure_response(&json!([
            text_body_part(),
            json!({
                "partId": "2", "blobId": "bPdf", "type": "application/pdf", "size": 3,
                "name": "renamed.pdf", "disposition": "attachment"
            }),
        ]))],
        MIME_SOURCE,
    );
    let bytes = provider
        .fetch_attachment_part(
            &account(),
            &message_with_pdf_part(),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap();
    assert_eq!(bytes, b"PDF");
    let downloads = exec.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains("/bMsg/"), "{}", downloads[0]);
}

#[tokio::test]
async fn attachment_fetch_missing_blobid_falls_back() {
    // The matched part carries no blobId — nothing to download per-part.
    let (provider, exec) = recording_with_download(
        vec![body_structure_response(&json!([
            text_body_part(),
            json!({
                "partId": "2", "blobId": Value::Null, "type": "application/pdf", "size": 3,
                "name": "report.pdf", "disposition": "attachment"
            }),
        ]))],
        MIME_SOURCE,
    );
    let bytes = provider
        .fetch_attachment_part(
            &account(),
            &message_with_pdf_part(),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap();
    assert_eq!(bytes, b"PDF");
    let downloads = exec.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains("/bMsg/"), "{}", downloads[0]);
}

#[tokio::test]
async fn attachment_fetch_no_download_url_falls_back() {
    // A session without downloadUrl cannot serve a per-part blob: the
    // condition itself is a fallback, not an error — the whole-source path
    // runs (and its own missing-downloadUrl error is what surfaces).
    let session_doc = json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": { "maxObjectsInGet": 500 },
            "urn:ietf:params:jmap:mail": {}
        },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let exec = Arc::new(FakeExecutor::from_session(&session_doc, Vec::new()));
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let err = provider
        .fetch_attachment_part(
            &account(),
            &message_with_pdf_part(),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, JmapError::Session(_)),
        "the whole-source fallback's missing-downloadUrl error surfaces: {err:?}"
    );
    // The fallback was chosen BEFORE any request or download was attempted.
    assert_eq!(exec.request_count(), 0);
    assert!(exec.download_urls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn attachment_fetch_without_stored_metadata_falls_back() {
    // A JMAP-synced row carries NO stored attachment metadata (Tier-1), so
    // the requested part's tuple is underivable: the whole-source fallback is
    // the answer, with zero probe traffic.
    let (provider, exec) = recording_with_download(Vec::new(), MIME_SOURCE);
    let bytes = provider
        .fetch_attachment_part(
            &account(),
            &message_with_blob("m1", "bMsg"),
            AttachmentPartId::new(0),
        )
        .await
        .unwrap();
    assert_eq!(bytes, b"PDF");
    assert_eq!(exec.request_count(), 0);
    let downloads = exec.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains("/bMsg/"), "{}", downloads[0]);
}

#[tokio::test]
async fn email_body_structure_null_is_an_error_not_a_phantom_part() {
    // An explicit `"bodyStructure": null` must take the documented Protocol
    // error path — flattening Value::Null would otherwise invent a phantom
    // zero-valued part.
    let response = json!({
        "methodResponses": [["Email/get", {
            "accountId": "c",
            "state": "s1",
            "list": [{ "id": "m1", "bodyStructure": Value::Null }],
            "notFound": []
        }, "0"]],
        "sessionState": "s1"
    });
    let (provider, _exec) = recording_with_download(vec![response], b"");
    let err = provider
        .email_body_structure(&account(), "m1")
        .await
        .unwrap_err();
    assert!(
        matches!(err, JmapError::Protocol(_)),
        "null bodyStructure is a protocol error: {err:?}"
    );
}
