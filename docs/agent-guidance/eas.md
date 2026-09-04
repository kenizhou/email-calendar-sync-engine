# EAS (Exchange ActiveSync) Client Guidance

> **Protocol client landed; adapter standing up verb by verb — connection
> facts + scopes + FolderSync (`sync_mailboxes`) + Sync class Email
> (`stream_email`) are in; the mail read domain is complete and the `mail`
> capability bit is on; the calendar read domain (`sync_calendars` +
> `sync_events`, class-Calendar Sync) and the calendar write verbs
> (`create_event`/`patch_event`/`delete_event` over Sync Commands upsync)
> are in on calendar-bound adapters (`put_event` refused — EAS's update is
> a field-level Change, not a document PUT).** The per-verb verdicts below come from the trait-shape spike
> (Plan B Task 3, 2026-08-24) and are stable. The relocation series has since
> brought the crate to engine quality — edition 2024, workspace lints, module
> split under the 500-line cap, the engine-tls transport, normalized live gating —
> and `EasAdapter` (`src/adapter/`) implements `engine_provider::Provider`'s
> `connection_info`, the EAS scope overrides, `sync_mailboxes` (FolderSync),
> and `stream_email` (Sync class Email), advertising exactly the capabilities
> whose verbs have landed (the verb ladder — a bit never precedes its verb).
> Everything marked *fork decision* or *P2* stays unimplemented
> until Plan C/P2 says otherwise. Update this file as the adapter's verbs land —
> it is intended to become authoritative for `provider-eas`, peer of
> `imap-smtp.md` / `graph.md`.

This document covers the **Exchange ActiveSync 16.1 (negotiated down to 12.0) mail +
calendar + contacts provider** — the `provider-eas` crate imported from the Kylins
client (upstream commit `0dc611d`, engine import commit `d961954`) and since
retrofitted to engine standards: a protocol client plus its `Provider` adapter
skeleton. Read it alongside `providers.md` (the Provider
Contract), `store-and-sync.md` (scopes/cursors/apply), `tls.md` (trust policy), and
the spike findings in the Kylins repo
(`docs/superpowers/research/p0-eas-trait-spike.md`), which carries the full evidence
table behind every verdict here.

## The crate

- **`provider-eas`** — a hand-rolled EAS client over `reqwest` POSTs of WBXML
  documents, imported from Kylins and split to the engine's 500-line cap.
  Layers: `wbxml/` (codec: parser/serializer, code pages, token tables),
  `commands/` (per-command request builders + response parsers), `client/` (the
  transport: headers, retry layers, per-command typed methods), `status.rs`
  (status classification), `provision.rs`, `auth.rs`, `autodiscover/`, `types/`
  (wire-shaped request/result structs), plus `calendar/` / `calendar_write/` /
  `contacts/` (class-typed conversion) and `meeting_uid.rs` / `multipart.rs`.
  `adapter/` holds the `Provider` seam: `EasAdapter`, bound to one folder per the
  IMAP/Graph precedent, with `negotiate()` as the connection-time OPTIONS
  exchange (protocol version negotiated, applied to the client's
  `MS-ASProtocolVersion`, held adapter-side — never `ConnectionInfo`).
- The adapter implements `connection_info`, the mail scope overrides, and the
  read verbs: `sync_mailboxes` (FolderSync — `adapter/mailboxes.rs`) and
  `stream_email` (Sync class Email — `adapter/email.rs`). The
  client sits behind a `tokio::sync::Mutex` (the verb lock): command methods
  rotate session state in place (hierarchy key, policy key, adopted URL), so
  verbs serialize onto one client — the IMAP connection-lock precedent — while
  `connection_info` reads the `ObservedHttpVersion` funnel through an `Arc`
  handle (`EasClient::http_version_handle`), lock-free. An email stream holds
  the lock for its whole pass, like IMAP's held connection guard.
  Capabilities follow the verb ladder: `mail` is on (containers *and*
  messages are both live — the whole domain the bit names); every other bit
  (`mail_writes`, `message_source`, `submission`, calendar/contacts) stays
  off until its verbs land.
  The spike's job was to decide whether the verbs *can* map without engine API
  changes. Answer: **one required engine change (new
  `SyncScope` variants — since landed, see the fork-decision records), everything
  else maps within the existing trait surface.**

## Protocol overview

