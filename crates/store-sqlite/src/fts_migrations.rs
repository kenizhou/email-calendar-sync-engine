//! The trigram flavour of the migration list. Fork-owned (`FORKING.md`, FTS
//! tokenizer option row): `migrations.rs` is upstream's verbatim — its
//! `const MIGRATIONS` builds the default `porter unicode61` shape — and a
//! database created under the `Trigram` option runs this list instead, with the
//! two FTS-bearing steps swapped for their trigram twins
//! ([`schema::fts`](crate::schema::fts)) and the fork's own steps appended.
//! Everything else (the runner, every other step's DDL) is upstream's, reused
//! in place, so a new upstream step is adopted by adding one line here.

use engine_store::{Result, SchemaStatus};
use rusqlite::Connection;

use crate::{
    backfill,
    migrations::{MIGRATIONS, Migration, run},
    schema::{self, fts},
};

/// The ordered steps of a default (porter) database: upstream's own list, plus
/// the fork's appended steps. Index `i` is schema version `i + 1`; see
/// `migrations.rs` for the append-only discipline.
pub(crate) fn migrations_porter() -> Vec<Migration> {
    let mut steps = MIGRATIONS.to_vec();
    steps.push(Migration::sql(schema::V15));
    steps
}

/// The ordered steps of a trigram database: the same list with the two
/// FTS-bearing steps (`V2`, `V5`) swapped for their trigram twins
/// ([`fts::V2_TRIGRAM`] / [`fts::V5_TRIGRAM`]).
pub(crate) fn migrations_trigram() -> Vec<Migration> {
    vec![
        Migration::sql(schema::V1),
        Migration::sql(fts::V2_TRIGRAM),
        Migration::sql(schema::V3),
        Migration::sql(schema::V4),
        Migration::sql(fts::V5_TRIGRAM),
        Migration::sql(schema::V6),
        Migration::sql(schema::V7),
        Migration::sql(schema::V8),
        Migration::sql(schema::V9),
        Migration::filled(schema::V10, backfill::msgid_refs),
        Migration::sql(schema::V11),
        Migration::sql(schema::V12),
        Migration::sql(schema::V13),
        Migration::sql(schema::V14),
        Migration::sql(schema::V15),
    ]
}

/// Brings `conn` up to the latest trigram schema version.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] if a step fails or the
/// database is newer than this build understands.
pub(crate) fn migrate_trigram(conn: &mut Connection) -> Result<SchemaStatus> {
    run(conn, &migrations_trigram())
}

/// Brings `conn` up to the latest default (porter) schema version — upstream's
/// steps plus the fork's own, one list so `user_version` stays in step with the
/// trigram build of the same release.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] if a step fails or the
/// database is newer than this build understands.
pub(crate) fn migrate_porter(conn: &mut Connection) -> Result<SchemaStatus> {
    run(conn, &migrations_porter())
}
