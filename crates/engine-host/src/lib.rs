//! `engine-host` — the read models a host builds over an engine it does not own.
//!
//! The engine's own facade (`engine-api`) answers *object* reads: one message, one
//! thread's members, one body. A product's primary surfaces are *aggregates*: a
//! conversation list is one row per thread — counts, flags, the newest member's
//! header — and forcing those through per-object reads would rank the whole account
//! in the caller and open a payload per row. Those aggregates belong on the store,
//! where one grouped statement answers a page at the cost of the page.
//!
//! This crate is where they accumulate. [`ThreadsRead`] is the first: the
//! conversation summary list behind a mail client's list pane, read straight from
//! the engine's SQLite `message` table through the `Engine::host_store` seam that
//! P1 opened for exactly this purpose. Later P1 slices (events, round summaries,
//! body warming, attachments) add their read models beside it, against the same
//! seam, so the host's engine-side surface stays one crate.
//!
//! The seam is deliberately narrow. Host code here reads through the store's
//! pooled `read` path (a `query_only` connection off the async runtime) and never
//! writes: the engine stays the sole writer, so no host read can race a sync into
//! a torn aggregate. Types stay engine-shaped (engine-core ids in, plain data out)
//! so nothing here re-models what the engine already models.

mod events;
mod round;
mod threads;
mod warm;

pub use events::{AccountState, CollectingSink, EngineEvent, EventSink};
pub use round::{RoundReport, run_account_round};
pub use threads::{ThreadCursor, ThreadSummary, ThreadsOptions, ThreadsPage, ThreadsRead};
pub use warm::{BatchSourceFetch, WarmReport, sequential_sources, warm_mail_bodies};
