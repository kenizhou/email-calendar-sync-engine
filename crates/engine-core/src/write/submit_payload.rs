//! The durable intent of a mail-submission pending op.
//!
//! A [`PendingOp`](super::PendingOp) payload is the only record of what an
//! interrupted submission was meant to send — a future drainer replays it. That
//! payload is the tagged `OutboxIntent` envelope (`engine-sync`, whose drivers
//! produce it); the submission intent its `submit_mail` verb carries is this
//! *tagged* [`SubmitPayload`] rather than a bare draft: the `kind` tag tells
//! the dispatcher whether to re-render a draft or re-send already-rendered
//! bytes, instead of inferring it from the payload's shape.

use serde::{Deserialize, Serialize};

/// Serde field codec for byte payloads: standard base64 with padding (RFC 4648 §4).
///
/// Rendered messages are not guaranteed UTF-8 (signed/encrypted MIME is arbitrary
/// bytes), so they cannot travel as a plain JSON string; serde's default for
/// `Vec<u8>` under `serde_json` is a number array, which is correct but roughly
/// four times the size of the bytes. Base64 keeps the payload lossless, compact
/// and text-safe.
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    /// Serializes `bytes` as a base64 string.
    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Deserializes a base64 string into bytes, rejecting malformed input.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let encoded = <String as Deserialize>::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(D::Error::custom)
    }
}

/// The submission intent the outbox envelope's `submit_mail` verb carries,
/// tagged so the recovery dispatches on it.
///
/// Generic over the draft shape `D` the submitting layer carries (the provider
/// layer's `Draft`) so this pure contract stays free of provider types: the tag
/// and the rendered-bytes encoding are engine-core's business, the draft payload
/// is the submitter's. `D` must serialize as a map — the internally tagged
/// representation places `kind` *beside* the variant's content, which every
/// struct-shaped draft satisfies.
///
/// `RenderedSource` is the host-crypto seam: the bytes are the caller's final
/// MIME (already signed/encrypted), the engine sends them verbatim and never
/// re-renders. Because the payload is the only record left after a crash, the
/// bytes **and the envelope recipients they must be sent to** ride in the
/// payload itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubmitPayload<D> {
    /// The engine renders this draft and submits the result (`submit_mail`).
    Draft(D),
    /// Submit the caller's already-rendered bytes verbatim.
    RenderedSource {
        /// The final RFC 5322 message, exactly as the engine must send it.
        #[serde(with = "base64_bytes")]
        rfc5322: Vec<u8>,
        /// The envelope recipients — the exact `RCPT TO` set, where Bcc lives
        /// without ever entering the bytes. Empty means derive mode: the sender
        /// derives the envelope from the bytes' own `To`/`Cc` headers. A replay
        /// re-sends to this exact set rather than re-deriving it, so the payload
        /// must carry it (the crash-recovery seam).
        #[serde(default)]
        recipients: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::SubmitPayload;

    /// Stands in for the provider layer's `Draft`: the concrete draft shape is the
    /// submitting layer's business, so engine-core tests pin the tagging contract
    /// against a minimal struct instead.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestDraft {
        message_id: String,
        subject: String,
    }

    #[test]
    fn draft_variant_carries_the_draft_beside_its_kind_tag() {
        let payload = SubmitPayload::Draft(TestDraft {
            message_id: "send-1@test.local".into(),
            subject: "Hello".into(),
        });
        let value = serde_json::to_value(&payload).unwrap();
        // The exact wire shape: the tag sits *beside* the draft's fields (an
        // internally tagged enum flattens the variant's content), never wrapping it.
        assert_eq!(
            value,
            json!({
                "kind": "draft",
                "message_id": "send-1@test.local",
                "subject": "Hello",
            })
        );
        assert_eq!(
            serde_json::from_value::<SubmitPayload<TestDraft>>(value).unwrap(),
            payload
        );
    }

    #[test]
    fn rendered_source_round_trips_non_utf8_bytes_and_recipients() {
        // Signed/encrypted MIME is arbitrary bytes, not guaranteed UTF-8 — the
        // payload must carry it losslessly (crash recovery re-sends from it alone) —
        // and the envelope recipients must ride beside it: a replay re-sends to the
        // exact same RCPT TO set (where Bcc lives without ever entering the bytes)
        // instead of re-deriving it from headers the caller never wrote.
        let bytes: Vec<u8> = vec![0xFF, 0xC3, 0x28, 0x00, b'\r', b'\n', 0x80, 0xFE, 0x41];
        let recipients = vec!["bob@test.local".to_owned(), "carol@test.local".to_owned()];
        let payload = SubmitPayload::RenderedSource {
            rfc5322: bytes.clone(),
            recipients: recipients.clone(),
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["kind"], json!("rendered_source"));
        // The bytes travel as one base64 string, not a JSON number array (which
        // would be correct but ~4x larger) or a lossy String.
        let encoded = value["rfc5322"].as_str().expect("rfc5322 must be a string");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .unwrap(),
            bytes
        );
        assert_eq!(value["recipients"], json!(recipients));
        assert_eq!(
            serde_json::from_value::<SubmitPayload<TestDraft>>(value).unwrap(),
            payload
        );
    }

    #[test]
    fn rendered_source_without_recipients_decodes_to_derive_mode() {
        // A payload serialized before the field existed must decode, not error:
        // an absent set is the documented derive mode (the sender derives the
        // envelope from the bytes' own To/Cc headers), never a corrupt op.
        let legacy = json!({
            "kind": "rendered_source",
            "rfc5322": "SGVsbG8=",
        });
        assert_eq!(
            serde_json::from_value::<SubmitPayload<TestDraft>>(legacy).unwrap(),
            SubmitPayload::<TestDraft>::RenderedSource {
                rfc5322: b"Hello".to_vec(),
                recipients: Vec::new(),
            }
        );
    }

    #[test]
    fn the_tag_is_a_closed_dispatch_not_a_hint() {
        // An unknown kind must not deserialize: a drainer dispatches on this tag,
        // so an open one would silently turn a decode gap into a no-op.
        let unknown = serde_json::from_value::<SubmitPayload<TestDraft>>(json!({
            "kind": "mystery",
        }));
        assert!(unknown.is_err());
        // Malformed base64 must not deserialize either — half-decoded MIME must
        // fail loudly, not reach the wire truncated.
        let malformed = serde_json::from_value::<SubmitPayload<TestDraft>>(json!({
            "kind": "rendered_source",
            "rfc5322": "!!not base64!!",
        }));
        assert!(malformed.is_err());
    }
}
