//! Per-part attachment reads over a durable, content-addressed file vault
//! (ER-2): fetch one attachment, keep it forever-lazy, survive the source
//! cache's size cap.
//!
//! The engine's own attachment read (`Engine::message_attachment`) serves a
//! part from the cached raw source and fetches the **whole message** when it
//! is not cached — correct for the facade's single-message contract, but it
//! leaves the part's survival tied to the source cache:
//! `drop_message_sources_over` (the size cap that keeps a mailbox's Tier-3
//! bytes bounded) forgets the raw source, and with it every part extracted
//! from it. A downloaded attachment is different goods: the user asked for
//! *these bytes*, so they belong somewhere the cap does not reach.
//!
//! # Why a file vault, not a store table
//!
//! [`AttachmentVault`] is a directory the **host** owns, not a table the
//! engine owns: D11's semantics (lazy, content-addressed, alive after source
//! slimming) are host policy, and landing them in the store would cost a
//! registration point — a migration, a store trait, a facade verb — for data
//! the engine has no read of its own to serve. A directory costs none of
//! that. Its layout is the engine blob area's own discipline, applied
//! host-side:
//!
//! ```text
//! <root>/<digest[..2]>/<digest>   the bytes, named by their sha256
//! <root>/index                    (account, message key, part) → digest
//! ```
//!
//! Same bytes land once wherever they are asked from (content addressing
//! dedupes), files are immutable so a partial write is detectable (the read
//! side re-verifies the digest), and the two-character shard keeps any one
//! directory small. The `index` sidecar is the *accounting*: it answers "is
//! this part already in the vault", which the files alone cannot (a digest
//! names content, not which message asked for it). It is one line per entry,
//! loaded and re-saved whole per call — a P1 simplification that assumes one
//! writer per vault root (the same single-host posture as the engine's own
//! store) and stays trivially inspectable; the honest upgrade path is a
//! SQLite file in the root, not a store table.
//!
//! # The three tiers
//!
//! [`attachment_bytes`] serves a part as cheaply as the state allows:
//! **(a)** the index names a digest the vault holds → read and return, zero
//! provider traffic; **(b)** the engine's store already cached the whole
//! source → extract the part with `engine-mime` and land it in the vault
//! (the extraction and the facade's own attachment read share one parser, so
//! the two surfaces agree on what a part is); **(c)** nothing local → one
//! [`AttachmentFetch`] call, and the bytes land in the vault so the next ask
//! is tier (a). Every landing writes the vault file **and** the index entry
//! together — bytes without accounting are unreachable, accounting without
//! bytes is a lie the read side would follow into a missing file.

use std::fs;

use async_trait::async_trait;
use engine_api::{AccountId, Engine, Provider};
use engine_core::mail::{AttachmentPartId, Message};
use engine_provider::ProviderError;
use engine_store::MessageSourceCache;
use provider_eas::EasAdapter;
use provider_imap::ImapProvider;
use tokio::io::{AsyncRead, AsyncWrite};

pub use self::vault::AttachmentVault;
use crate::attachment::vault::{digest_of, index_entry_key, load_index, save_index};

/// The vault's own leaf module: the content-addressed directory plus its
/// index sidecar. A nested module (not inline) keeps `attachment.rs` at one
/// responsibility per file chunk under the 500-line ceiling.
mod vault {
    use std::{
        collections::{BTreeMap, HashMap},
        fmt::Write as _,
        fs,
        path::{Path, PathBuf},
    };

    use engine_core::mail::AttachmentPartId;
    use sha2::{Digest, Sha256};

    /// A durable, content-addressed store of decoded attachment bytes.
    ///
    /// Files live at `<root>/<digest[..2]>/<digest>` — the engine blob area's
    /// layout, applied host-side — and the `index` sidecar under the same root
    /// maps `(account, message key, part)` → digest so a caller can ask "is
    /// this part already here". See the module docs for why this is a
    /// directory and not a store table.
    #[derive(Debug)]
    pub struct AttachmentVault {
        /// The vault's root directory, the host's own; the engine never
        /// touches it.
        pub(crate) root: PathBuf,
    }

    impl AttachmentVault {
        /// Creates a vault rooted at `root`, creating nothing — directories
        /// appear when the first [`AttachmentVault::put`] writes.
        #[must_use]
        pub fn new(root: impl Into<PathBuf>) -> Self {
            Self { root: root.into() }
        }

        /// Writes `bytes` into the vault and returns the path they live at.
        ///
        /// Idempotent by content addressing: the same bytes always map to the
        /// same path, and bytes already present are left untouched (no
        /// rewrite, so a concurrent reader never sees a torn file).
        ///
        /// # Errors
        ///
        /// Returns the filesystem error's text when the shard directory
        /// cannot be created or the file cannot be written.
        pub fn put(&self, bytes: &[u8]) -> Result<PathBuf, String> {
            self.store(bytes).map(|(path, _)| path)
        }

