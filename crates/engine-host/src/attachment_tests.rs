//! The attachment tests: the vault's content addressing in isolation, then the
//! three-tier read against a really-open engine — each tier pinned by a fake
//! provider whose every call is counted, so "zero provider calls" is asserted,
//! not assumed. The fake [`AttachmentFetch`] delegates to the orphan-rule
//! default exactly the way the EAS/IMAP impls do, so the tier-(c) tests also
//! prove the default composition end to end.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use engine_api::{AccountId, Engine};
use engine_core::{
    ids::{MailboxId, MessageId},
    mail::{AttachmentPartId, Message},
    membership::Memberships,
    raw::RawMime,
};
use engine_provider::{Capabilities, ConnectionInfo, Provider, ProviderError, ProviderResult};
use engine_store::MessageSourceCache;
use tempfile::TempDir;

use super::*;

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// One message filed once — the object an attachment read addresses.
fn message() -> Message {
    Message::new(
        MessageId::try_from("imap:v1:u1@INBOX").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

/// The fixture source the shell's old materialize test used: a text part plus
/// one base64 attachment (`UERG` → `PDF`), so `AttachmentPartId::new(0)` is the
/// downloadable attachment and its decoded bytes are exactly `b"PDF"`.
fn fixture_source() -> RawMime {
    RawMime::new(
        b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
            --m\r\nContent-Type: text/plain\r\n\r\nthe body text\r\n\
            --m\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
            Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
            --m--\r\n"
            .to_vec(),
    )
}

/// A provider whose only verb is the whole-source fetch, counting every call —
/// the witness that pins which tier served a read.
struct CountingSource {
    caps: Capabilities,
    calls: Mutex<Vec<String>>,
}

impl CountingSource {
    fn new() -> Self {
        Self {
            caps: Capabilities::none().with_mail().with_message_source(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Provider for CountingSource {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(self.caps)
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        self.calls
            .lock()
            .unwrap()
            .push(message.id.key().as_str().to_owned());
        Ok(fixture_source())
    }
}

/// The tier-(c) seam under test: an [`AttachmentFetch`] that counts every call
/// and delegates to the orphan-rule default — the exact shape of the
/// `EasAdapter`/`ImapProvider` impls, so what the tests pin is the real path.
struct CountingFetch {
    provider: CountingSource,
    calls: AtomicUsize,
}

impl CountingFetch {
    fn new() -> Self {
        Self {
            provider: CountingSource::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AttachmentFetch for CountingFetch {
    async fn fetch_message_attachment(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        default_fetch_attachment(&self.provider, account, message, part).await
    }
}

#[test]
fn vault_content_addresses_bytes_and_repeats_one_put_idempotently() {
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());

    let first = vault.put(b"hello vault").unwrap();
    let second = vault.put(b"hello vault").unwrap();

    // Same bytes → same path, the second put no error, no rewrite: the name IS
    // the digest of the content.
    assert_eq!(first, second, "content addressing: one path per content");
    let name = first.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), 64, "a sha256 hex digest: {name}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "lowercase hex"
    );
    assert_eq!(
        first
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        &name[..2],
        "the shard directory is the digest's first two characters"
    );
    assert_eq!(std::fs::read(&first).unwrap(), b"hello vault");
}

#[test]
fn vault_separates_different_bytes_into_different_paths() {
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());

    let a = vault.put(b"one").unwrap();
    let b = vault.put(b"two").unwrap();

    assert_ne!(a, b, "different content, different names");
    assert_eq!(std::fs::read(&a).unwrap(), b"one");
    assert_eq!(std::fs::read(&b).unwrap(), b"two");
    // `get_path` is the read-side of the same addressing: hand it the digest of
    // `b"one"` and it names that file.
    let digest = digest_of(b"one");
    assert_eq!(vault.get_path(&digest), a);
}

#[tokio::test]
async fn an_indexed_part_is_served_from_the_vault_without_any_provider_call() {
    let engine = Engine::open_in_memory().unwrap();
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());
    let fetch = CountingFetch::new();

    // Seed exactly what tier (a) reads: the bytes in the vault and the index
    // line that points the (account, message, part) at their digest.
    let digest = digest_of(b"PDF");
    vault.put(b"PDF").unwrap();
    let mut index = load_index(&vault.root).unwrap();
    index.insert(
        index_entry_key("acct-1", "imap:v1:u1@INBOX", AttachmentPartId::new(0)),
        digest,
    );
    save_index(&vault.root, &index).unwrap();

    let bytes = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();

    assert_eq!(bytes, b"PDF");
    assert_eq!(fetch.calls(), 0, "the vault answers, the fetcher is idle");
    assert_eq!(fetch.provider.calls(), 0, "no whole-source fetch either");
    // And the store holds no cached source — tier (a) needed nothing but the
    // vault and its index.
    assert!(
        engine
            .host_store()
            .get_message_source(
                &account(),
                &engine_core::ids::ProviderKey::new("imap:v1:u1@INBOX").unwrap(),
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_cached_whole_source_extracts_into_the_vault_without_any_provider_call() {
    let engine = Engine::open_in_memory().unwrap();
    let key = engine_core::ids::ProviderKey::new("imap:v1:u1@INBOX").unwrap();
    engine
        .host_store()
        .put_message_source(&account(), &key, fixture_source())
        .await
        .unwrap();
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());
    let fetch = CountingFetch::new();

    let bytes = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();

    assert_eq!(bytes, b"PDF", "the base64 part decoded");
    assert_eq!(fetch.calls(), 0, "the store's cache answered the fetch");
    assert_eq!(fetch.provider.calls(), 0);
    // The extraction also landed in the vault and its index, so the next read
    // is tier (a).
    let digest = digest_of(b"PDF");
    assert_eq!(std::fs::read(vault.get_path(&digest)).unwrap(), b"PDF");
    let index = load_index(&vault.root).unwrap();
    assert_eq!(
        index.get(&index_entry_key(
            "acct-1",
            "imap:v1:u1@INBOX",
            AttachmentPartId::new(0)
        )),
        Some(&digest),
        "the digest accounting names what the vault now holds"
    );
}

#[tokio::test]
async fn an_uncached_part_fetches_once_then_serves_from_the_vault() {
    let engine = Engine::open_in_memory().unwrap();
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());
    let fetch = CountingFetch::new();

    // Tier (c): nothing cached anywhere — the default path fetches the whole
    // source once, extracts the part, and lands it in the vault.
    let bytes = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"PDF");
    assert_eq!(fetch.calls(), 1, "exactly one attachment fetch");
    assert_eq!(
        fetch.provider.calls(),
        1,
        "the default path is one whole-source fetch"
    );
    let digest = digest_of(b"PDF");
    assert_eq!(std::fs::read(vault.get_path(&digest)).unwrap(), b"PDF");
    assert!(
        load_index(&vault.root)
            .unwrap()
            .contains_key(&index_entry_key(
                "acct-1",
                "imap:v1:u1@INBOX",
                AttachmentPartId::new(0)
            ))
    );

    // Tier (a) on the second ask: same bytes, zero new calls of either kind.
    let again = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();
    assert_eq!(again, b"PDF");
    assert_eq!(fetch.calls(), 1, "the vault served the second read");
    assert_eq!(fetch.provider.calls(), 1);
}

#[tokio::test]
async fn a_part_no_source_carries_answers_not_found() {
    let engine = Engine::open_in_memory().unwrap();
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());
    let fetch = CountingFetch::new();

    // Uncached: the default path fetches the source, finds no such part, and
    // the error says so.
    let err = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(9),
    )
    .await
    .unwrap_err();
    assert!(err.contains("not found"), "the shape of the miss: {err}");
    assert!(
        load_index(&vault.root)
            .unwrap()
            .keys()
            .all(|key| !key.ends_with("\t9")),
        "nothing was accounted into the vault for a part that does not exist"
    );

    // Cached source, same question: the store's cache is the authority — the
    // miss answers without any provider call at all.
    let key = engine_core::ids::ProviderKey::new("imap:v1:u1@INBOX").unwrap();
    engine
        .host_store()
        .put_message_source(&account(), &key, fixture_source())
        .await
        .unwrap();
    let err = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(9),
    )
    .await
    .unwrap_err();
    assert!(err.contains("not found"), "the shape of the miss: {err}");
    assert_eq!(
        fetch.provider.calls(),
        1,
        "still only the first read's fetch"
    );
}

