//! The FTS tokenizer option's own tests: the creation-time `porter unicode61` /
//! `trigram` choice, the `meta.fts_tokenizer` record, the mismatch refusal, and the
//! CJK substring acceptance pins. Fork-owned (`FORKING.md` patch series, FTS tokenizer
//! option row); split from `tests.rs` when the merged file crossed the 500-line cap.

use engine_store::ManualClock;

use super::SqliteStore;
use crate::{
    options::{FtsTokenizer, OpenOptions},
    tokenizer_reconcile::{FtsTokenizerKnown, classify, ensure_compatible, record},
};

#[test]
fn fresh_database_uses_the_requested_tokenizer_for_both_fts_tables() {
    for (tokenizer, clause) in [
        (FtsTokenizer::Trigram, "trigram"),
        (FtsTokenizer::PorterUnicode61, "porter unicode61"),
    ] {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, tokenizer).unwrap();
        for table in ["fts_index", "message_body_fts"] {
            let ddl: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                ddl.contains(&format!("tokenize = '{clause}'")),
                "{table} under {clause}: {ddl}"
            );
        }
    }
}

/// `Fresh` ⇔ no FTS index exists yet (a v0 or v1 database this open shapes):
/// anything is compatible, and the open records what it chose.
#[test]
fn a_fresh_database_accepts_any_request_and_records_it() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    let found = classify(&conn).unwrap();
    assert!(matches!(found, FtsTokenizerKnown::Fresh));
    ensure_compatible(found, FtsTokenizer::Trigram).unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    record(&conn, FtsTokenizer::Trigram).unwrap();
    assert_eq!(recorded(&conn), "trigram");
}

/// A database created before the option existed still has a porter `fts_index`;
/// classify derives porter from the DDL, a porter open is accepted and fills the
/// record, and a trigram open is refused.
#[test]
fn a_pre_option_database_derives_porter_records_it_and_refuses_trigram() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();
    // The full schema, but no recorded row — exactly how a pre-option database
    // meets this build.
    let found = classify(&conn).unwrap();
    assert!(matches!(
        found,
        FtsTokenizerKnown::Known(FtsTokenizer::PorterUnicode61)
    ));
    ensure_compatible(found, FtsTokenizer::PorterUnicode61).unwrap();
    record(&conn, FtsTokenizer::PorterUnicode61).unwrap();
    assert_eq!(recorded(&conn), "porter unicode61");
    let refused = ensure_compatible(found, FtsTokenizer::Trigram);
    let msg = format!("{}", refused.unwrap_err());
    assert!(msg.contains("fts tokenizer mismatch"), "{msg}");
}

#[test]
fn a_recorded_tokenizer_mismatching_the_request_is_refused() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('fts_tokenizer', 'trigram')",
        [],
    )
    .unwrap();
    let found = classify(&conn).unwrap();
    assert!(matches!(
        found,
        FtsTokenizerKnown::Known(FtsTokenizer::Trigram)
    ));
    assert!(ensure_compatible(found, FtsTokenizer::PorterUnicode61).is_err());
    // Re-requesting the recorded value stays accepted, and record leaves the
    // row alone.
    ensure_compatible(found, FtsTokenizer::Trigram).unwrap();
    record(&conn, FtsTokenizer::Trigram).unwrap();
    assert_eq!(recorded(&conn), "trigram");
}

/// Finding pin (v2/v3): a database stopped after migration V2 has a porter
/// `fts_index` and no `meta` table (that arrives in V4). A trigram open must be
/// refused off the DDL alone — not misread as fresh, which would silently build
/// `message_body_fts` under trigram and record the mix as pure trigram.
#[test]
fn a_v2_database_classifies_porter_and_refuses_trigram_unmutated() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::V1).unwrap();
    conn.execute_batch(&crate::schema::v2(FtsTokenizer::PorterUnicode61))
        .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();

    let found = classify(&conn).unwrap();
    assert!(matches!(
        found,
        FtsTokenizerKnown::Known(FtsTokenizer::PorterUnicode61)
    ));
    assert!(ensure_compatible(found, FtsTokenizer::Trigram).is_err());
    // The refusal path is read-only: the database stands where it did.
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let meta: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(meta, 0, "no meta table was conjured");
}

