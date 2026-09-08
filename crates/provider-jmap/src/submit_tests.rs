//! Unit tests for the submission builders and receipt parsing (`crate::submit`),
//! driven by the captured Stalwart fixtures.

use engine_core::error::FailureClass;

use super::*;

fn send_response() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/submit_send_response.json")).unwrap()
}

fn results(doc: &Value) -> (Value, Value) {
    // methodResponses: [Email/set "0", EmailSubmission/set "1", implicit Email/set "1"]
    let responses = doc["methodResponses"].as_array().unwrap();
    (responses[0][1].clone(), responses[1][1].clone())
}

fn message_id() -> MessageIdHeader {
    MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap()
}

#[test]
fn parses_the_sent_email_key_and_echoes_message_id() {
    let doc = send_response();
    let (email, submission) = results(&doc);
    let receipt = parse_receipt(&email, &submission, &message_id()).unwrap();
    // The created email id (kept across the Drafts→Sent move) is the resolved key.
    assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
    assert_eq!(receipt.message_id, message_id());
}

#[test]
fn email_set_error_classifies_and_aborts() {
    let email = json!({
        "notCreated": { "draft": { "type": "invalidProperties", "properties": ["from"] } }
    });
    let submission = json!({ "created": { "sub": { "id": "x" } } });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[test]
fn submission_error_classifies_after_email_created() {
    // The observed Stalwart failure when identityId is missing.
    let email = json!({ "created": { "draft": { "id": "e1" } } });
    let submission = json!({
        "notCreated": { "sub": { "type": "invalidProperties", "properties": ["identityId"] } }
    });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[test]
fn rate_limited_submission_is_retryable() {
    let email = json!({ "created": { "draft": { "id": "e1" } } });
    let submission = json!({ "notCreated": { "sub": { "type": "rateLimit" } } });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert!(err.failure_class().is_retryable());
}

#[test]
fn build_draft_targets_drafts_and_carries_message_id() {
    let context = SubmitContext {
        drafts: "d".to_owned(),
        sent: "e".to_owned(),
        identity: "b".to_owned(),
    };
    let draft = Draft::new(
        message_id(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    );
    let create = build_draft(&context, &draft, &[]);
    assert_eq!(create["mailboxIds"]["d"], json!(true));
    assert_eq!(create["keywords"]["$draft"], json!(true));
    assert_eq!(create["messageId"][0], "step4-send-probe-0002@test.local");
    assert_eq!(create["from"][0]["email"], "alice@test.local");

    let (submission, on_success) = build_submission(&context, &draft);
    assert_eq!(submission["emailId"], "#draft");
    assert_eq!(submission["identityId"], "b");
    // onSuccessUpdateEmail moves Drafts→Sent and clears $draft.
    assert_eq!(on_success["#sub"]["mailboxIds/d"], Value::Null);
    assert_eq!(on_success["#sub"]["mailboxIds/e"], json!(true));
    assert_eq!(on_success["#sub"]["keywords/$draft"], Value::Null);
}

#[test]
fn build_draft_carries_html_as_alternative_body() {
    let context = SubmitContext {
        drafts: "d".to_owned(),
        sent: "e".to_owned(),
        identity: "b".to_owned(),
    };
    let draft = Draft::new(
        message_id(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Plain",
        "Plain",
    )
    .with_html_body("<p>Plain</p>");

    let create = build_draft(&context, &draft, &[]);

    assert_eq!(create["bodyStructure"]["type"], "multipart/alternative");
    assert_eq!(create["bodyStructure"]["subParts"][0]["partId"], "text");
    assert_eq!(create["bodyStructure"]["subParts"][1]["partId"], "html");
    assert_eq!(create["bodyValues"]["text"]["value"], "Plain");
    assert_eq!(create["bodyValues"]["html"]["value"], "<p>Plain</p>");
}

#[test]
fn source_receipt_takes_the_import_key_and_echoes_message_id() {
    let import = json!({ "created": { "src": { "id": "imp-1" } } });
    let submission = json!({ "created": { "sub": { "id": "sub-1" } } });
    let receipt = parse_source_receipt(&import, &submission, message_id()).unwrap();
    assert_eq!(receipt.email_key.as_str(), "imp-1");
    assert_eq!(receipt.message_id, message_id());
    assert!(receipt.sent_copy.is_filed());
}

#[test]
fn source_import_error_classifies_and_aborts() {
    let import = json!({
        "notCreated": { "src": { "type": "invalidProperties", "properties": ["blobId"] } }
    });
    let submission = json!({ "created": { "sub": { "id": "sub-1" } } });
    let err = parse_source_receipt(&import, &submission, message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[test]
fn source_submission_error_classifies_after_import_created() {
    let import = json!({ "created": { "src": { "id": "imp-1" } } });
    let submission = json!({
        "notCreated": { "sub": { "type": "invalidProperties", "properties": ["identityId"] } }
    });
    let err = parse_source_receipt(&import, &submission, message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}