        /// The path the digest `digest` (hex sha256) lives at — the read side
        /// of the same addressing [`AttachmentVault::put`] writes.
        #[must_use]
        pub fn get_path(&self, digest: &str) -> PathBuf {
            let shard: String = digest.chars().take(2).collect();
            self.root.join(shard).join(digest)
        }

        /// Writes `bytes` and returns `(path, digest)` in one hash, so a
        /// caller that needs both (the index write after a put) does not pay
        /// for the digest twice.
        pub(crate) fn store(&self, bytes: &[u8]) -> Result<(PathBuf, String), String> {
            let digest = digest_of(bytes);
            let path = self.get_path(&digest);
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).map_err(|err| {
                        format!("vault directory {} cannot be created: {err}", dir.display())
                    })?;
                }
                fs::write(&path, bytes).map_err(|err| {
                    format!("vault file {} cannot be written: {err}", path.display())
                })?;
            }
            Ok((path, digest))
        }
    }

    /// The hex sha256 of `bytes` — the vault's one naming discipline, shared
    /// by files and index entries.
    pub(crate) fn digest_of(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(64);
        // `write!` to a String cannot fail; the capacity is exact.
        for byte in Sha256::digest(bytes) {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// The index key naming one part of one message of one account:
    /// tab-joined, the exact spine of an index line's first three fields.
    pub(crate) fn index_entry_key(account: &str, key: &str, part: AttachmentPartId) -> String {
        format!("{account}\t{key}\t{}", part.as_u32())
    }

    /// Reads the vault's index: `(account, key, part)` → digest.
    ///
    /// A missing index is an empty one (a fresh vault); a malformed line is
    /// skipped rather than fatal — a torn final line from a crash mid-write
    /// must not brick every attachment read, and the worst case is one
    /// re-fetch. This deliberately mirrors the store's own corrupt-blob
    /// posture: unverifiable state reads as absent.
    ///
    /// # Errors
    ///
    /// Returns the filesystem error's text when the index exists but cannot
    /// be read.
    pub(crate) fn load_index(root: &Path) -> Result<HashMap<String, String>, String> {
        let text = match fs::read_to_string(root.join("index")) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(err) => return Err(format!("vault index cannot be read: {err}")),
        };
        Ok(parse_lines(&text))
    }

    /// Parses the index body: one `account\tkey\tpart\tdigest` line per entry,
    /// malformed lines skipped (see [`load_index`]).
    fn parse_lines(text: &str) -> HashMap<String, String> {
        let mut index = HashMap::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            if let [account, key, part, digest] = fields[..] {
                index.insert(format!("{account}\t{key}\t{part}"), digest.to_owned());
            }
        }
        index
    }

    /// Writes the whole index back — entries in sorted order so the file is
    /// deterministic whatever order the map was built in. Per-call load/save
    /// is the documented P1 simplification: one writer per vault root.
    ///
    /// # Errors
    ///
    /// Returns the filesystem error's text when the index cannot be written.
    pub(crate) fn save_index(root: &Path, index: &HashMap<String, String>) -> Result<(), String> {
        let sorted: BTreeMap<&String, &String> = index.iter().collect();
        let mut text = String::new();
        for (entry, digest) in sorted {
            let _ = writeln!(text, "{entry}\t{digest}");
        }
        let path = root.join("index");
        fs::write(&path, text)
            .map_err(|err| format!("vault index {} cannot be written: {err}", path.display()))
    }
}