#[tokio::test]
async fn a_corrupt_vault_file_is_repaired_by_the_next_landing() {
    let engine = Engine::open_in_memory().unwrap();
    let dir = TempDir::new().unwrap();
    let vault = AttachmentVault::new(dir.path());
    let fetch = CountingFetch::new();

    // The lying state: the index points at the digest of the right bytes, but
    // the file at that path is truncated garbage (a crash mid-write, a disk
    // fault — whatever left the name holding the wrong content).
    let digest = digest_of(b"PDF");
    vault.put(b"PDF").unwrap();
    let path = vault.get_path(&digest);
    std::fs::write(&path, b"truncated").unwrap();
    let mut index = load_index(&vault.root).unwrap();
    index.insert(
        index_entry_key("acct-1", "imap:v1:u1@INBOX", AttachmentPartId::new(0)),
        digest,
    );
    save_index(&vault.root, &index).unwrap();

    // Tier (a) refuses the corrupt file (digest mismatch) and falls through;
    // with no cached source, tier (c) fetches once and the landing's content
    // check overwrites the corrupt file with the correct bytes.
    let bytes = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"PDF", "the fall-through returned the right bytes");
    assert_eq!(fetch.calls(), 1, "the corrupt file cost one fetch, once");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"PDF",
        "the vault file is healed at the same path"
    );

    // Healed means tier (a) again: same bytes, zero further calls.
    let again = attachment_bytes(
        &engine,
        &vault,
        &fetch,
        &account(),
        &message(),
        AttachmentPartId::new(0),
    )
    .await
    .unwrap();
    assert_eq!(again, b"PDF");
    assert_eq!(
        fetch.calls(),
        1,
        "the healed vault served the second read, not the provider"
    );
}