- **EAS 16.1** ([MS-ASHTTP] + family), negotiated down per server via the `OPTIONS`
  round: the server advertises `MS-ASProtocolVersions` / `MS-ASProtocolCommands`
  headers, and `pick_protocol_version` chooses the highest mutually known
  (`client/options.rs`, `options()` / `pick_protocol_version`). Wire format is WBXML
  (binary XML with per-namespace code pages) for every command body except
  SendMail's `<Mime>`, which rides as a WBXML OPAQUE blob so binary MIME survives.
- Every request is `POST <endpoint>?Cmd=<Command>&User=<user>&DeviceId=<id>`
  with `MS-ASProtocolVersion`, auth (Basic or OAuth bearer via `EasAuth`), and the
  device identity headers. HTTP 200 with an **empty body is the success shape**
  for the SendMail family; for Sync it means "no changes" and must echo the
  request's sync key.
- **Auth**: Basic (`DOMAIN\user`) or OAuth (`auth.rs`, token refresh on 401 is one
  of the transport retry layers). **Autodiscover** (`autodiscover/`) resolves the
  endpoint; an HTTP 451 `X-MS-Location` redirect is followed in-transport
  (hop-capped) and the adopted URL surfaces for the host to persist.

## Command surface the crate covers

| Command | Where | Purpose (engine verb it feeds) |
| --- | --- | --- |
| `OPTIONS` | `client/options.rs` | version/command negotiation at connect |
| `Provision` | `provision.rs`, `client/` retry layers | policy handshake; policy key rides `X-MS-PolicyKey` on every command |
| `Settings` (DeviceInformation / UserInformation / OOF / device password) | `client/settings.rs` | bootstrap facts, OOF (no engine verb yet) |
| `FolderSync` / `FolderCreate` / `FolderUpdate` / `FolderDelete` | `client/sync.rs`, `commands/folder_sync.rs`, `commands/folder_ops.rs` | container sync (`sync_mailboxes`) + folder writes |
| `Sync` (down) | `client/sync.rs::sync`, `commands/sync/` | per-collection item sync, classes Email/Calendar/Contacts (`stream_email`, `sync_events`, `sync_contacts`) |
| `Sync` (up: Commands Add/Change/Delete) | `client/sync.rs::sync_changes`, `commands/sync/` | client-side mutations (flags, calendar/contact writes) |
| `SendMail` / `SmartForward` / `SmartReply` | `client/compose.rs`, `commands/send.rs` | submission (`submit_email`); SmartForward degrades to SendMail on rejection |
| `MoveItems` | `client/items.rs::move_items` | per-message move (`MailEdit::MoveTo`) |
| `ItemOperations` (Fetch / EmptyFolderContents / Move-conversation) | `client/items.rs`, `commands/item_operations/` | body/MIME/attachment fetch (`fetch_message_source`), destructive extras (no engine verbs) |
| `MeetingResponse` | `client/items.rs`, `commands/meeting.rs` | invitation answer (`rsvp_event_from_invite`) |
| `Ping` | `client/items.rs`, `commands/ping.rs` | push (`Watch`) |
| `GetItemEstimate` / `Search` / `ResolveRecipients` / `ValidateCert` | `client/items.rs` / `client/settings.rs` | counts, GAL/mailbox search, cert validation (no engine verbs yet) |

## Capability / quirk notes

- **OPTIONS version negotiation** happens at account setup; the chosen version is
  then a property of the client (it rides `MS-ASProtocolVersion` on every request).
  `ConnectionInfo` reports TLS/HTTP versions only; the EAS protocol version stays
  adapter-internal (hosts do not branch on it — `providers.md`).
- **Provision/policy**: servers may demand the two-phase Provision handshake at any
  time (HTTP 449, or in-body Common/FolderSync status 142–144). The transport
  retries once after re-provisioning (`client/transport.rs::send_command_ex`); `RemoteWipe`
  is surfaced as a permanent error, never executed.
- **Status classification** (`status.rs`): one `RecoveryAction` classifier per
  command family. Engine mapping: `ResetSyncKey` (Sync 3, FolderSync 9) →
  `ProviderError::needs_resync`; `RetryTransient` (Sync 5/16, HTTP 429/5xx) →
  `retryable`/`rate_limited`; `SurfaceAuth` → `authentication`; `RunFolderSync`
  (Sync 12, Ping 7) → needs-resync-or-internal-FolderSync; `RetryProvision` /
  `RefreshToken` / `FollowRedirect` are handled inside the transport and never
  surface. This is a clean 1:1 with `FailureClass`. The family a status came
  from decides its meaning, so the Sync family surfaces through its own
  `EasError::SyncStatus` variant (classified via the Sync table), while the
  family-untagged `CommandStatus` serves the families without adapter slices
  yet. Verified against the [MS-ASCMD] Status (Sync) table (2026-08-27):
  3 = invalid sync key (MUST re-bootstrap from "0"), 5/16 = transient
  retry, **6 = "error in client/server conversion … not a transient
  condition" → permanent** (an earlier table treated 6 as `Ok` — corrected;
  per-item upsync 6s will need skip-item handling when `edit_mail` lands).
