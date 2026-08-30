//! The host store seam: read-model queries against the engine's own store.
//!
//! P1 splits the engine's host surface in two. The facade's methods stay the
//! engine's *object* reads; the *aggregates* a product's surfaces are made of
//! (a conversation list, a round's counts) live in the host crate (`engine-host`),
//! because they are host read models the engine would otherwise grow one
//! hard-coded shape per UI. But a host cannot reach the store an `Engine` owns
//! without this seam: the field is private, as it should be — composition is the
//! facade's job, not a host's.
//!
//! [`Engine::host_store`] hands back the store read-only in practice: the pooled
//! `SqliteStore::read` path a host reaches through it runs against a
//! `query_only` connection, so a host read model can answer a page without
//! waiting on a committing sync and cannot write no matter what it tries. The
//! clock parameter stays unnamed (`impl Clock`) — it is this crate's private
//! `SystemClock`, an injection detail of the facade's composition that a host
//! has no reason to name to call `read`.

use engine_store::Clock;
use store_sqlite::SqliteStore;

use crate::Engine;

impl Engine {
    /// The engine's SQLite store, for host-built read models
    /// (`engine-host` and its P1 successors).
    ///
    /// Hosts read through [`SqliteStore::read`], never through the facade's
    /// internals; the writer path stays engine-only, so the engine remains the
    /// sole writer of its own store.
    #[must_use]
    pub fn host_store(&self) -> &SqliteStore<impl Clock> {
        &self.store
    }
}
