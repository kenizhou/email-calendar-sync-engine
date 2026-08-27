//! Source submission: sending the caller's already-rendered message bytes.
//!
//! The sibling of [`crate::filing`]'s draft path, reached through the
//! [`submit_email_source`](engine_provider::Provider::submit_email_source) verb:
//! where that path **assembles** a [`Draft`](engine_provider::Draft) into message
//! bytes, this one submits bytes the caller rendered — and possibly signed or
//! encrypted — **verbatim**. Two invariants follow, and both are what the seam
//! exists for:
//!
//! - the bytes go into SMTP `DATA` exactly as given (the dot-stuffing [`crate::smtp`] applies is
//!   the wire's own framing and round-trips invisibly);
//! - the Sent copy is an `APPEND` of **the same bytes** — no
//!   [`assemble_filed_message`](engine_rfc5322::assemble_filed_message), so the wire copy and the
//!   filed copy are one and the same, and what the recipients got is what the sender keeps.
//!
//! The receipt's `Message-ID` and (in derive mode) the envelope are read back
//! **out of the bytes** (via `engine-rfc5322`'s parse side): a caller that submits
//! final MIME supplies its headers with it. The envelope's `RCPT TO` set may also
//! be given **explicitly** — the exact list, which is where Bcc travels without
//! ever entering the bytes. Delivery classification, Sent-placement retry, and the
//! `Unfiled` receipt semantics are the draft path's, shared through
//! [`crate::filing`].

use std::collections::HashSet;

use engine_core::ids::MessageIdHeader;
use engine_provider::{ProviderError, ProviderResult, SubmissionReceipt};
use engine_rfc5322::{header_values, parse_message_id};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

use crate::{
    error::ImapError,
    filing::{SmtpSender, ensure_delivered, tls_connect},
    provider::ImapProvider,
    smtp,
};

/// Everything derived from the caller's rendered bytes once, shared by the wire send
/// and the receipt — the source-path counterpart of `filing`'s `Submission`. Where
/// that struct assembles a message from a draft, this one reads the submission facts
/// back out of the bytes, because the bytes **are** the message and are never
/// re-rendered.
pub(crate) struct SourceSubmission {
    /// The bytes' own `Message-ID`, read back for the receipt: the sent copy
    /// reconciles by it on a later sync, exactly as a draft's pre-generated id does.
    message_id: MessageIdHeader,
    /// Envelope `MAIL FROM` address (the first `From` addr-spec).
    from: String,
    /// De-duplicated envelope `RCPT TO` list (To + Cc + Bcc addr-specs).
    to: Vec<String>,
    /// The `EHLO` identity (the sender's domain).
    ehlo: String,
}

