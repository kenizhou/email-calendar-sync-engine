// SPDX-License-Identifier: MPL-2.0
//! Unit tests for the submission slice (`submit.rs`) — the `#[path]` split
//! the repo uses to hold the 500-line cap (the `email_tests.rs` precedent).
//! The transport-level proofs (OPAQUE byte-exactness, SaveInSentItems,
//! status classification) live in the harness `adapter_submit_flow`
//! scenarios; these pin the pure derivation and validation helpers.

use super::*;

/// The FNV-1a 64 reference vector — the digest the ClientId rides on.
#[test]
fn fnv1a64_matches_the_reference_vector() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

/// The ClientId derivation: deterministic per Message-ID, normalized
/// against brackets/case/whitespace, always under the enforced 40-char cap.
#[test]
fn client_ids_derive_deterministically_under_the_cap() {
    let a = send_client_id("eas-send-0001@test.local");
    let b = send_client_id("eas-send-0001@test.local");
    assert_eq!(a, b, "the same Message-ID derives the same ClientId");
    assert_eq!(a.len(), 18, "SM + 16 hex chars");
    assert!(a.len() <= crate::types::CLIENT_ID_MAX_LEN);
    assert_eq!(
        send_client_id("<EAS-SEND-0001@Test.Local>"),
        a,
        "brackets, case, and whitespace normalize away — a folded id derives \
         the same id"
    );
    assert_ne!(
        send_client_id("other@test.local"),
        a,
        "different messages dedup independently"
    );
    let empty = send_client_id("   ");
    assert!(
        empty.starts_with("SM") && empty != a,
        "an unusable id falls back to the minted uuid form"
    );
}

/// The placeholder key the receipt carries — Graph/IMAP's no-id shape.
#[test]
fn placeholder_keys_carry_the_message_id() {
    let id = MessageIdHeader::new("x-1@test.local").unwrap();
    assert_eq!(placeholder_key(&id).as_str(), "sent:x-1@test.local");
}

/// The addr-spec extractor: bracketed forms keep the inner spec, bare
/// tokens are their own.
#[test]
fn addr_specs_survive_display_names() {
    assert_eq!(addr_spec_of("bob@example.net"), "bob@example.net");
    assert_eq!(
        addr_spec_of("Bob Q. Public <bob@example.net>"),
        "bob@example.net"
    );
    assert_eq!(addr_spec_of("  <bob@example.net> "), "bob@example.net");
}

/// The envelope rule's set comparison: the header set must equal the list
/// exactly — an extra or missing recipient is a mismatch, order is not.
#[test]
fn header_sets_compare_order_insensitively_but_exactly() {
    let source = b"From: a@example.test\r\nTo: b@example.net, c@example.net\r\n\
                   Cc: d@example.org\r\nBcc: e@example.org\r\n\r\nbody\r\n";
    let list = |v: &[&str]| {
        v.iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        header_addr_specs(source),
        Some(
            [
                "b@example.net",
                "c@example.net",
                "d@example.org",
                "e@example.org"
            ]
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
        ),
        "the full header set, order-insensitive"
    );
    assert_eq!(
        header_addr_specs(source),
        Some(addr_spec_set(&list(&[
            "e@example.org",
            "d@example.org",
            "c@example.net",
            "b@example.net"
        ]))),
        "the same set in any order matches"
    );
    assert_ne!(
        header_addr_specs(source),
        Some(addr_spec_set(&list(&[
            "b@example.net",
            "c@example.net",
            "d@example.org"
        ]))),
        "a stripped Bcc in the list is a mismatch — those recipients would not deliver"
    );
    assert_ne!(
        header_addr_specs(source),
        Some(addr_spec_set(&[
            "b@example.net".to_owned(),
            "c@example.net".to_owned(),
            "d@example.org".to_owned(),
            "e@example.org".to_owned(),
            "extra@example.net".to_owned(),
        ])),
        "an envelope-only recipient is a mismatch"
    );
}

/// The conservative direction: a quoted display name defeats confident
/// splitting, so the headers answer "cannot compare" — the caller refuses,
/// never mis-delivers.
#[test]
fn quoted_display_names_refuse_comparison_rather_than_mis_split() {
    let source = b"From: a@example.test\r\nTo: \"Public, Bob\" <bob@example.net>\r\n\r\nx\r\n";
    assert_eq!(
        header_addr_specs(source),
        None,
        "a comma may hide inside the quotes — no confident split"
    );
}

/// The trailing-terminator tolerance: a body must end in a line feed; the
/// CRLF form is the wire shape, a bare LF still terminates.
#[test]
fn unterminated_bodies_are_detected() {
    let crlf = b"From: a@example.test\r\n\r\nbody\r\n";
    let lf = b"From: a@example.test\r\n\r\nbody\n";
    let open = b"From: a@example.test\r\n\r\nbody";
    assert!(crlf.ends_with(b"\n"));
    assert!(lf.ends_with(b"\n"));
    assert!(!open.ends_with(b"\n"));
}
