//! Tests for reading headers back out of rendered bytes (`parse.rs`), including the
//! emit/read round-trip against the crate's own assembler.

use engine_core::ids::MessageIdHeader;
use engine_provider::Draft;

use super::parse_message_id;
use crate::header_values;

/// A minimal valid message with the given `Message-ID` header line.
fn message_with(message_id_line: &str) -> Vec<u8> {
    let mut source = Vec::new();
    source.extend_from_slice(b"Date: Mon, 27 Aug 2026 10:00:00 +0000\r\n");
    source.extend_from_slice(message_id_line.as_bytes());
    source.extend_from_slice(b"From: alice@test.local\r\nSubject: hi\r\n\r\nbody\r\n");
    source
}

#[test]
fn parses_the_angle_bracketed_message_id() {
    let id = parse_message_id(&message_with("Message-ID: <rendered-1@test.local>\r\n"))
        .expect("a stamped Message-ID parses");
    // The bracket-less form the Draft carries — the same shape the assembler writes.
    assert_eq!(id.as_str(), "rendered-1@test.local");
}

#[test]
fn message_id_lookup_ignores_name_case_and_folds() {
    // The header name in odd case, the value folded across a continuation line —
    // both legal RFC 5322, both must still read back.
    let id = parse_message_id(&message_with("message-id:\r\n\t<folded@test.local>\r\n"))
        .expect("case-insensitive + folded still parses");
    assert_eq!(id.as_str(), "folded@test.local");
}

#[test]
fn a_bare_message_id_value_still_parses() {
    // Some mailers emit the id without brackets; the value is the id.
    let id = parse_message_id(&message_with("Message-ID: bare@test.local\r\n"))
        .expect("a bracket-less value parses");
    assert_eq!(id.as_str(), "bare@test.local");
}

#[test]
fn no_message_id_is_none_not_an_error() {
    assert!(parse_message_id(&message_with("X-Other: x\r\n")).is_none());
    // A body mention is not a header: the block ends at the blank line.
    let body_only = b"From: alice@test.local\r\n\r\nMessage-ID: <in-body@test.local>\r\n";
    assert!(parse_message_id(body_only).is_none());
}

#[test]
fn a_non_ascii_or_oversized_id_is_none() {
    // A lossy-decoded (non-UTF-8) header value contains replacement characters and
    // is not an ASCII msg-id — rejected, never mangled into a receipt.
    let non_ascii = b"Message-ID: <ren\xc3\xa9@bad>\r\n\r\n";
    assert!(parse_message_id(non_ascii).is_none());
    let long = format!("Message-ID: <{}@test.local>\r\n", "a".repeat(1000));
    assert!(parse_message_id(message_with(&long).as_slice()).is_none());
}

#[test]
fn the_assembler_emits_what_the_parser_reads_back() {
    // The emit/read pair lives in one crate so it cannot drift: assemble a draft,
    // read the id back, and it is the draft's own pre-generated Message-ID.
    let draft = Draft::new(
        MessageIdHeader::new("round-trip@test.local").unwrap(),
        engine_core::mail::EmailAddress::new("alice@test.local"),
        vec![engine_core::mail::EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    );
    let source = crate::assemble_message(&draft, time::macros::datetime!(2026-08-27 10:00:00 UTC))
        .expect("assembles");
    assert_eq!(
        parse_message_id(&source).map(|id| id.as_str().to_owned()),
        Some("round-trip@test.local".to_owned())
    );
}

#[test]
fn header_values_collects_every_occurrence_case_insensitively() {
    let source = b"To: one@test.local\r\nto: two@test.local\r\n\r\nbody";
    assert_eq!(
        header_values(source, "TO"),
        vec!["one@test.local".to_owned(), "two@test.local".to_owned()]
    );
    assert_eq!(header_values(source, "Cc"), Vec::<String>::new());
}

#[test]
fn header_values_never_read_past_the_blank_line() {
    let source = b"From: alice@test.local\r\n\r\nFrom: in-body@test.local";
    assert_eq!(
        header_values(source, "From"),
        vec!["alice@test.local".to_owned()]
    );
}
