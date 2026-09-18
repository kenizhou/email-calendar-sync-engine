//! The trigram flavour of the two FTS-bearing migration steps (`V2`, `V5`).
//! Fork-owned (`FORKING.md`, FTS tokenizer option row): upstream's `schema.rs` carries
//! the `porter unicode61` consts verbatim, and a database created under the
//! `Trigram` option uses these DDLs in their place — the same shape, the
//! `tokenize=` clause aside, because SQLite fixes an FTS table's tokenizer at
//! creation and the choice must ride the step that creates the table.

/// The trigram twin of [`V2`](super::V2): the same reshape, `fts_index` created
/// with the `trigram` tokenizer so CJK mid-string queries match (see
/// `options::FtsTokenizer`).
pub(crate) const V2_TRIGRAM: &str = "\
DROP TABLE fts_doc;

CREATE TABLE fts_doc (
    rowid        INTEGER PRIMARY KEY,
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    subject      TEXT NOT NULL DEFAULT '',
    body         TEXT NOT NULL DEFAULT '',
    location     TEXT NOT NULL DEFAULT '',
    UNIQUE (scope_key, provider_key)
) STRICT;

CREATE VIRTUAL TABLE fts_index USING fts5 (
    subject, body, location,
    content = 'fts_doc',
    content_rowid = 'rowid',
    tokenize = 'trigram'
);

CREATE TRIGGER fts_doc_ai AFTER INSERT ON fts_doc BEGIN
    INSERT INTO fts_index (rowid, subject, body, location)
    VALUES (new.rowid, new.subject, new.body, new.location);
END;

CREATE TRIGGER fts_doc_ad AFTER DELETE ON fts_doc BEGIN
    INSERT INTO fts_index (fts_index, rowid, subject, body, location)
    VALUES ('delete', old.rowid, old.subject, old.body, old.location);
END;

CREATE TRIGGER fts_doc_au AFTER UPDATE ON fts_doc BEGIN
    INSERT INTO fts_index (fts_index, rowid, subject, body, location)
    VALUES ('delete', old.rowid, old.subject, old.body, old.location);
    INSERT INTO fts_index (rowid, subject, body, location)
    VALUES (new.rowid, new.subject, new.body, new.location);
END;

CREATE TABLE mail_index (
    scope_key      TEXT    NOT NULL,
    provider_key   TEXT    NOT NULL,
    date_utc       TEXT,
    has_attachment INTEGER NOT NULL,
    thread_id      TEXT,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE INDEX mail_index_date ON mail_index (scope_key, date_utc);

CREATE TABLE mail_address (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    field        TEXT NOT NULL,
    addr         TEXT NOT NULL,
    name         TEXT,
    PRIMARY KEY (scope_key, provider_key, field, addr)
) STRICT, WITHOUT ROWID;

CREATE INDEX mail_address_lookup ON mail_address (scope_key, field, addr);

CREATE TABLE membership (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    kind         TEXT NOT NULL,
    value        TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key, kind, value)
) STRICT, WITHOUT ROWID;

CREATE INDEX membership_lookup ON membership (scope_key, kind, value);

CREATE TABLE event_index (
    scope_key      TEXT    NOT NULL,
    provider_key   TEXT    NOT NULL,
    has_conference INTEGER NOT NULL,
    my_partstat    TEXT,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE event_participant (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    role         TEXT NOT NULL,
    addr         TEXT NOT NULL,
    partstat     TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key, role, addr)
) STRICT, WITHOUT ROWID;

CREATE INDEX event_participant_lookup ON event_participant (scope_key, role, addr);

CREATE TABLE embedding (
    scope_key    TEXT    NOT NULL,
    provider_key TEXT    NOT NULL,
    chunk_ix     INTEGER NOT NULL,
    model        TEXT    NOT NULL,
    dim          INTEGER NOT NULL,
    vector       BLOB    NOT NULL,
    PRIMARY KEY (scope_key, provider_key, chunk_ix)
) STRICT, WITHOUT ROWID;
";

/// The trigram twin of [`V5`](super::V5): `message_body_fts` created with the
/// `trigram` tokenizer.
pub(crate) const V5_TRIGRAM: &str = "\
CREATE TABLE message_source (
    account      TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    fetched_at   TEXT NOT NULL,
    PRIMARY KEY (account, provider_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_body (
    rowid        INTEGER PRIMARY KEY,
    account      TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    plain        TEXT NOT NULL DEFAULT '',
    html         TEXT,
    fetched_at   TEXT NOT NULL,
    UNIQUE (account, provider_key)
) STRICT;

CREATE VIRTUAL TABLE message_body_fts USING fts5 (
    plain,
    content = 'message_body',
    content_rowid = 'rowid',
    tokenize = 'trigram'
);

CREATE TRIGGER message_body_ai AFTER INSERT ON message_body BEGIN
    INSERT INTO message_body_fts (rowid, plain) VALUES (new.rowid, new.plain);
END;

CREATE TRIGGER message_body_ad AFTER DELETE ON message_body BEGIN
    INSERT INTO message_body_fts (message_body_fts, rowid, plain)
    VALUES ('delete', old.rowid, old.plain);
END;

CREATE TRIGGER message_body_au AFTER UPDATE ON message_body BEGIN
    INSERT INTO message_body_fts (message_body_fts, rowid, plain)
    VALUES ('delete', old.rowid, old.plain);
    INSERT INTO message_body_fts (rowid, plain) VALUES (new.rowid, new.plain);
END;
";