- **Sync key mechanics** (the load-bearing quirk set):
  - A collection sync key is an opaque server cursor; `"0"` bootstraps. The
    bootstrap round itself returns **no items** on some servers (Exchange 15.2) —
    the drain loop must follow the rotated key once (the Kylins
    `should_follow_empty_bootstrap` rule, ported into `adapter/email.rs`).
  - Sync key invalid (status 3) = the EAS `cannotCalculateChanges`; FolderSync 9
    is the same for the hierarchy. Restart from `"0"` as a reconcile snapshot.
  - Each Sync round rotates the key, and `MoreAvailable` says whether more rounds
    remain — so **every round is a safe checkpoint** (EAS is more resumable
    mid-pass than JMAP/Graph, not less).
  - `WindowSize` (items per round): the adapter maps the engine's `fetch_batch`
    knob directly (`0` → the 512 Kylins/Android cap); ≤200 upsync commands per
    request are chunked and key-threaded automatically
    (`client/sync.rs::sync_changes`).
  - `FilterType` wire values are an **enum code table, not days** ([MS-ASCMD]
    §2.2.3.68: 1=1 day, 2=3 days, 3=1 week, 4=2 weeks, 5=1 month, 6=3 months,
    7=6 months, 0=no filter — correcting this file's earlier "days ladder"
    note). `stream_email` currently sends 0 (the Kylins client's live-proven
    shape): the window's bound holds at apply time (`SyncWindow::admits`
    filters every additive chunk) with the host's prune pass as the backstop
    for a Reconcile recovery's coarser enumeration; mapping `since` to the
    smallest covering code is a pending adapter slice.
- **MoveItems' status table is inverted**: 3 is success (with `DstMsgId`), 1 is
  invalid source. A move re-keys the message (new ServerId), exactly like an IMAP
  move re-keys by UID — `MailEditReceipt::message_key` keeps the source key and
  the destination reconciles on the next sync.
- **ClientId dedup**: SendMail's `<ClientId>` (≤40 chars, enforced — Exchange 15.2
  rejects over-cap with in-body status 103) is derived from the draft's
  `Message-ID` so a lost-response retry does not double-send.
- **The collection-key ledger** (write-path): an EAS SyncKey is per-collection
  server state every Sync command must thread — but the trait's write seam
  (`edit_mail`) carries no cursor, and the engine's cursor stays authoritative
  for what has been *delivered*. The adapter therefore keeps a one-key ledger
  for its bound folder: a completed `stream_email` pass records its final key
  (the same value the engine persists — one fact, two holders), and a
  `SetKeywords` edit rides that key and records its rotation. Resuming the
  next pass from a rotation is lossless because the upsync request sends no
  `GetChanges` (invalid in 16.1) — a rotation carries no server rows. A **cold**
  ledger (a fresh adapter that has not yet observed a pass — e.g. the process
  restarted and the user edits before any sync) refuses `NeedsResync` instead
  of guessing: the orchestrator re-syncs, the pass re-seeds the ledger, and
  the outbox retries the edit. A dead key (crash between edit and next pass,
  or a partially-applied chunked batch) surfaces as Sync status 3 and takes
  the stream's standing in-pass Reconcile recovery. The **hierarchy** key
  has its own shared ledger (`adapter/hierarchy.rs`): one server FolderSync
  cursor serves BOTH container scopes (`EasFolderList`/`EasCalendarList`),
  so the adapter tracks the freshest key plus a per-scope backlog of rows a
  riding scope missed (its class's folders, the class-less deletions, and
  the present-set after another scope's bootstrap — the riding pass then
  reads as a snapshot). Interleaved container passes share one cursor
  instead of invalidating each other into status-9 re-enumerations; a
  behind-ledger (another adapter advanced the server) still self-heals
  through the status-9 recovery. Per adapter — cross-adapter interleaves
  keep the old self-healing shape. The calendar write verbs ride the same
  collection-key ledger discipline against the bound calendar folder
  (seeded by a completed `sync_events` pass, rotated by each write).

