//! Envelope derivation for the source-submission path: reading `MAIL FROM` and
//! `RCPT TO` back out of the caller's rendered message bytes.
//!
//! A `submit_email_source` caller hands over final MIME — possibly signed or
//! encrypted — and the bytes are never re-rendered, so everything the
//! submission envelope needs is parsed back out of them. The parsing mirrors
//! `provider-imap`'s `SourceSubmission::parse` (the two transports must derive
//! the same envelope from the same bytes): `MAIL FROM` is the first `From`
//! addr-spec; a derive-mode `RCPT TO` is every `To`/`Cc`/`Bcc` addr-spec,
//! de-duplicated case-insensitively in first-appearance order.

use std::collections::HashSet;

use engine_rfc5322::header_values;

/// The envelope sender: the first `From` addr-spec in the bytes, or `None`
/// when they name no usable address.
pub(crate) fn mail_from(source: &[u8]) -> Option<String> {
    addr_specs(&header_values(source, "From").join(", "))
        .into_iter()
        .next()
}

/// Every envelope recipient the bytes name: `To` + `Cc` + `Bcc` addr-specs,
/// de-duplicated case-insensitively, in first-appearance order. A `Bcc`
/// header left in the bytes is honored here (it travels them verbatim, and is
/// visible in every recipient's copy); Bcc recipients the bytes do not name
/// belong in the caller's explicit `recipients`.
pub(crate) fn derive_recipients(source: &[u8]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    ["To", "Cc", "Bcc"]
        .into_iter()
        .flat_map(|name| header_values(source, name))
        .flat_map(|value| addr_specs(&value))
        .filter(|address| seen.insert(address.to_ascii_lowercase()))
        .collect()
}

/// Splits an address-list header value into its entries: on the commas that
/// **separate** addresses (RFC 5322 §3.4), never on one inside a quoted display name,
/// a `(comment)`, or an angle-addr.
fn split_addresses(value: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    // Bracket depth for `<…>` and paren depth for `(…)`.
    let mut angle = 0usize;
    let mut paren = 0usize;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if in_quote && ch == '\\' {
            current.push(ch);
            escaped = true;
        } else {
            match ch {
                '"' => {
                    in_quote = !in_quote;
                    current.push(ch);
                }
                '<' if !in_quote => {
                    angle += 1;
                    current.push(ch);
                }
                '>' if !in_quote => {
                    angle = angle.saturating_sub(1);
                    current.push(ch);
                }
                '(' if !in_quote => {
                    paren += 1;
                    current.push(ch);
                }
                ')' if !in_quote => {
                    paren = paren.saturating_sub(1);
                    current.push(ch);
                }
                ',' if !in_quote && angle == 0 && paren == 0 => {
                    entries.push(std::mem::take(&mut current));
                }
                _ => current.push(ch),
            }
        }
    }
    entries.push(current);
    entries
}

/// One address-list entry's addr-spec: the content of its angle brackets when it has
/// them, else the bare token (a group's members follow its `:`); `None` for what
/// names no address — a group marker (`undisclosed-recipients:;`), a display name
/// without an address, a bare comment.
fn addr_spec_of(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if let Some(start) = entry.find('<') {
        let rest = &entry[start + 1..];
        let end = rest.find('>')?;
        return valid_addr(rest[..end].trim());
    }
    // No angle-addr: strip a group's label (RFC 5322 §3.4.8 — the members follow the
    // colon) and any trailing group terminator, then what is left must be an addr-spec.
    let members = entry.rsplit_once(':').map_or(entry, |(_, after)| after);
    let bare = members
        .split('(')
        .next()
        .unwrap_or(entry)
        .trim_end_matches(';');
    valid_addr(bare.trim())
}

/// `Some` for a plausible ASCII addr-spec (a `local@domain` with no whitespace or
/// list syntax left in it); `None` for anything else. The addr-spec goes verbatim
/// into the JMAP envelope, so only a clean ASCII token may pass.
fn valid_addr(candidate: &str) -> Option<String> {
    let clean = candidate.is_ascii()
        && candidate.contains('@')
        && !candidate
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'"' | b'(' | b')' | b',' | b';'));
    clean.then(|| candidate.to_owned())
}

/// The addr-specs of an address-list header value, in order.
fn addr_specs(value: &str) -> Vec<String> {
    split_addresses(value)
        .iter()
        .filter_map(|entry| addr_spec_of(entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_from_takes_the_first_from_addr_spec() {
        let source = b"From: Alice <alice@test.local>, ops@test.local\r\n\r\nbody\r\n";
        assert_eq!(mail_from(source).as_deref(), Some("alice@test.local"));
    }

    #[test]
    fn mail_from_is_none_without_a_usable_from() {
        assert_eq!(mail_from(b"To: bob@test.local\r\n\r\nbody\r\n"), None);
        // A display name with no address names nothing.
        assert_eq!(mail_from(b"From: Alice\r\n\r\nbody\r\n"), None);
    }

    #[test]
    fn derive_reads_to_cc_bcc_deduped_case_insensitively() {
        let source = b"From: alice@test.local\r\n\
                       To: Bob <bob@test.local>, CAROL@test.local\r\n\
                       Cc: carol@test.local\r\n\
                       Bcc: dave@test.local\r\n\
                       \r\nbody\r\n";
        assert_eq!(
            derive_recipients(source),
            vec![
                "bob@test.local".to_owned(),
                "CAROL@test.local".to_owned(),
                "dave@test.local".to_owned()
            ]
        );
    }

    #[test]
    fn derive_skips_groups_and_display_names_without_addresses() {
        let source = b"From: alice@test.local\r\n\
                       To: undisclosed-recipients:;, Team: bob@test.local, carol@test.local;\r\n\
                       \r\nbody\r\n";
        assert_eq!(
            derive_recipients(source),
            vec!["bob@test.local".to_owned(), "carol@test.local".to_owned()]
        );
    }

    #[test]
    fn a_quoted_comma_does_not_split_a_display_name() {
        let specs = addr_specs("\"Doe, Jane\" <jane@test.local>, bob@test.local");
        assert_eq!(
            specs,
            vec!["jane@test.local".to_owned(), "bob@test.local".to_owned()]
        );
    }

    #[test]
    fn a_comma_inside_an_angle_addr_never_splits() {
        // The split tracks `<…>` depth, so even malformed-but-bracketed input
        // cannot produce a half-address.
        let specs = addr_specs("<a,b@test.local>");
        assert_eq!(specs, Vec::<String>::new());
    }
}
