# Fork maintenance

This repository is [kylins-client](https://github.com/kenizhou/kylins-client)'s fork of
[allodia-eu/email-calendar-sync-engine](https://github.com/allodia-eu/email-calendar-sync-engine).
We cannot submit PRs upstream, so the fork carries our engine changes as a small,
disciplined patch series on top of `upstream/main`, rebased periodically.

## Upstream files

**The governing goal: modify upstream-tracked files as little as possible.**
Every diff against `upstream/main` must be one of exactly two shapes:

1. **A registration point** — the one `mod` / `use` / export line that wires a
   fork-owned file in, and that line is a row in the patch-series table below.
2. **An additive member a fork feature cannot exist without** — a trait method,
   an inherent-impl method, an appended schema step — each ledgered in the
   table with the feature that needs it.

Anything else — restructures, splits, re-wrapped docs, renumbering, reshaping
an existing fn's signature or body — is never done silently. When a fork change
would need shape (3), **re-home the feature into a fork-owned file instead**
and leave the upstream file byte-identical: the FTS tokenizer (2026-09-18) is
the precedent — the feature moved to `fts_migrations.rs`/`schema/fts.rs` and
`schema.rs`/`migrations.rs` went back to upstream verbatim. When upstream
supersedes a fork feature entirely, discard ours (see the merge-review section
below). Fork code lives in fork-owned files (`engine-host`, `fts_migrations.rs`,
`invite.rs`, `tokenizer_reconcile.rs`, …). When a shared file crosses the
500-line cap, or a merge conflict tempts a reshape of upstream's half, stop and
ask: a red length check on a shared file is the developer's call, not the
agent's.

## Remotes

- `origin` — this fork (the only push target).
- `upstream` — allodia-eu (fetch only; never push).

## Patch series

| Patch | Why |
|---|---|
| FTS tokenizer option — creation-time `porter unicode61` / `trigram` choice, `meta.fts_tokenizer` recording, mismatch refusal, CJK substring acceptance pins. **Re-homed 2026-09-18**: the upstream files are upstream's again — `schema.rs` is upstream verbatim + 4 registration lines (`mod fts; mod person;` + two `pub(crate) use`), `migrations.rs` is upstream verbatim + visibility widenings (`MIGRATIONS`/`Migration`/`run` pub(crate), `#[derive(Clone, Copy)]`) + `reconcile_normalizer_version` + an `#[allow(dead_code)]` on upstream's own `migrate` (this build routes both opens through the composer, whose lists append the fork's steps beyond upstream's fourteen). The feature lives in fork-owned files: `options.rs`, `tokenizer_reconcile.rs`, `tokenizer_tests.rs`, `schema/fts.rs` (the trigram DDL twins of V2/V5), `fts_migrations.rs` (the step-list composer: porter = upstream's `MIGRATIONS.to_vec()` + V15; trigram = the same with V2/V5 swapped for their trigram twins). `lib.rs`'s configure keeps the classify/refuse/record block and dispatches porter/trigram to the composer's two entry points; the `open_with`/`open_in_memory_with` constructors are fork additions, not upstream edits. A new upstream schema step is adopted by adding one line to `fts_migrations.rs` — zero upstream-file conflict (8 commits + 1 upstream-adaptation commit, then the re-home) | kylins needs CJK substring search; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §4 |
| EAS provider relocation — import of the Kylins Exchange ActiveSync protocol client at provenance `0dc611d` plus an engine-quality retrofit (edition 2024, workspace lints, 500-line module split, transport on `engine-tls`, env-gated live suite, offline mock-HTTP transport harness, guidance docs) (18 commits) | kylins P0: the engine needs an EAS provider; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §3. Protocol client only — the `Provider` trait impl follows in the next series (Plan C) |
| Rendered-source submission seam — submit the caller's own final MIME verbatim: tagged `SubmitPayload` outbox intent (`draft` / `rendered_source`), `Provider::submit_email_source` (SMTP; byte-capable transports only), `engine_sync::submit_mail_source`, `Engine::submit_mail_source` (5 commits) | kylins crypto pipeline needs to submit its own rendered+signed/encrypted MIME; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §5 |
| **DISCARDED 2026-09-18** (superseded by upstream #60): the fork's outbox drainer series — the tagged dispatch halves, `drain_mail_ops`/`drain_contact_ops`, the facade verbs — deleted in favour of upstream's queue + `drain_outbox`; see the "drainer series discarded" row | upstream landed the same feature; two drainers was the recurring merge pain |
| EAS `Provider` adapter — the engine's mail verbs over the protocol client: `engine-core` `SyncScope` EAS variants, FolderSync/Sync sync with in-stream SyncKey-invalidation recovery, ItemOperations message source with range reassembly, SendMail submission (draft + rendered source), keyword/move edits over the collection-key ledger, `Ping` `Watch` with heartbeat self-tuning, and the `engine-cli eas-sync` acceptance path (offline harness + live full/incremental; Sync/FolderSync status 111 retry-later classification) (8 commits) | kylins P0 exit — the engine drives an EAS account end to end; spec §3.2/§3.3/§8 of the same p0 spec; spike: kylins-client `docs/superpowers/research/p0-eas-trait-spike.md` |
| rusqlite 0.39.0, not 0.40.x — one-line workspace version pin | kylins P1 embeds the engine in the app binary beside sqlx 0.9 (its own kylins.db); cargo's `links = "sqlite3"` allows ONE libsqlite3-sys, and 0.39.0 + sqlx 0.9.0 both declare sys ^0.37.0 while rusqlite 0.40 forces ^0.38 (no released sqlx accepts that). Full suite green on 0.39. Drop when sqlx declares sys ≥ 0.38. Spec: kylins-client `docs/superpowers/specs/2026-08-28-p1-mail-cutover-design.md` Task 1 |
| P1 host seam — the `engine-host` crate for hosts embedding the engine in-process: `ThreadsRead` (thread-summary keyset read model), `EngineEvent`/`EventSink` (externally-tagged event contract), `run_account_round` (one driver round with its report), `warm_mail_bodies` (`BatchSourceFetch` batched body warm), `AttachmentVault` (content-addressed durable attachment store with digest self-heal); existing files are touched only at two ledgered registration points — one `mod host_access;` line in `engine-api/src/engine/mod.rs` and `pub` on the inherent `read` in `store-sqlite/src/lib.rs` (7 commits, fdd3b5a..261187f) | kylins P1 mail cutover — the app shell drives every mail verb through these seams and deletes its own sync stack; the registration-point list is the D12 contract for cheap upstream rebases. Spec: kylins-client `docs/superpowers/specs/2026-08-28-p1-mail-cutover-design.md` |
| engine-tls TOFU fingerprint pinning — `TlsPolicy::PinnedFingerprints` + `pinned_fingerprints(..)`: a custom verifier that accepts iff the presented end-entity SHA-256 is pinned, because webpki anchor semantics (`TlsPolicy::pinned`) validate issuers and can never verify a CA-signed leaf served alone (the on-prem lab shape — pin flows failed with UnknownIssuer). `TlsError::EmptyPinSet`; regression tests in `engine-tls/tests/pinned.rs` (1 commit) | kylins lab cutover surfaced that leaf pinning never worked end-to-end; the host stores per-account pins and needs the leaf form for servers that keep their root off-wire |
| EAS calendar read+write — `sync_calendars` (class-`Calendar` FolderSync) and `sync_events` (per-collection Sync: SyncKey cursor, `MoreAvailable` paging, status-3/12 discard-and-rebootstrap, Exchange-15.2 empty-bootstrap follow); props→`Event` conversion (TZI folded to the start-instant fixed offset, the raw blob kept in `extended`; structural recurrence incl. `FirstDayOfWeek`; exceptions→exclusion/patch overrides); `create_event`/`patch_event`/`delete_event` over Sync upsync (complete-document series rebuild, `Exceptions` emission) under per-collection SyncKey ledgers seeded by reads; one shared hierarchy-SyncKey ledger serving all three container scopes (7 commits, `65a4e3b..d1e9328`) | kylins P2 calendar cutover — the engine drives EAS calendars both ways; spec: kylins-client `docs/superpowers/specs/2026-09-04-p2-calendar-contacts-cutover-design.md` |
| EAS contacts read+write — the `ContactsProvider` suite on the adapter: type-9 address-book discovery through the shared hierarchy ledger, card sync with SyncKey-invalidation resync, `ContactCard` conversion, Add/Change/Delete upsync with explicit family clears for phones and addresses (a removal reaches the wire as an empty element, never as silence), photo refusal (the EAS `Picture` is dropped at parse time; no fetchable URI survives to address an ItemOperations round) (2 commits, `6fdd46e`+`38056ba`) | kylins P2 contacts cutover — the engine drives EAS contacts both ways and derives unified people; spec: same p2 spec |
| From-invite RSVP (the calendar-drain half of this series was **discarded 2026-09-18** with the drainer series) — `RsvpEventFromInvite` outbox intent (an invite answered from the message); `Engine::rsvp_invitation` + new `engine/invitation.rs` + the `uuid` dep — the invitation write reuses the upstream `engine/calendar_writes.rs` `reconciling` helper, widened `fn`→`pub(crate)` for it, and the `calendar_writes` test harness teaches its fake server `fetch_message_source` (with `serving`/`answers`) for the new `rsvp_invitation` scenario; `Provider::rsvp_event_from_invite` trait verb + default + `Box` forwarding (EAS: `MeetingResponse`, with the rescheduled-invitation stored-copy look-back); `engine-core` `UtcDateTime::checked_sub`. Upstream wiring touches: `mod invitation;` + one module-doc sentence in `engine-api/src/engine/mod.rs`; `mod invite;` + the invite export in `outbox/mod.rs`; the engine-provider `tests_rsvp_invite` split (a `#[cfg(test)]` mod line in `lib.rs`, two `tests.rs` helpers widened `pub(super)`); the engine-sync test mods (FakeMail gains the from-invite verb; `stored` widened `pub(super)` in `tests/calendar_write.rs`). The `provider.rs` edit forced a restructure: the fork-owned `submit_email_source` doc contract moved to `submit.rs` module docs, ~14 upstream doc lines re-wrapped, one module-header phrase changed (7 commits, `ad29da7..5041826`) | kylins P2 — a calendar write must survive a crash like mail does, and an invitation is answered from the message through the outbox; spec: same p2 spec |
| **DISCARDED 2026-09-18** with the drainer series: `Store::release_pending_op` + Mem/SQLite impls + contract cases deleted — upstream's targeted claims answer the same starvation without a release verb. The `reconcile_normalizer_version` relocation lib.rs→migrations.rs outlived it | upstream's targeted claim owns the fix now | kylins P2 `run_pim_round`'s fixed calendar-first drain order permanently starved contact writes under scope-blind claims; spec: same p2 spec, task 7b |
| PIM host round + grid + CLI PIM acceptance — no new registration points outside the upstream `engine-cli` crate's split wiring: `engine-host` (the fork's own P1-series crate) gains `run_pim_round`/`PimRoundReport`, `CalendarChanged`/`ContactsChanged` events, and `CalendarGridRead`/`CalendarGridPage` (zone-drift-aware window maintenance; `run_pim_round` now ends in one mail-only `drain_outbox` pass — calendar/contact ops stay queued until the drain port); `engine-cli eas-sync --kind calendar|contacts` (per-collection adapters through the engine's own `sync_calendar`/`sync_contacts`, occurrence summary, `--create` ServerId-backfill round-trip; the flag parser split to `flags.rs` at the cap — the upstream-crate touches that implies: `cli.rs` sheds the parser, `lib.rs` gains the `mod flags;`/`mod eas_pim;` wiring and the rework that `search_calendar` now reads the account's actual event scopes, the `search_mail` rule); offline twins in the transport harness, `EAS_LIVE_*`-gated live twins; the fork-owned EAS guidance doc (`docs/agent-guidance/eas.md`) brought current across the series' calendar/contacts/RSVP verbs and PIM CLI arms (3 commits, `0bd8411`+`dad959e`+`de60cd9`) | kylins P2 close-out — the host scheduler drives one PIM round per tick and the CLI is the acceptance path; spec: same p2 spec |
| **DISCARDED 2026-09-18** with the drainer series: `settle_outcome`/release-on-retryable deleted; retryable failures park behind upstream's bounded backoff (30s doubling to a 30min cap, MAX_ATTEMPTS=8, hurriable via `retry_pending_op_now`). **Check the kylins T13 hint text against the 8-attempt bound**: a NeedsResync write cold through 8 attempts settles Failed where the fork retried forever | upstream owns the retry cadence; one queue, one answer | kylins T13 live acceptance surfaced it: the one-shot CLI create refused NeedsResync was recorded terminal, so the error hint's "the outbox retries the write after it" never happened — found by driving the real account; spec: same p2 spec, task 13 |
| Contact create: `ContactFieldSet::from_card` requests `Kind` only for a non-default kind — the unconditional seed refused EVERY contact create over the EAS destination (whose `supported_fields` deliberately excludes `Kind`: the EAS contacts class is individual-only with no kind element) with "does not support fields {Kind}" before the wire, the shell's product create path included; the exhaustive-population fixture answers with `ContactKind::Organization` to keep covering the request path (1 commit, `9b70823`) | kylins dev-account E2E probe (`kylins.client.backend/tests/e2e_dev_accounts/contacts.rs`) surfaced it live: the product-path create_contact over felixzhou's EAS account was refused before any network write — found by driving the real account |
| Unread aggregates in `ThreadsRead` — `unread_counts_by_label` (per mailbox label, the count of distinct threads with at least one unread member: unread is `page()`'s own `(flags & (SEEN\|DRAFT)) = 0`, a thread's labels are every mailbox any member is filed in, counted once per label) + `unread_thread_total` (those threads once each — the tray total a per-label sum cannot give); one grouped statement each over the same `message`/`membership` rows `page()` reads, every join an index seek (`message_account_thread`, the membership primary key); the equivalence-pin test folds `threads()` pages at size 1 and asserts the aggregates equal the walk (1 commit, `97e7c6d`) | kylins folder-pane unread badges walked every thread page per account per sync — the 30.7k-thread EAS dev account spent 6.3 s per refresh on the fold; the aggregate answers the same account in tens of ms (probe: `kylins.client.backend/tests/e2e_dev_accounts` read-latency) |
| `person.display_name` nullable (schema v14 — renumbered from v13 when upstream's `399b951` landed its own v13, the `pending_op_held_resource` index) — the v7 `person` table's `NOT NULL` contradicted the model's deliberate `Option<String>` (`Person::display_name`: a card with no name and no address stays `None`; naming the nameless is the host's presentation call), so every contacts collection holding such a card faulted its whole people replacement (`NOT NULL constraint failed: person.display_name`); v14 rebuilds the table copy-forward (no SQL reads the column, nothing references `person` across tables), plus the nameless-person contract case (1 commit) | kylins hotmail dev account: 2 of 6 Graph contact collections faulted every PIM round from day one — found in the app log while diagnosing the folder-pane refresh reports (2026-09-15 session); registered as ER-13 in kylins `docs/engine-enhancement-requests.md` |

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

## Every upstream merge: the overlap and adoption review

Before resolving conflicts in any upstream merge or rebase, survey what upstream
brought (`git log --oneline <old-upstream>..upstream/main`) and act on two
questions, in this order. Skipping this review is how a fork ends up maintaining
two answers to one question — the 2026-09-18 outbox-drainer merge was painful
almost entirely because it was skipped for months.

1. **Overlap: did upstream implement or fix something we also carry?** Check
   every new upstream commit against the patch-series table above (the table is
   the index of what we carry). When upstream's implementation supersedes ours,
   **discard the fork's version and migrate onto upstream's** — delete our
   parallel code, adapt our callers, re-home what must stay into fork-owned
   files, and mark the table row `DISCARDED (superseded by upstream)` with the
   behaviour differences a host must know about. Two drainers, two retry
   cadences, or two payload shapes in one crate is never the steady state.
2. **Adoption: did upstream land a feature we do not carry?** For each one,
   assess whether kylins can adopt it (what it would replace, what it needs
   from the host). Record the assessment — a row here, or an entry in kylins
   `docs/engine-enhancement-requests.md` — even when the answer is "not now";
   an unrecorded assessment has to be re-done from scratch next merge. Do not
   implement an adoption speculatively inside the engine fork.

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
| SMTP AUTH mechanism negotiation — `converse` reads the EHLO response's `AUTH` mechanisms and speaks `PLAIN` where advertised, the two-step `AUTH LOGIN` exchange otherwise, refusing before any credential material moves when the server offers neither; the module docs record the Exchange shape (`AUTH GSSAPI NTLM LOGIN`, no PLAIN — a PLAIN-only client drew `504 5.7.4`) (1 commit, `df61c73`) | kylins T13 IMAP/SMTP live acceptance against the on-prem Exchange 2016 lab surfaced the PLAIN-only client as unusable there; spec: kylins-client `docs/superpowers/specs/2026-09-04-p2-calendar-contacts-cutover-design.md`, task 13 |
| Graph mail-folder listing BFS — `/mailFolders` returns only the TOP level, so the listing now traverses `/mailFolders/{id}/childFolders` per discovered folder (the contacts listing's own rule) and the store finally carries subfolders with their parents; nested-fixture regression test (1 commit) | kylins folder pane showed a flat top level with no children — the engine's `mailboxes()` had no subfolder objects to serve (2026-09-16 report); registered as ER-15 in kylins `docs/engine-enhancement-requests.md`; upstream `mail-calendar` shares the gap |
| `Mailbox.total_count` + Graph `totalItemCount` mapping — the server-side TOTAL rides the default `mailFolder` projection beside `unreadItemCount`, `#[serde(default)]` so stored payloads without it deserialize (1 commit) | kylins' Drafts folder badge needs the TOTAL (a draft is never 'unread'); registered as ER-16 in kylins `docs/engine-enhancement-requests.md` |
| Upstream merge (b984884, 2026-09-18) + **drainer series discarded**: upstream landed its own outbox queue + drainer (issue #60), and the fork deleted its parallel series rather than maintain two answers — gone: `outbox/drain_ops.rs`/`execute.rs`, `drain_{mail,contact,calendar}_ops`, `Engine` drain facade (`engine/drain.rs`), `Store::release_pending_op` + impls + contract cases, `settle_outcome` release-on-retryable, the calendar/contact drain tests. Kept: the tagged `OutboxIntent` payload envelope (load-bearing for the fork-only intents — rendered-source submit, from-invite RSVP), `OutboxIntent::pending_op_kind()`, dual-generation decode in `drain_outbox`/`queued_draft` (upstream-shaped rows and this build's rows both decode), `run_pim_round`/`run_account_round` now ending in one mail-only `drain_outbox` pass. Fork's `person.display_name` step renumbered v14→v15 (upstream v14 = queue columns), in the fork-owned `schema/person.rs`. Cap-driven fork-owned splits: `store-sqlite/src/tokenizer_tests.rs`, `migrations/tests.rs`, `engine-api/tests/sync/source_submits.rs`, `engine-sync/src/tests/fixtures.rs`. Cap now green everywhere this series touched | upstream moved fast on the same feature; **owed follow-up**: port calendar/contact drain onto the upstream queue (kinds + targeted claims), and check kylins T13 hint text against the 8-attempt bound |
| **PIM drain onto the upstream queue** — the owed follow-up above, built: fork-owned `engine-sync` `outbox/drain_pim.rs` (the two drain loops: list → kind filter → targeted `claim_pending_op` → replay → `mark_pending_op`/`record_failure`, the store owning park-vs-settle and the `MAX_ATTEMPTS = 8` bound; `InFlight` admitted so an expired lease's crash orphan is reclaimed) + `outbox/drain_replay.rs` (tagged-intent decode with kind-column cross-check, base re-reads via `object_payload`, the surviving `pub(crate)` `execute_*` halves; gone-base patch/RSVP → `Conflict`, gone-target delete → success, undecodable → terminal poison) + facade verbs `Engine::drain_contact_ops`/`drain_calendar_ops` in fork-owned `engine-api` `engine/drain_pim.rs` (the names/signatures the kylins shell's `pim_many.rs` already calls); `engine-host`'s `run_pim_round` regains its per-scope calendar+contact drains and the two-field `PimRoundReport`. Registration points only on upstream files: `mod drain_pim; mod drain_replay;` + one `pub use` in `engine-sync/src/outbox/mod.rs`, the `drain_calendar_ops, drain_contact_ops` pair in `engine-sync/src/lib.rs`'s outbox re-export, `mod drain_pim;` in `engine-api/src/engine/mod.rs`, `mod drain_pim;` in `engine-sync/src/tests/mod.rs`; tests fork-owned at `engine-sync/src/tests/drain_pim{.rs,/calendar.rs}` incl. the NeedsResync-settles pin (3 commits, `87e6d54..9f4ad0b`) | upstream's drainer is mail-only by design (`drain.rs` "Mail only, so far") and the replay needs the `pub(crate)` execute halves + the store's claim/mark — the orphan rule leaves the shell no seam; registered as ER-17 in kylins `docs/engine-enhancement-requests.md`; the T13 hint half of the follow-up resolved in `docs/agent-guidance/store-and-sync.md` (`NeedsResync` settles on first failure — `is_retryable` is `Retryable\|RateLimited` only) |


