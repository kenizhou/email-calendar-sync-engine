//! Reading submission-relevant headers back out of rendered message bytes.
//!
//! The crate's other half assembles a [`Draft`] into bytes ([`assemble_message`]);
//! this module is the inverse seam for the **already-rendered** path: a caller that
//! submits its own final MIME (e.g. signed/encrypted over a rendered draft) still
//! owes the engine the message's `Message-ID` — the receipt reconciles the sent copy
//! by it — and that id lives in the bytes, not in any structured field. Reading it
//! out belongs here, beside the code that writes it in, so the emit/read pair cannot
//! drift apart (`submit_email_source`).
//!
//! [`assemble_message`]: crate::assemble_message
//! [`submit_email_source`]: engine_provider::Provider::submit_email_source

use engine_core::ids::MessageIdHeader;

/// Parses the message's own `Message-ID` header (first occurrence) out of `source`:
/// the angle-bracketed id without its brackets, the JMAP `MessageIds` form — the same
/// shape [`assemble_message`](crate::assemble_message) writes and
/// [`Draft::message_id`](engine_provider::Draft::message_id) carries.
///
/// `None` when the message carries no `Message-ID` (or one too long or not ASCII to
/// be a [`MessageIdHeader`]) — a message the caller must stamp before submitting
/// through the source seam, per the Write Contract.
#[must_use]
pub fn parse_message_id(source: &[u8]) -> Option<MessageIdHeader> {
    let value = header_values(source, "Message-ID").into_iter().next()?;
    // The conventional `<id@host>` form: the first angle-bracketed id. A bare value
    // (no brackets) is taken as-is; both are trimmed, because a folded or padded
    // header value must still reconcile.
    let id = match (value.find('<'), value.find('>')) {
        (Some(start), Some(end)) if end > start => value[start + 1..end].trim(),
        _ => value.trim(),
    };
    if id.is_empty() || !id.is_ascii() {
        return None;
    }
    MessageIdHeader::new(id).ok()
}

/// Every value the named header carries in `source`'s header block, case-insensitive
/// by name (RFC 5322 §2.2 says field names are ASCII case-insensitive) and **unfolded**
/// (a continuation line joins the value it continues, per §2.2.3). Body bytes are
/// never touched: the header block ends at the first blank line.
///
/// Values are decoded lossily: mail is hostile input, and a non-UTF-8 value yields
/// replacement characters rather than a panic — a caller that needs strict bytes
/// (the envelope, an addr-spec) rejects what it cannot use.
#[must_use]
pub fn header_values(source: &[u8], name: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (field, value) in headers(source) {
        if field.eq_ignore_ascii_case(name) {
            values.push(value);
        }
    }
    values
}

/// The logical (name, value) header fields of `source`: header-block lines joined
/// across folds, split at the first colon, both sides trimmed. A line with no colon
/// is malformed mail and contributes nothing.
fn headers(source: &[u8]) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in header_lines(header_block(source)) {
        // A continuation (leading space/tab) appends to the previous field, not a
        // new one — RFC 5322 §2.2.3 folding.
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    fields
}

/// The header block: everything before the first blank line (a `CRLF CRLF` or a bare
/// `LF LF`, whichever comes first). A message with no blank line is all headers —
/// reading on costs nothing and a headers-only message still parses.
fn header_block(source: &[u8]) -> &[u8] {
    let lf = source
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|at| at + 1);
    let crlf = source
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 2);
    let end = match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (found, None) | (None, found) => found,
    };
    end.map_or(source, |end| &source[..end])
}

/// Splits `block` into lines on `\n` (stripping the `\r` of a CRLF), lossily decoded.
fn header_lines(block: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(block)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