## TLS decision record (landed — P0-b Task 8)

The EAS transport builds on the engine's unified TLS policy, exactly like the
Graph/JMAP/CalDAV transports: `EasClient::new(config, &tls)` takes the host's
`engine_tls::TlsClientConfig` and constructs its `reqwest::Client` via
`tls.reqwest_builder()` (workspace `reqwest` 0.13 + preconfigured ring-backed
rustls), finished with the EAS-specific non-TLS settings only (the client-wide
120 s timeout and the User-Agent). The crate's crate-local
`reqwest = 0.12 + native-tls` declaration is gone, and `EasConfig`'s
`accept_invalid_certs` field with it — a per-provider invalid-cert escape is
not expressible on the shared-policy path by design.

**What a host passes in**: one `TlsClientConfig` per account, built once from
its `TlsPolicy` via `engine_tls::client_config(&policy)?` and shared (a cheap
`Arc` clone) with the account's other providers — the same wiring as
`docs/agent-guidance/tls.md`'s "How each provider consumes it". The
autodiscover flow's `http` client is the caller's `reqwest::Client` and must
come from the same `reqwest_builder()`.

The historical motivation for native-tls in the Kylins client — **enterprise
root CAs and intercepting proxies** — remains expressible through the policy's
opt-ins (`crates/engine-tls/src/policy.rs`): `TlsPolicy::bundled_and_system()`
picks up OS/enterprise roots with zero configuration, `TlsPolicy::pinned`
trusts exactly a private CA, and `TlsPolicy::PlatformVerifier` delegates
wholly to the OS verifier (MDM-installed roots on Android). The old
`accept_invalid_certs` lab-server escape becomes "pin the lab CA as a custom
root"; the crate's own gated live tests use the test-builds-only
`TlsClientConfig::dangerous_accept_any()` (`EAS_LIVE_INSECURE`), which
never compiles into a production build.