/// Finding pin (mutate-before-refuse): a v4 database (meta table, no
/// `fts_tokenizer` row, porter `fts_index`, no `message_body_fts`) opened with
/// the trigram option must fail **before** any migration step runs. Under the
/// old ordering, v5 would build and commit a trigram `message_body_fts`
/// (user_version 4→11) and only then refuse, leaving a mixed database that a
/// later default open would record as porter.
#[test]
fn a_v4_database_refusing_trigram_is_left_unmutated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("v4.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(crate::schema::V1).unwrap();
        conn.execute_batch(&crate::schema::v2(FtsTokenizer::PorterUnicode61))
            .unwrap();
        conn.execute_batch(crate::schema::V3).unwrap();
        conn.execute_batch(crate::schema::V4).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
    }
    let refused = SqliteStore::open_with(
        &path,
        ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant")),
        OpenOptions {
            fts_tokenizer: FtsTokenizer::Trigram,
        },
    );
    let msg = format!("{}", refused.unwrap_err());
    assert!(msg.contains("fts tokenizer mismatch"), "{msg}");

    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4, "the refusal did not run or commit a step");
    let body_fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'message_body_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body_fts, 0, "no trigram message_body_fts was built");
    let row: i64 = conn
        .query_row(
            "SELECT count(*) FROM meta WHERE key = 'fts_tokenizer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row, 0, "no tokenizer row was recorded over a refusal");
}

/// Finding pin (crash window): a trigram database whose migrate committed but
/// whose record insert never did — process death in between. It must classify
/// as trigram from the DDL, accept a trigram open (repairing the record), and
/// refuse a default porter open instead of being misrecorded as porter.
#[test]
fn a_crashed_trigram_database_classifies_from_the_ddl_and_repairs_the_record() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    // ...and then the process died before `record` ran.

    let found = classify(&conn).unwrap();
    assert!(matches!(
        found,
        FtsTokenizerKnown::Known(FtsTokenizer::Trigram)
    ));
    ensure_compatible(found, FtsTokenizer::Trigram).unwrap();
    assert!(
        ensure_compatible(found, FtsTokenizer::PorterUnicode61).is_err(),
        "a default open must not record porter over trigram tables"
    );
    record(&conn, FtsTokenizer::Trigram).unwrap();
    assert_eq!(recorded(&conn), "trigram");
}

/// The recorded `meta.fts_tokenizer` value, as `record` leaves it.
fn recorded(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// The `_with` constructors must thread the option all the way through
/// `configure`: the FTS tables this open creates carry the requested tokenizer
/// and the choice is recorded in `meta`. An in-memory database vanishes with
/// its connection, so construction succeeding under a non-default option —
/// with the schema and record to show for it — is this test's assertion; the
/// mismatch refusal itself is covered at the connection level above.
#[tokio::test]
async fn open_in_memory_with_trigram_creates_and_records_the_trigram_index() {
    let store = SqliteStore::open_in_memory_with(
        ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant")),
        OpenOptions {
            fts_tokenizer: FtsTokenizer::Trigram,
        },
    )
    .expect("open under the trigram option");
    let (ddl, recorded): (String, String) = store
        .read(|conn| {
            let ddl = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'fts_index'",
                    [],
                    |r| r.get(0),
                )
                .expect("fts_index exists");
            let recorded = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
                    [],
                    |r| r.get(0),
                )
                .expect("the tokenizer row is recorded");
            (ddl, recorded)
        })
        .await;
    assert!(ddl.contains("tokenize = 'trigram'"), "{ddl}");
    assert_eq!(recorded, "trigram");
}

/// The kylins CJK acceptance case (spec P0 §4): a mid-string query must match
/// under `trigram`. This is the search-as-you-type phrase-prefix form the
/// search layer really issues (`fts_match`), not a hand-rolled MATCH.
#[test]
fn trigram_matches_mid_string_cjk_where_porter_cannot() {
    let body = "请查收今天的会议纪要附件";
    let query = "\"会议纪\"*";
    for (tokenizer, expected) in [
        (FtsTokenizer::Trigram, 1),
        (FtsTokenizer::PorterUnicode61, 0),
    ] {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, tokenizer).unwrap();
        conn.execute(
            "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
             VALUES ('s', 'm1', '周报', ?1, '会议室 3A')",
            [body],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM fts_index WHERE fts_index MATCH ?1",
                [query],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, expected, "{tokenizer:?} on query {query}");
    }
}

/// The ≥3-character rule is part of the contract: a 2-character query cannot
/// use a trigram index (kylins' previous engine behaves the same way — no
/// regression, now documented, spec P0 §4).
#[test]
fn trigram_two_character_queries_do_not_match() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    conn.execute(
        "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
         VALUES ('s', 'm1', '周报', '请查收今天的会议纪要附件', '')",
        [],
    )
    .unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM fts_index WHERE fts_index MATCH ?1",
            ["\"会议\"*"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 0);
}