/// Fetches one attachment part's decoded bytes — the seam a provider that can
/// do better than "fetch the whole message" overrides.
///
/// The default ([`default_fetch_attachment`], what every impl delegates to
/// today) costs one whole-source fetch per part. The two protocol-aware
/// overrides the plan names are later work, and both need provider-crate
/// seams this crate must not improvise: EAS's `ItemOperations` attachment
/// fetch is addressed by the `FileReference` Sync metadata carries but the
/// adapter does not yet translate onto `Message`, and IMAP's `BODY.PEEK[
/// <section>]` needs a section-path derivation provider-imap does not yet
/// expose (its BODYSTRUCTURE pass only answers whether a downloadable part
/// exists). Until those land, the trait exists so the override is one local
/// impl away, not a rewrite of [`attachment_bytes`].
#[async_trait]
pub trait AttachmentFetch {
    /// Fetches the bytes of `part` of `message`, decoded.
    ///
    /// # Errors
    ///
    /// Returns the provider's error; a part the source does not carry is a
    /// permanent "not found".
    async fn fetch_message_attachment(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, ProviderError>;
}

/// The default per-part fetch every provider gets: fetch the whole raw source
/// once, extract the named part with the same `engine-mime` extractor the
/// facade's own attachment read uses — one behavior, two surfaces.
///
/// # Errors
///
/// Returns the provider's whole-source fetch error unchanged, or a
/// permanent error naming the part when the fetched source carries no such
/// downloadable part.
pub async fn default_fetch_attachment<P: Provider>(
    provider: &P,
    account: &AccountId,
    message: &Message,
    part: AttachmentPartId,
) -> Result<Vec<u8>, ProviderError> {
    let raw = provider.fetch_message_source(account, message).await?;
    let content = engine_mime::extract_attachment(&raw, part).ok_or_else(|| {
        ProviderError::permanent(format!(
            "attachment part {} not found in the source of {}",
            part.as_u32(),
            message.id.key().as_str()
        ))
    })?;
    Ok(content.into_bytes())
}

#[async_trait]
impl AttachmentFetch for EasAdapter {
    async fn fetch_message_attachment(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, ProviderError> {
        default_fetch_attachment(self, account, message, part).await
    }
}

#[async_trait]
impl<S> AttachmentFetch for ImapProvider<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn fetch_message_attachment(
        &self,
        account: &AccountId,
        message: &Message,
        part: AttachmentPartId,
    ) -> Result<Vec<u8>, ProviderError> {
        default_fetch_attachment(self, account, message, part).await
    }
}

/// Returns one attachment part's bytes as cheaply as the local state allows —
/// the three tiers, in order:
///
/// **(a) Vault.** The index names a digest the vault holds → read, re-verify
/// the digest (content addressing is the vault's only integrity story), return.
/// Zero provider traffic, zero store reads — this tier is what makes a
/// downloaded attachment durable: it answers even after
/// `drop_message_sources_over` slimmed the source cache away. A missing or
/// unverifiable file reads as a miss and falls through, exactly like the
/// store's own corrupt-blob posture.
///
/// **(b) Cached whole source.** The engine's store holds the message's raw
/// source → extract the part with `engine-mime` (the facade's own extractor,
/// so the two surfaces agree on what a part is) → land the bytes in the vault
/// and the entry in the index → return. Zero provider traffic.
///
/// **(c) One fetch.** Nothing local → `fetch.fetch_message_attachment`, land
/// the result in vault + index, return. The next ask for this part is tier (a).
///
/// A source that is readable but carries no such part is a definitive answer
/// at whichever tier read it — an `Err` whose text says "not found" — not a
/// fall-through to the network: the raw bytes are the authority on their own
/// parts, and re-asking a provider cannot add one.
///
/// # Errors
///
/// Returns a text error naming what failed: the vault or index I/O, the
/// store's source-cache read, or the provider fetch (passed through verbatim,
/// so a "not found" keeps its shape).
pub async fn attachment_bytes(
    engine: &Engine,
    vault: &AttachmentVault,
    fetch: &dyn AttachmentFetch,
    account: &AccountId,
    message: &Message,
    part: AttachmentPartId,
) -> Result<Vec<u8>, String> {
    let entry = index_entry_key(account.as_str(), message.id.key().as_str(), part);

    // Tier (a): the index's digest plus the file it names, verified.
    if let Some(digest) = load_index(&vault.root)?.get(&entry).cloned() {
        match fs::read(vault.get_path(&digest)) {
            Ok(bytes) if digest_of(&bytes) == digest => return Ok(bytes),
            // A missing or unverifiable file is a miss, not an error: the
            // landing below re-writes both halves.
            Ok(_) | Err(_) => {}
        }
    }

    // Tier (b): the cached whole source is the authority on its parts.
    let cached = engine
        .host_store()
        .get_message_source(account, message.id.key())
        .await
        .map_err(|err| format!("the source cache cannot be read: {err}"))?;
    if let Some(raw) = cached {
        let content = engine_mime::extract_attachment(&raw, part)
            .ok_or_else(|| not_found(part, message, "the cached source carries no such part"))?;
        let bytes = content.into_bytes();
        land(vault, &entry, &bytes)?;
        return Ok(bytes);
    }

    // Tier (c): one per-part fetch, landed for the next ask.
    let bytes = fetch
        .fetch_message_attachment(account, message, part)
        .await
        .map_err(|err| err.to_string())?;
    land(vault, &entry, &bytes)?;
    Ok(bytes)
}

/// Lands `bytes` in the vault and points the index `entry` at their digest —
/// the one write path every producing tier shares, so the accounting never
/// drifts from the files.
fn land(vault: &AttachmentVault, entry: &str, bytes: &[u8]) -> Result<(), String> {
    let (_, digest) = vault.store(bytes)?;
    let mut index = load_index(&vault.root)?;
    index.insert(entry.to_owned(), digest);
    save_index(&vault.root, &index)
}

/// The miss shape every tier answers a part no source carries.
fn not_found(part: AttachmentPartId, message: &Message, detail: &str) -> String {
    format!(
        "attachment part {} of {} not found: {detail}",
        part.as_u32(),
        message.id.key().as_str()
    )
}

#[cfg(test)]
#[path = "attachment_tests.rs"]
mod attachment_tests;