Two notes on the switched transport. (1) The shared builder advertises ALPN
`h2` then `http/1.1`, so EAS may now ride HTTP/2 where the server offers it;
the transport's explicit `Connection: keep-alive` header is illegal in h2 and
hyper strips it there (harmless; EAS semantics don't depend on it). (2) The
EAS retry layers are unchanged: 449→Provision, 401→OAuth refresh, 451→
`X-MS-Location` adoption, in-body status retries, and the transport's own
`Retry-After` parsing into `EasError::HttpStatus`. `engine_http::send_retrying`
(spike option) was judged not to fit: EAS already promotes 429/503 windows
through `HttpStatus.retry_after`, and the engine's 60 s poll loop is the retry
for transient failures — revisit only if a live deployment shows a gap.

## Live testing (env-gated)

The live suite (`tests/live_eas.rs` + the split modules under `tests/live_eas/`)
runs against the shared O365/Exchange test account,
with the same skip-when-unset convention as the Google/Graph live suites
(`GOOGLE_ACCESS_TOKEN` / `GRAPH_ACCESS_TOKEN`), the gates named `EAS_LIVE_*`.
Gating is two-layered: every test carries `#[ignore = "live Exchange account
required"]`, and each additionally no-ops when a required gate is unset, so even
an explicit `--include-ignored` run without credentials skips cleanly (one
"live gates unset" line per test, exit 0).

| Gate | Required | Covers |
| --- | --- | --- |
| `EAS_LIVE_URL` | yes | the `Microsoft-Server-ActiveSync` endpoint |
| `EAS_LIVE_USER` | yes | mailbox address (the EAS `User` query param) |
| `EAS_LIVE_PASSWORD` | yes | the account password / app password |
| `EAS_LIVE_USERNAME` | no | Basic-auth identity when it differs from the mailbox address; unset → identity = `USER` |
| `EAS_LIVE_INSECURE` | no | set to `1` to trust the self-signed on-prem test server (the test-builds-only `dangerous_accept_any`, see the TLS record above) |

The exact invocation lives in the header of `tests/live_eas.rs` (run it with
`--test-threads=1`). The scaffold exercises the Basic-auth path only (OAuth
accounts need `EasConfig::auth`). Operational rules learned live: **one
`DeviceId` per TEST, not per role** — two tests sharing an id race each
other's per-device sync state even when each is internally serialized
(concurrent FolderSync bootstraps answered status 6, and concurrent
Provision handshakes status 135; live evidence 2026-08-28 for the former —
the contacts smoke and the calendar item probe both sat on
`KYLINSLIVETEST04`), and EAS `FolderSync` ServerIds are
per-device-partnership (never compare them across devices). The folder
probes are rerunnable by construction: per-run unique folder names, checked
command statuses, and self-cleanup, with a prefix-sweeping backstop
(`calendar_folder_drill_cleanup`) for crashed runs — the earlier
fixed-name-and-leave-artifact shape broke the "exactly one Add" assertion
whenever a leftover survived. Serial runs also keep concurrent load off the
lab server, which answers Sync/FolderSync status 111 under fan-out pressure
(see the classifier note above).

**The account-level live acceptance is `engine-cli eas-sync`** (the P0 exit):
`--rounds 2` against one `--db` is the full pass then the incremental one,
through the engine's own fan-out. Credentials come from the same `EAS_LIVE_*`
gates; the self-signed lab server needs a build with the diagnostic
`--features eas-insecure-tls` plus `--insecure`. The offline twin — the same
command against the mock harness — is pinned in CI
(`tests/transport_harness/engine_cli_flow.rs`). Live evidence from the first
full run (2026-08-28, on-prem Exchange 15.2, 45 mail folders, ~20k messages):
the Sync family must classify status **111** ("server error (retry later)",
the counterpart of 110's "do not retry") as transient — the server answers it
under fan-out load and recovers, and classifying it permanent reported a hard
failure for every folder that followed.

The P2 arms extend the same command over the PIM families (offline twins in
`tests/transport_harness/engine_cli_pim_flow.rs`, live twins in
`tests/live_eas/engine_cli_pim.rs`): `--kind calendar` drives the engine's own
`sync_calendar` fan-out over the discovered class-`Calendar` collections
(per-collection adapters, the container pass riding the shared store cursor)
and ends with the occurrence materialization summary;
`--kind calendar --create` adds the create→re-sync round-trip that proves the
Sync Add ack's ServerId backfill (the probe's uid is deterministic in the
account, so a repeat run against the same store resolves as a duplicate);
`--kind contacts` drives `sync_contacts` over the discovered type-9 address
books and ends with the people count.

**Fixtures**: anything learned from a live run (a wire shape, a status quirk, a
version-specific behaviour) must be captured as a **scrubbed fixture** wired into
the offline suite — observed bytes with every identifier moved to a reserved
name (`example.com`/`.net`/`.org`, `.test`, `.local`), keeping the byte shape,
per AGENTS.md "Identifiers in fixtures and docs use reserved names" and the
fixture rules the sibling providers' `tests/fixtures/` record. There is no CI
harness (no live account in CI): the suite is an occasional drift check, and it
is excluded from the offline coverage metric like the other providers' live
tests.

## Per-verb mapping table (trait → EAS), with verdicts

Verdicts: **no gap** (maps inside the current trait), **EAS-local** (works, but the
EAS side must build it), **engine change needed (fork patch)** (the trait/core must
move first). Full evidence (file:line on both sides) lives in the Kylins spike
document; this is the summary.

| Trait verb | EAS mapping | Verdict |
| --- | --- | --- |
| `connection_info` | **landed** (skeleton): composed per call — caps from the verb ladder (`none()` until each verb slice lands), `http_version` from the transport's `ObservedHttpVersion` (recorded by `options()` and every command send, shared across client clones, most-recent-wins; `None` before the `negotiate` OPTIONS first contact — the JMAP/CalDAV connect-time precedent), `concurrent_fetches` the default 1 until a measured per-server ceiling exists | no gap |
| `mailbox_scope` / `email_scope` | **landed** (skeleton): the adapter returns `SyncScope::EasFolderList` / `EasFolder` (+ `EasCalendarList`/`EasCalendar` and `EasContactList`/`EasContact` siblings exist in `engine-core` for the calendar/contacts slices, whose id bindings differ), exactly as IMAP (`ImapMailboxList`/`ImapMailbox`) and Graph (`GraphFolderList`/`GraphFolder`) did | no gap |
| `sync_mailboxes` | **landed**: `FolderSync` (`adapter/mailboxes.rs`); the hierarchy SyncKey is the cursor (`None`/empty → bootstrap `"0"` → snapshot of the full hierarchy; `Some(key)` → Add/Update/Delete delta); status 9 invalidation recovers **inside the call** — one re-bootstrap from `"0"` returning a snapshot (the JMAP needs-resync→snapshot-fallback precedent; a server answering 9 to `"0"` itself surfaces as `needs_resync`); non-mail classes (Calendar/Contacts/Tasks/Notes) are filtered out — `Mailbox` is the *mail* container type and those folders belong to the calendar/contacts scopes (the wire's Delete element carries no class, so delta deletions pass through unfiltered — tombstoning a key the mail scope never held is a store no-op); a success response omitting its SyncKey keeps the request's key (the Sync empty-body invariant — an empty key would poison the cursor); `Type`-derived roles (2 Inbox / 3 Drafts / 4 Trash / 5 Sent; Outbox and user types carry none, the raw type survives in `extended["eas/*"]`); the `mail` bit flipped only once `stream_email` landed beside it — see the crate section | no gap (JMAP `container_sync` is the code precedent) |
| `stream_email` | **landed**: Sync class Email (`adapter/email.rs`); cursor = collection sync key (`None`/empty → `"0"`); per-round chunks are `Additive` with `advance_to` = rotated key (sub-chunks within a round hold the cursor — the pre-round key is always a valid resume point; `fetch_batch` = `WindowSize`, `0` → the 512 drain-loop cap; `chunk_size` splits a round for incremental commit; Exchange-15.2's empty bootstrap round is followed once; metadata-tier only — bodies are `fetch_message_source`'s job); status 3 (and 12, degraded to the same reset) recovers **inside the stream** — one restart from `"0"` as a `Reconcile` pass (present-set + tombstoning), the JMAP `cannotCalculateChanges` recovery precedent; a mid-pass or at-`"0"` invalidation surfaces `needs_resync`; **no wire filter yet** (`FilterType` 0 — the window's bound holds at apply via `admits`; the coarse `FilterType` ladder is a pending slice, and note its wire values are an enum code table, not days: 1=1d, 2=3d, 3=1w, 4=2w, 5=1m, 6=3m, 7=6m) | no gap (`PassMode::Reconcile` matches SyncKey invalidation cleanly — JMAP `cannotCalculateChanges` recovery is the precedent) |
| `default_sync_window` / `SyncWindow` | `FilterType` day-ladder (coarse) + `admits` tighten | no gap (mapping note) |
| `fetch_message_source` | **landed**: `ItemOperations` Fetch with `MIMESupport=2` + `BodyPreference` type 4 (`adapter/source.rs`); a truncated answer (Truncated flag / Total shortfall) re-fetches as `Options>Range` rounds and reassembles byte-exact from the authoritative server ranges; per-item statuses classify (6 moved → `Conflict`, 18 → `Authentication`) | no gap |
| `submit_email` | **landed**: `SendMail` (`adapter/submit.rs`); the draft assembles through `engine-rfc5322`'s **filed** variant (SendMail routes recipients from the bytes, so the `Bcc` header stays in them — the Graph reasoning), rides as an OPAQUE `<Mime>` with `<SaveInSentItems/>`; the `<ClientId>` is `SM` + FNV-1a-64 of the normalized `Message-ID` (deterministic per message → Exchange's ClientId dedup absorbs a lost-response retry; live evidence: 15.2 enforces the 40-char cap with Status 103); empty-body success, in-body statuses classify through the SendMail table; the receipt keys the Graph `sent:<Message-ID>` placeholder (EAS returns no id) | no gap |
| `submit_email_source` | **landed**: same wire, bytes **verbatim**; validates the seam's shape first (`Message-ID`, `From`, terminated body); a non-empty `recipients` envelope is honored only when it names exactly the bytes' own To/Cc/Bcc addr-specs — SendMail has no separate envelope, so anything else is refused permanently (the comparison errs toward refusal: quoted display names count as un-comparable) | no gap |
| `file_sent_copy` | rejecting default — the server files the copy (`SaveInSentItems`), `Unfiled` never occurs | no gap |
| `edit_mail` | **landed** (`adapter/mutate.rs`): `SetKeywords` → Sync `Change` upsync (`$seen`→`Read`, `$flagged`→`Flag` incl. the empty `<Flag/>` clear form; any other keyword refused permanently pre-wire — the IMAP `PERMANENTFLAGS` spirit); `MoveTo` → `MoveItems` with the bound folder as source collection, receipt records the SOURCE key (the moved copy is a new ServerId that reconciles next sync); `Delete` → **refused `InvalidState`** (decided): EAS has no per-item hard delete and the trait's Delete means permanent — the documented policy is `MoveTo` the deleted-items folder (what Kylins' own source does); the upsync's collection SyncKey comes from the adapter's **key ledger** (see the quirk notes) | no gap (quirk decisions recorded here) |
| `Watch` (Ping) | **landed** (`adapter/watch.rs`, handed out by `EasAdapter::watcher`): one session watches the bound folder; status 2 → `Changed` (and a non-empty changed-folder list is a change signal whatever the status label — the mislabel defense), status 1 → `KeepAlive`; status 5 is absorbed (the client's retry carries the server interval on the wire; the watcher adopts it clamped into the 300–900 s band, ported from Kylins with its live evidence); a transport drop tunes DOWN before surfacing retryable (proxy/NAT idle kills); error statuses classify through the Ping table (7 → `NeedsResync`, else permanent; an HTTP 429 stays `RateLimited`); the tuning survives restarts via `heartbeat_secs`/`set_heartbeat_secs` — no trait seam needed | no gap; two *optional* fork records below |
| `calendar_scope`/`event_scope`, `sync_calendars`, `sync_events` | **landed** (P2 Task 2, `adapter/calendar.rs`): the scope overrides return `EasCalendarList`/`EasCalendar`; `sync_calendars` is the FolderSync container verb filtered to the Calendar class (folder Type 8), driven through the **shared hierarchy ledger** (`adapter/hierarchy.rs` — both container scopes ride one server cursor; see the quirk notes) with the status-9 in-call snapshot recovery (the mail slice's shape); `sync_events` is Sync class `Calendar` over the adapter's **calendar binding** (`EasAdapter::with_calendar` / `calendar_adapter` — the Graph placeholder-discovery pattern; the `calendars` capability bit flips with the binding, per the verb ladder), with the collection SyncKey as cursor, in-call `MoreAvailable` paging, the Exchange-15.2 empty-bootstrap follow, and status-3/12 invalidation recovered in-call by re-bootstrapping once as a snapshot (atomic whole-scope apply makes the restart clean). Items convert via `calendar::calendar_event_from_props` (id = ServerId, uid = the EAS UID, fixed-offset `Etc/GMT±H` TZI fold — see `calendar/convert_time.rs` — structural recurrence incl. exceptions as overrides); a malformed item is skipped, never failing the pass. The binding also flips `calendar_writes` (its verbs landed — see the write rows) and `calendar_rsvp` (the invitation answer landed with it — see the RSVP row) | no gap |
| `create_event` / `patch_event` | **landed** (P2 Task 3, `adapter/calendar_write.rs`): `create` → Sync `Add` with a synthesized ≤40-char ClientId — the ack under `Responses` ([MS-ASCMD] §2.2.3.7.2) is the only id-reveal point, and an ack-less success keys the ClientId placeholder (reconciled by `uid` next pass). `patch` → Sync `Change`: a **complete** `ApplicationData` rebuilt from `base` + patch (safe under both ghosting and whole-replacement server semantics), through `calendar/convert_write*.rs` — times fold back through the **fixed-offset** TZI only (a named-DST zone refuses: no adapter carries tzdata to resolve it, never a guessed offset); `PatchTarget::Series` rebuilds the master, `Instance` re-emits the master's `Exceptions` container with the target occurrence updated (start AND end when either moves); clears write explicit empty elements (never a ghosted old value); attendees ride as Email+Name (AttendeeStatus server-owned), the organizer NEVER (Status 6 evidence); EAS-native busy/sensitivity ride back from `extended["eas/*"]`; recurrence: engine rule → EAS Type/parts (the inverse of the read mapping), `Until` from the resolved instant or derived through the fixed offset; everything unrepresentable (sub-daily, BYSETPOS, daily+BYDAY, rule unions, exotic alerts/override fields) **refuses** rather than silently flattens. `WriteGuard::Absent` (Sync Change carries no revision tokens — last-write-wins) + `OverrideSurvival::kept()` by construction (a series Replace re-emits every override from the base — the CalDAV structural-patcher argument). The write rides the calendar collection-key ledger (cold → `NeedsResync`) | no gap (expressible; landed) |
| `put_event` | **landed as the rejecting default** — EAS's update verb is a field-level Change, not a document PUT, and there is no iCalendar document on an EAS server to PUT; the refusal names `patch_event` (the trait explicitly allows an adapter advertising `calendar_writes` to leave this refused) | no gap |
| `rsvp_event` / `rsvp_event_from_invite` | **landed** (P2 Task 4, `adapter/calendar_write.rs`): `rsvp_event` is **refused `InvalidState`** pointing at the message path (MeetingResponse addresses the invite EMAIL — a stored event names nothing the protocol can answer from), and `rsvp_event_from_invite` **is** `MeetingResponse`: `CollectionId` = the invite email's own mailbox membership (never the bound calendar), `RequestId` = the message id verbatim (the T4 identity mapping), `user_response` 1/2/3 (accept/tentative/decline), `InstanceId` never sent today (the neutral `EventRsvp` carries no occurrence target — the per-occurrence form maps there when one lands), `SendResponse` emitted iff notify ∧ negotiated version carries the token (16.0/16.1); `base` ignored by design — an EAS account can answer an invitation whose event the store has never held (the reason the fork verb exists; the engine facade is `Engine::rsvp_invitation`). `RsvpControls { comment: false (nowhere in the page-8 schema), suppress_notification: negotiated 16.0/16.1 (pre-negotiate: false), guard: WriteGuard::Absent }`, composed per call from the negotiated version so the wire never disagrees with the advertisement. No ledger ride — MeetingResponse carries no SyncKey | no gap (landed) |
| `delete_event` | **landed** (P2 Task 3): `DeleteTarget::Series` → wire `Delete` { ServerId }; `Occurrence` → a `Change` of the master carrying the deleted-marker exception ([MS-ASCAL] §2.2.2.16, the EXDATE form — the base event is REQUIRED for this form, it rewrites the series document); already-gone = success (a per-item 8, or no item status at all per §2.2.3.154); a failed item status surfaces with its code | no gap (landed) |
| `ContactsProvider` (`sync_address_books`…`delete_contact`) | FolderSync contacts folders + Sync class Contacts; `Add`/`Change`/`Delete` with ghosting; `WriteGuard::Absent` | rides the scope fork patch; otherwise no gap (P2) |
| `fetch_contact_photo` | EAS pictures are **in-band** (`Picture` inside ApplicationData; currently dropped at parse, presence-only) — retain the bytes (P2) and serve them; `ContactPhoto` fits (fingerprint = sync-key revision) | EAS-local TODO; no engine gap |

## Fork-decision records (engine repo)

These do **not** become upstream issues — the engine is our fork; they are patches
we may make here, judged against the cross-provider survey (see the spike document
for the full table):

1. **[Landed] EAS `SyncScope` variants** — the EAS scope family now lives in
   `engine-core` (`EasFolderList`/`EasFolder`, `EasCalendarList`/`EasCalendar`,
   `EasContactList`/`EasContact`, in `engine-core/src/sync/scope.rs`; member
   scopes carry the folder ServerId as a `MailboxId`/`CalendarId`/`AddressBookId`
   exactly as the sibling families do). The adapter's mail `*_scope` overrides
   now return them; the calendar/contacts overrides land with their adapter
   slices. Survey: every non-JMAP provider
   (IMAP, Graph, Gmail, CalDAV/CardDAV) added its own variants; EAS is the sixth
   and the pattern is mechanical.
2. **[Optional, deferred] `Provider::watch()` accessor** — there is no trait seam
   to obtain a `Watch` today; IMAP and JMAP hand out concrete watcher types and the
   host switches on provider kind to build one. EAS would be the third such. A
   `fn watch(&self, scope) -> Option<Box<dyn Watch>>` defaulting to `None` would
   fix the switch-on-kind leak for all three; not needed for Plan C.
3. **[Optional, deferred] scope-tagged `WatchEvent`** — EAS `Ping` monitors many
   collections on one connection and names which changed; `Watch` is one-session-
   one-scope, so per-folder watchers multiply long-poll connections (correct but
   wasteful). `WatchEvent` is `#[non_exhaustive]` and its docs anticipate a
   scope-tagged variant. Defer until profiling shows the extra connections hurt.

## EAS-local work items (crate/adapter side, no engine involvement)

- ~~Adopt `engine-tls`/`TlsClientConfig` (Task 8) and drop `accept_invalid_certs`.~~ — landed (see the TLS decision record above).
- `Options>Range` pagination for ItemOperations Fetch (large attachments).
- Retain contact `Picture` bytes (currently presence-only) for
  `fetch_contact_photo`.
- Keyword-vocabulary policy for `MailEdit::SetKeywords` (express `read` +
  `flagged`; refuse the rest, IMAP-`PERMANENTFLAGS` style).
- `MailEdit::Delete` policy: refuse vs degrade-to-Trash-move (Kylins degrades
  today); whichever is chosen, document it in this file.
- Heartbeat persistence across restarts (constructor parameter or re-tune per
  session; the trait offers no persistence seam, and none is needed).
