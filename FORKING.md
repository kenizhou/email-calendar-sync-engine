# Fork maintenance

This repository is [kylins-client](https://github.com/kenizhou/kylins-client)'s fork of
[allodia-eu/email-calendar-sync-engine](https://github.com/allodia-eu/email-calendar-sync-engine).
We cannot submit PRs upstream, so the fork carries our engine changes as a small,
disciplined patch series on top of `upstream/main`, rebased periodically.

## Remotes

- `origin` — this fork (the only push target).
- `upstream` — allodia-eu (fetch only; never push).

## Patch series

| Patch | Why |
|---|---|
| FTS tokenizer option — creation-time `porter unicode61` / `trigram` choice, `meta.fts_tokenizer` recording, mismatch refusal, CJK substring acceptance pins (8 commits + 1 upstream-adaptation commit) | kylins needs CJK substring search; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §4 |
| EAS provider relocation — import of the Kylins Exchange ActiveSync protocol client at provenance `0dc611d` plus an engine-quality retrofit (edition 2024, workspace lints, 500-line module split, transport on `engine-tls`, env-gated live suite, offline mock-HTTP transport harness, guidance docs) (18 commits) | kylins P0: the engine needs an EAS provider; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §3. Protocol client only — the `Provider` trait impl follows in the next series (Plan C) |
| Rendered-source submission seam — submit the caller's own final MIME verbatim: tagged `SubmitPayload` outbox intent (`draft` / `rendered_source`), `Provider::submit_email_source` (SMTP; byte-capable transports only), `engine_sync::submit_mail_source`, `Engine::submit_mail_source` (5 commits) | kylins crypto pipeline needs to submit its own rendered+signed/encrypted MIME; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §5 |
| Outbox drainer — recovery of crash-orphaned and unstarted ops: tagged `OutboxIntent` payloads, split enqueue/execute halves, `engine_sync::{drain_mail_ops, drain_contact_ops}`, `Engine::drain_mail_ops` / `drain_contact_ops` (4 commits) | engine issue #60 Phase 1 — mail+contacts drainer so a crash never strands a recorded write; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §6 |
| EAS `Provider` adapter — the engine's mail verbs over the protocol client: `engine-core` `SyncScope` EAS variants, FolderSync/Sync sync with in-stream SyncKey-invalidation recovery, ItemOperations message source with range reassembly, SendMail submission (draft + rendered source), keyword/move edits over the collection-key ledger, `Ping` `Watch` with heartbeat self-tuning, and the `engine-cli eas-sync` acceptance path (offline harness + live full/incremental; Sync/FolderSync status 111 retry-later classification) (8 commits) | kylins P0 exit — the engine drives an EAS account end to end; spec §3.2/§3.3/§8 of the same p0 spec; spike: kylins-client `docs/superpowers/research/p0-eas-trait-spike.md` |
| rusqlite 0.39.0, not 0.40.x — one-line workspace version pin | kylins P1 embeds the engine in the app binary beside sqlx 0.9 (its own kylins.db); cargo's `links = "sqlite3"` allows ONE libsqlite3-sys, and 0.39.0 + sqlx 0.9.0 both declare sys ^0.37.0 while rusqlite 0.40 forces ^0.38 (no released sqlx accepts that). Full suite green on 0.39. Drop when sqlx declares sys ≥ 0.38. Spec: kylins-client `docs/superpowers/specs/2026-08-28-p1-mail-cutover-design.md` Task 1 |
| P1 host seam — the `engine-host` crate for hosts embedding the engine in-process: `ThreadsRead` (thread-summary keyset read model), `EngineEvent`/`EventSink` (externally-tagged event contract), `run_account_round` (one driver round with its report), `warm_mail_bodies` (`BatchSourceFetch` batched body warm), `AttachmentVault` (content-addressed durable attachment store with digest self-heal); existing files are touched only at two ledgered registration points — one `mod host_access;` line in `engine-api/src/engine/mod.rs` and `pub` on the inherent `read` in `store-sqlite/src/lib.rs` (7 commits, fdd3b5a..261187f) | kylins P1 mail cutover — the app shell drives every mail verb through these seams and deletes its own sync stack; the registration-point list is the D12 contract for cheap upstream rebases. Spec: kylins-client `docs/superpowers/specs/2026-08-28-p1-mail-cutover-design.md` |
| engine-tls TOFU fingerprint pinning — `TlsPolicy::PinnedFingerprints` + `pinned_fingerprints(..)`: a custom verifier that accepts iff the presented end-entity SHA-256 is pinned, because webpki anchor semantics (`TlsPolicy::pinned`) validate issuers and can never verify a CA-signed leaf served alone (the on-prem lab shape — pin flows failed with UnknownIssuer). `TlsError::EmptyPinSet`; regression tests in `engine-tls/tests/pinned.rs` (1 commit) | kylins lab cutover surfaced that leaf pinning never worked end-to-end; the host stores per-account pins and needs the leaf form for servers that keep their root off-wire |
| EAS calendar read+write — `sync_calendars` (class-`Calendar` FolderSync) and `sync_events` (per-collection Sync: SyncKey cursor, `MoreAvailable` paging, status-3/12 discard-and-rebootstrap, Exchange-15.2 empty-bootstrap follow); props→`Event` conversion (TZI folded to the start-instant fixed offset, the raw blob kept in `extended`; structural recurrence incl. `FirstDayOfWeek`; exceptions→exclusion/patch overrides); `create_event`/`patch_event`/`delete_event` over Sync upsync (complete-document series rebuild, `Exceptions` emission) under per-collection SyncKey ledgers seeded by reads; one shared hierarchy-SyncKey ledger serving all three container scopes (7 commits, `65a4e3b..d1e9328`) | kylins P2 calendar cutover — the engine drives EAS calendars both ways; spec: kylins-client `docs/superpowers/specs/2026-09-04-p2-calendar-contacts-cutover-design.md` |
| EAS contacts read+write — the `ContactsProvider` suite on the adapter: type-9 address-book discovery through the shared hierarchy ledger, card sync with SyncKey-invalidation resync, `ContactCard` conversion, Add/Change/Delete upsync with explicit family clears for phones and addresses (a removal reaches the wire as an empty element, never as silence), photo refusal (the EAS `Picture` is dropped at parse time; no fetchable URI survives to address an ItemOperations round) (2 commits, `6fdd46e`+`38056ba`) | kylins P2 contacts cutover — the engine drives EAS contacts both ways and derives unified people; spec: same p2 spec |
| Calendar outbox drain + from-invite RSVP — `engine_sync::drain_calendar_ops` (execution halves in the upstream `outbox/calendar.rs`, the fork-added `execute.rs`/`drain.rs` dispatchers, `lib.rs` export); `RsvpEventFromInvite` outbox intent (an invite answered from the message, replayable); `Engine::drain_calendar_ops` facade; `Engine::rsvp_invitation` + new `engine/invitation.rs` + the `uuid` dep — the invitation write reuses the upstream `engine/calendar_writes.rs` `reconciling` helper, widened `fn`→`pub(crate)` for it, and the `calendar_writes` test harness teaches its fake server `fetch_message_source` (with `serving`/`answers`) for the new `rsvp_invitation` scenario; `Provider::rsvp_event_from_invite` trait verb + default + `Box` forwarding (EAS: `MeetingResponse`, with the rescheduled-invitation stored-copy look-back); `engine-core` `UtcDateTime::checked_sub`. Upstream wiring touches: `mod invitation;` + one module-doc sentence in `engine-api/src/engine/mod.rs`; `mod invite;` + the calendar drain/invite exports in `outbox/mod.rs`; the engine-provider `tests_rsvp_invite` split (a `#[cfg(test)]` mod line in `lib.rs`, two `tests.rs` helpers widened `pub(super)`); the engine-sync test mods (FakeMail gains the from-invite verb; `stored` widened `pub(super)` in `tests/calendar_write.rs`). The `provider.rs` edit forced a restructure: the fork-owned `submit_email_source` doc contract moved to `submit.rs` module docs, ~14 upstream doc lines re-wrapped, one module-header phrase changed (7 commits, `ad29da7..5041826`) | kylins P2 — a calendar write must survive a crash like mail does, and an invitation is answered from the message through the outbox; spec: same p2 spec |
| Outbox skip release — `Store::release_pending_op` (a claimed op returns to `Pending` under its own lease, fencing token bumped, `InFlight`-only guard) + Mem/SQLite impls + two contract cases wired into `run_all`; the drainers release on `ExecuteFailure::OutOfScope` so scope-blind claims cannot starve a foreign drain (any drain ordering is safe); `store-sqlite` `reconcile_normalizer_version` relocated lib.rs→migrations.rs to hold the 500-line cap, three test call sites repointed (2 commits, `9e5c527`+`26d2f92`) | kylins P2 `run_pim_round`'s fixed calendar-first drain order permanently starved contact writes under scope-blind claims; spec: same p2 spec, task 7b |
| PIM host round + grid + CLI PIM acceptance — no new registration points outside the upstream `engine-cli` crate's split wiring: `engine-host` (the fork's own P1-series crate) gains `run_pim_round`/`PimRoundReport`, `CalendarChanged`/`ContactsChanged` events, and `CalendarGridRead`/`CalendarGridPage` (zone-drift-aware window maintenance, calendar-first drains safe under the release above); `engine-cli eas-sync --kind calendar|contacts` (per-collection adapters through the engine's own `sync_calendar`/`sync_contacts`, occurrence summary, `--create` ServerId-backfill round-trip; the flag parser split to `flags.rs` at the cap — the upstream-crate touches that implies: `cli.rs` sheds the parser, `lib.rs` gains the `mod flags;`/`mod eas_pim;` wiring and the rework that `search_calendar` now reads the account's actual event scopes, the `search_mail` rule); offline twins in the transport harness, `EAS_LIVE_*`-gated live twins; the fork-owned EAS guidance doc (`docs/agent-guidance/eas.md`) brought current across the series' calendar/contacts/RSVP verbs and PIM CLI arms (3 commits, `0bd8411`+`dad959e`+`de60cd9`) | kylins P2 close-out — the host scheduler drives one PIM round per tick and the CLI is the acceptance path; spec: same p2 spec |

Every fork-only change must appear here with its motivation, or it will surprise
the next rebase.

## Rebase procedure

1. `git fetch upstream`
2. Rebase the patch series onto `upstream/main` (`git rebase upstream/main` on
   the branch holding it, or on fork `main` if the series lives there).
   Conflicts are expected where upstream edits files we restructured — resolve
   by keeping our shape and folding in upstream's new content (see the
   "Adapt an upstream test to the tokenizer-taking migrate" commit for the
   shape of adapting new upstream call sites to changed fork signatures).
3. Run the full verification gate from `AGENTS.md` before pushing to `origin`.
4. Update the patch-series table above if the series changed.

Cadence: whenever upstream lands something we want (the engine moves fast —
blob GC, delta bounds, occurrence edits all arrived within weeks); at minimum
monthly. Force-pushing `origin/main` after a rebase is expected in a fork;
upstream history is never rewritten.

## Standards

All engine-repo discipline still applies here (CI gate, 500-line cap, fixture
identifier rules, real-server evidence). Cheap rebases are the return on that
investment, and a clean series stays upstreamable as one PR if the situation
ever changes.

## Line endings on Windows checkouts

`core.autocrlf` must be `input` (or `false`) in this clone: the pinned nightly
rustfmt enforces LF, and an `autocrlf=true` checkout fails `cargo fmt --check`
repo-wide. Two fixture files legitimately contain CRLF bytes
(`crates/engine-api/tests/fixtures/stalwart-invitation.eml`,
`crates/provider-caldav/tests/fixtures/sync-initial.xml`) — do not run
`git add --renormalize` across them.