impl SourceSubmission {
    /// Derives the submission from the rendered `source` bytes and the caller's
    /// envelope: their own `Message-ID`, `MAIL FROM` (the first `From` addr-spec),
    /// and the `RCPT TO` list — `recipients` verbatim when non-empty (the exact
    /// set, where Bcc lives without ever entering the bytes), else the bytes' own
    /// `To`/`Cc` headers (a `Bcc` header left in the bytes is honored and travels
    /// them verbatim). Parsing happens **before any dial**, so bytes this seam
    /// cannot send are refused without a connection ever being opened.
    ///
    /// # Errors
    ///
    /// [`ProviderError::permanent`] — a property of the bytes and envelope, never
    /// of the transport, so retrying unchanged can never succeed: no `Message-ID`
    /// (the caller must stamp one before submitting, the Write Contract); no `From`
    /// address to derive `MAIL FROM` from; bytes not ending in a line terminator
    /// (SMTP's `DATA` terminator would corrupt the last line — the bytes must end
    /// `\n`-terminated as RFC 5322 produces them); or no envelope recipient at all.
    fn parse(source: &[u8], recipients: &[String]) -> ProviderResult<Self> {
        let Some(message_id) = parse_message_id(source) else {
            return Err(ProviderError::permanent(
                "the submitted bytes carry no Message-ID; the caller must stamp one \
                 before submitting them",
            ));
        };
        // A message whose last line is unterminated would have the `DATA` terminator
        // merged into it on the wire (the crate's dot-stuffing splits on lines), so
        // the byte-equality contract itself rules the bytes out.
        if !source.ends_with(b"\n") {
            return Err(ProviderError::permanent(
                "the submitted bytes do not end in a line terminator; SMTP's DATA \
                 terminator would corrupt the last line",
            ));
        }
        let from = addr_specs(&header_values(source, "From").join(", "))
            .into_iter()
            .next()
            .ok_or_else(|| {
                ProviderError::permanent(
                    "the submitted bytes carry no From address; the SMTP envelope sender \
                     cannot be derived from them",
                )
            })?;
        let to = if recipients.is_empty() {
            // Derive mode: every envelope recipient the bytes name gets a `RCPT TO` —
            // To + Cc + Bcc, de-duplicated case-insensitively, exactly as the draft
            // path does. A `Bcc` header inside the bytes travels them verbatim (and
            // is visible in every recipient's copy); Bcc recipients the bytes do not
            // name belong in `recipients`.
            let mut seen: HashSet<String> = HashSet::new();
            ["To", "Cc", "Bcc"]
                .into_iter()
                .flat_map(|name| header_values(source, name))
                .flat_map(|value| addr_specs(&value))
                .filter(|address| seen.insert(address.to_ascii_lowercase()))
                .collect()
        } else {
            // Exact mode: the caller composed the envelope (To + Cc + Bcc), so it is
            // used verbatim — same trust as a draft's recipient list (the wire
            // screen in `crate::smtp` still rejects control characters in it).
            recipients.to_vec()
        };
        if to.is_empty() {
            return Err(ProviderError::permanent(
                "the submission names no envelope recipient: `recipients` is empty and \
                 the bytes carry no To or Cc address",
            ));
        }
        let ehlo = from
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
            .to_owned();
        Ok(Self {
            message_id,
            from,
            to,
            ehlo,
        })
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImapProvider<S> {
    /// Submits the caller's rendered `source` bytes over the configured SMTP
    /// transport, opening a fresh connection per send, exactly as the draft path
    /// does: plaintext and implicit TLS transmit directly; STARTTLS negotiates the
    /// cleartext upgrade, TLS-wraps the socket, then transmits over TLS, with
    /// `AUTH PLAIN` only ever on a secured stream. `recipients` is the envelope:
    /// the exact `RCPT TO` set when non-empty, else derived from the bytes' own
    /// `To`/`Cc` headers ([`SourceSubmission::parse`]).
    ///
    /// # Errors
    ///
    /// [`ProviderError::invalid_state`] when no SMTP transport is configured;
    /// [`ProviderError::permanent`] for bytes this seam cannot send (no
    /// `Message-ID`, no `From`, no trailing line terminator, no envelope
    /// recipient); the same classified failures as the draft path for a send that
    /// happens (a post-`DATA` ambiguity is never blind-retried).
    pub(crate) async fn submit_source(
        &self,
        source: &[u8],
        recipients: &[String],
    ) -> ProviderResult<SubmissionReceipt> {
        let sender = self
            .smtp
            .as_ref()
            .ok_or_else(|| ProviderError::invalid_state("no SMTP transport configured"))?;
        // Refuse bytes this seam cannot send before opening any connection.
        let sub = SourceSubmission::parse(source, recipients)?;
        let result = match sender {
            SmtpSender::Plaintext { addr } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                smtp::send(tcp, &sub.ehlo, &sub.from, &sub.to, source, None).await?
            }
            SmtpSender::ImplicitTls {
                addr,
                server_name,
                connector,
                username,
                password,
            } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                let tls = tls_connect(connector, server_name, tcp).await?;
                smtp::send(
                    tls,
                    &sub.ehlo,
                    &sub.from,
                    &sub.to,
                    source,
                    Some((username, password)),
                )
                .await?
            }
            SmtpSender::StartTls {
                addr,
                server_name,
                connector,
                username,
                password,
            } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                // Cleartext STARTTLS handshake, then upgrade the socket and transmit
                // the source bytes (with `AUTH PLAIN`) over the established TLS.
                let tcp = smtp::negotiate_starttls(tcp, &sub.ehlo).await?;
                let tls = tls_connect(connector, server_name, tcp).await?;
                smtp::send_after_starttls(
                    tls,
                    &sub.ehlo,
                    &sub.from,
                    &sub.to,
                    source,
                    Some((username, password)),
                )
                .await?
            }
        };
        self.file_source_result(result, &sub, source).await
    }

    /// Classifies the send, then (on delivery) files the **same bytes** as the Sent
    /// copy and builds the receipt — the shared filing tail, with no
    /// `assemble_filed_message` step: the wire copy and the filed copy are one and
    /// the same on this path.
    ///
    /// # Errors
    ///
    /// The classified [`ProviderError`] of a rejected or ambiguous send; never an
    /// `Err` for a Sent-filing failure (the draft path's rule, inherited: the mail
    /// has gone, and the receipt says [`Unfiled`](engine_provider::SentCopy::Unfiled)
    /// instead).
    async fn file_source_result(
        &self,
        result: smtp::SmtpResult,
        sub: &SourceSubmission,
        source: &[u8],
    ) -> ProviderResult<SubmissionReceipt> {
        ensure_delivered(&result)?;
        self.file_and_receipt(source, &sub.message_id).await
    }
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
/// into `MAIL FROM`/`RCPT TO` command lines, so only a clean ASCII token may pass —
/// `crate::smtp`'s control-character screen then guards the rest.
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
#[path = "smtp_source_tests.rs"]
mod tests;
