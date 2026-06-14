# assistant-host Contract

## Public API
The run-loop composition root: drives one real turn end-to-end. Owns
`RunOptions`/`run`/`serve` and the generic, channel-agnostic
`Host<R: ContainerRuntime>` that holds a `SessionLayout` and, per turn, enriches
the inbound message with retrieved catalog memory (see Persistence/Config),
enqueues an inbound session message, ensures a per-session container is spawned,
polls `outbound.db` for the reply, marks it delivered, and returns the
`OutboundMessage`s via `run_turn(&InboundMessage)`. `process_turn` is the
terminal wrapper over `run_turn` (read a line as the local sender, render each
reply to a writer). The Slack inbound surface (`serve_slack` + `SlackServeOptions`)
drives a Socket Mode listener over an injected `SocketOpener`, mapping each
routable, non-self message to a per-channel `Host` and posting replies back over
a `SlackChannel`; the one-call `serve_slack_live` (gated behind the non-default
`socket-mode` feature) wires the real websocket transport and routes both Slack
surfaces through the OneCLI proxy so the process never holds a Slack token (see
Config). Before a Slack message drives a turn it passes a deny-by-default gate
(see Persistence): `evaluate_sender` (assistant-permissions) under the configured
`UnknownPolicy` — an unknown sender is dropped and audited — then
`evaluate_engagement` (assistant-router) under the configured `EngagementMode`; an
engaging message in `MentionSticky` mode (re)opens the conversation's sticky
window so follow-ups stay engaged. Socket Mode is at-least-once, so a redelivered
event (the same `RoutingEvent.dedupe_key`) is dropped before the gate and the
turn by a bounded in-memory recently-seen set — a redelivery never re-runs the
gate or drives a duplicate reply. The set is per serve session and in-memory, so
a redelivery spanning a process restart can still double-fire. Also supplies the reusable product
`SetupStep`s (apply the domain subsystems' central-DB migrations, build agent
image, provision the per-installation OneCLI gateway material) that both products
append before the readiness gate, and the reusable `admin::register_user` action
(create/reuse a user and bind a Slack DM route) backing the products'
`register-user` subcommand. The terminal `run`/`serve` path is the trusted local
console and is not gated. It exposes `scheduler::sweep_once`, one
deterministic due-work pass over the central DB (caller supplies `now`): it
expires stale sticky-engagement windows, then claims every due scheduled
occurrence (assistant-scheduler) and drives each as a turn into the occurrence's target
session, completing the occurrence only after its turn runs.

When `SlackServeOptions.scheduler` is set (the live Slack daemon does so via
`SchedulerTickConfig { owner, lease_ttl_secs, tick_interval }`), the Slack serve
loop also fires due scheduled items on a live cadence. The transport surfaces a
`SocketError::Idle` yield when no inbound frame arrives within its read window; on
each idle yield the loop runs a throttled scheduler tick on the same single
thread (so a fired turn reuses the channel's existing per-channel `Host` rather
than racing a second container). A tick expires sticky windows, then claims every
due occurrence, and for each resolves its target session, drives the item's intent
as a turn there, posts the reply **top-level** (no thread root), and finalizes the
firing exactly once (skipping finalization on a failed turn so a later tick
retries). A retried firing drives the turn through `Host::run_turn_keyed` with the
occurrence's stable idempotency key, so the inbound enqueue reuses the one row a
prior attempt wrote rather than accumulating duplicates that the eventual
container would each reply to. One-off and recurring items both fire; they differ only in finalization,
which commits in a single transaction: a recurring item has its occurrence marked
fired and its projected `process_after` advanced to
`recurrence.next_after(scheduled_for)` (anchored to the occurrence's scheduled
time, so a late run never drifts the cadence) while staying active, so the next
sweep claims the following occurrence; a one-off has its occurrence marked fired
and the item moved to `Completed`, dropping it from the active set the sweep walks
(the already-fired guard also blocks any re-claim). Committing both writes together
means a crash can never leave a recurring item with its occurrence fired but its
`process_after` un-advanced (which would silently halt the recurrence). Catch-up
after downtime fires at most one occurrence per tick.
A turn may also create scheduled work: when a run emits a `schedule_message`
action, the serve loop intercepts it (in both the inbound and scheduled-turn
delivery paths), projects a one-off or recurring item into the central index due
in the turn's own session, and suppresses delivery (a scheduling action is not
user-visible text, so the run's own confirmation text is what the user sees). The
host owns the item's identity, agent group, and creation time; the agent supplies
only the intent text and timing. This is central-only, matching
`admin::create_scheduled_message`; durably reconstructing live-created items from
a per-session source of truth, the runner-side tool wiring that makes an agent
emit the action, and the terminal-path tick remain deferred.
`admin::create_scheduled_message` (the operator path, also used before runner-side
emission lands) seeds one scheduled item into the instance's central projection
for the live daemon to fire — one-off by default, or recurring when
`--every-seconds` is given.

A turn may also persist memory: when a run emits a `save_memory` action, the serve
loop intercepts it (in both the inbound and scheduled-turn delivery paths) and
writes the note as a side effect rather than posting it (a memory write is not
user-visible text, so the run's own text is the confirmation). The host owns the
entry's identity, scope, and trust — a directly-stated, all-chats, same-scope,
high-confidence note keyed under the single host agent group — and the agent
supplies only the content and an optional title.

A turn may also delegate to a specialist sub-agent. The host is
**specialist-agnostic**: it holds no specialist itself; products register a list
of `SpecialistSpec`s (the `assistant-specialist-spec` vocab — plain data describing a
specialist's route name, agent-graph identity, concurrency limits, custom image,
and in-container turn config) via `SlackRunOptions.specialists`. When a run emits
a `delegate` action (only on the Slack inbound path, and only when the spec list
is non-empty), the serve loop intercepts it *after* delivering the turn's other
replies — so the orchestrator's acknowledgment posts first — and drives the
delegated task through three phases split across threads.
`delegation::begin_specialist` runs on the serve thread (it owns the central DB):
it resolves the `delegate` payload's `specialist` against the registered specs by
`route_name` (an unmatched name is reported, no job opened), builds a
`RegisteredProfile` from the matched spec, creates the spec's specialist group on
first use, opens and starts the job, and returns a `SpecialistTicket` carrying the
resolved spec. `delegation::run_specialist_turn` runs on a **background worker
thread** spawned per job (it touches no DB): it executes the specialist's container
turn off the serve thread and reports the outcome back over an `mpsc` channel.
`delegation::finish_specialist` runs again on the serve thread when that result is
drained: it terminalizes the job, records provenance, and yields the re-injection
text. The agent-graph engine (assistant-agent-graph) owns the job lifecycle, policy,
and audit: it admits each registered spec's profile into a registry, creates the
spec's specialist group on first use (a second create is rejected by the
spec's max-specialists policy), validates a structured `HandoffPacket` (goal plus
ephemeral facts and constraints), opens and starts a job under the spec's
concurrency policy (deliberately over-provisioned now that jobs run off the serve
thread), drives its status machine (Queued→Running→Succeeded, or →Failed on any
post-start error), and enforces the spec's result-artifact size policy — auditing
each transition. A job counts as in-flight from `begin` until `finish`, so the
concurrency cap bounds the number of background workers running at once.
The Host machinery owns execution: the specialist runs one turn in its own
job-keyed container (`{agent}-{job_id}` under the spec's session group, so it
never collides with a channel container) on the spec's **custom image** — the host
sets `config.image` from the spec's `image_repository`/`image_tag` (or pins by
digest when set), so each specialist runs the binaries baked into its own image
rather than inheriting the orchestrator's. Delegation is **fully background**: the
worker thread runs the specialist while the serve loop keeps accepting and handling
inbound frames (a second message is serviced while a specialist is in flight). The
serve loop drains finished jobs on every idle tick (unthrottled, independent of the
scheduler sweep) and once more at shutdown — where it first joins every in-flight
worker (each bounded by the per-turn timeout) and then does a final drain, so an
acknowledged delegation still delivers its follow-up rather than being dropped. The
specialist's result text is re-injected as a fresh follow-up orchestrator turn
through the same per-channel `Host`, so the orchestrator integrates it and replies
in its own voice, threaded under the original trigger; a nested `delegate` from
that follow-up is dropped (re-delegation is bounded to one level). A hard
specialist failure — e.g. an unknown specialist, rejected by `begin_specialist`
before any job is opened, or a failed run — is surfaced to the user in the same
thread rather than left silent. A specialist runs with
`RunnerAuthMode::Specialist`: its own real Claude turn, credentialed through the
OneCLI proxy exactly like the orchestrator (`Specialist` is gateway-gated just
like `ClaudeOAuth`, differing only in its runner-mode tag), with a restricted
toolset the host supplies entirely from the spec via generic `CLAW_SPECIALIST_*`
env (system prompt, enabled tools, auto-approve patterns, step ceiling). The host
folds any per-spec guardrails into the spec's `system_prompt` at build time, so
the in-container harness needs no specialist-specific knowledge. On success the
host records the container that ran the job as a run link
(`assistant_agent_graph::store::add_run_link`), additive provenance whose failure is
logged, not fatal. A `delegate` reaching the direct delivery path (a scheduled
turn, which has no delegation driver) is dropped rather than posted as raw payload.

Owns the real, node-backed qmd `MemoryBackend` (the `qmd` module): `NodeQmd`
implements assistant-memory's pure `MemoryBackend` seam by shelling the host-side
sidecar — `QmdSidecar` is the plain config it carries (node binary, sidecar
script, index db / corpus paths, topN, search/index timeouts), and the
`SidecarRunner` trait abstracts the process call so `NodeQmd::with_runner` lets the
offline gate drive every fail-open branch with a fake (no node required) while
`NodeQmd::new` shells real `node` in production. The sidecar speaks a three-verb
JSON-over-stdio protocol (`retrieve`/`embed`/`status`); `parse_status_response`
maps the `embed`/`status` reply (`{ indexed }` healthy when `N>0`, else empty
index; `{ error }` degraded through the shared `from_wire` vocabulary; malformed/
empty degraded) into `MemoryHealth`, and search replies reuse assistant-memory's
`parse_qmd_response`. The host is the only crate allowed to shell out, which is why
this lives here rather than in assistant-memory; failure modes (missing binary, timeout,
non-zero exit, malformed output) all degrade memory health, never fail a turn.
Retrieval is a lex+vec hybrid: the sidecar issues an embedding-similarity (vec)
query plus a keyword (FTS lex) query built from the same message and lets qmd fuse
them via RRF — vec catches paraphrase/semantic matches, lex catches exact terms
(names, ids, jargon) the embedding can miss (no HyDE/LLM query expansion — that
cold-loads a multi-GB generative model per call and is incompatible with the
per-turn search timeout, so only the embedding model is ever loaded). The inbound
message is sanitized before it reaches qmd: newlines are collapsed to a single line
and hyphen-before-word/quote sequences are neutralized, because qmd's semantic
validator rejects both multi-line queries and the negation pattern `-\w`/`-"` (the
latter otherwise misfiring on ordinary hyphenated words like "sub-agent"); the lex
leg double-quotes each alphanumeric token (deduped, capped) so FTS operator words
and stray punctuation are treated as literals. If the hybrid search throws (the lex
leg being the riskier addition) the sidecar retries vec-only so a keyword-query
problem cannot knock out semantic retrieval; a vec-only failure still degrades
fail-open. The corpus is written one file per memory id whose filename
is the hex encoding of the id (`assistant_memory::corpus_stem_for_memory_id`), a fixed
point of qmd's path normalization, so a hit's `displayPath` stem decodes back to
the memory id rather than being mangled.

## Persistence Ownership
Owns no tables and defines no migrations of its own. Reads `inbound.db`/`outbound.db`
only through the `assistant-session` public API (host writes inbound even-seq, reads
outbound read-only). A `Host`'s delivered high-water mark is held in memory but
resumed from the persisted `delivered` marker (`assistant-session::max_delivered_seq`)
on first turn, so a daemon restart mid-session does not re-read and re-deliver the
prior turn's reply — delivery stays at-most-once across process restarts.
Container lifecycle state is derived from the on-disk
heartbeat, not stored here; the host refreshes (truncates in place to a current
mtime) that placeholder before each spawn so a heartbeat left by an
earlier process — possibly older than `stale_after` — cannot cause the
freshly-spawned container to be reaped during its boot window. As the run-loop composition root it does, at setup,
apply the domain subsystems' own central-DB migrations (the `v2` migrations
owned by `assistant-router`, `assistant-permissions`, `assistant-scheduler`, `assistant-memory`, and
`assistant-agent-graph`) on top of the M1 baseline, so a real install has the
sticky-engagement, standing-instruction, scheduler-lease, memory-catalog, and
specialist-job tables those subsystems read; it authors none of that SQL — each
owning module does. During a delegation it reads and writes the specialist-job and
agent-group state only through assistant-agent-graph's engine/store API (`create_specialist`,
`start_job`, `transition_job`, `specialist_group_count`), authoring no SQL.

On the Slack serve path it opens this installation's central DB once and, per
inbound event, touches it only through the owning modules' public APIs (authors
no SQL): it reads `user_dms` (assistant-permissions `evaluate_sender`) and the active
`sticky_engagement` row (assistant-router `has_active_sticky`), and writes the
`dropped_messages` audit (assistant-router `record_drop`) for a rejected message and a
`sticky_engagement` row (assistant-router `open_sticky`) when an engaging message
opens/refreshes the sticky window. The `register-user` admin action writes
`users`/`user_roles`/`user_dms` via assistant-permissions (`bootstrap_owner` /
`create_user` / `add_user_dm`). The sticky window is keyed under a single host
agent group (this composition wires one agent).

When memory injection is configured (both run paths set it), each turn opens the
central DB read-only and reads the `memory_entries` catalog through assistant-memory
(`retrieve` → `render_memory_block`, authoring no SQL) to build a
`<retrieved_memories>` block, keyed under the same single host agent group. Catalog
retrieval yields metadata-only envelopes, so before rendering, the host reads each
entry's markdown body back from the agent's on-disk memory root and attaches it as
the envelope snippet (capped at a fixed char budget); this is what makes the
injected block carry the actual remembered text without a qmd index in front of
it. Body hydration is fail-open per entry: a missing or unparseable note leaves
that envelope snippet-less rather than dropping it. The block is attached to the
inbound message's `metadata` (leaving `content` untouched, so it is a clean
side-channel the shim consumes separately), capped at a fixed top-N. Injection is
fail-open: an open or retrieve error skips the block and the turn proceeds
unenriched. A turn already carrying inbound metadata is left unmodified. Retrieval
uses a default (empty) context by design: the isolation boundary is the instance,
not a per-entry scope, so every entry the host writes (`all_chats`) is eligible and
no channel/thread/user filtering is applied. The scope-filtering machinery in
assistant-memory is retained as latent capability, not wired here.

When a qmd sidecar is configured (the `MemoryRetrieval.qmd` field; live-only, set
via `with_qmd_sidecar`), each turn first tries a semantic ranking pass *on top of*
the catalog: it reads the same agent group's catalog rows (`entries_for_agent`),
runs the inbound text through the host-shelled `NodeQmd` backend
(`MemoryBackend::search`), and composes the hits over those candidates
(`assistant-memory inject_from_search` → `render_memory_block`) — snippets come straight
from the qmd hit, so no on-disk hydration runs on this path. A healthy qmd result
is authoritative: even an empty ranking suppresses injection (qmd said "nothing
relevant"). Only an unconfigured or *degraded* qmd state (missing sidecar/node,
timeout, malformed output, or a catalog read error — `SearchOutcome::is_degraded`)
falls through to the unranked catalog floor described above. qmd therefore never
makes memory worse than catalog-only: it is purely additive and fail-open, and the
default + offline-test paths (no sidecar configured) are byte-for-byte the existing
catalog behavior.

A turn's `save_memory` action is the write side of the same catalog. The host
parses the action's payload, composes a markdown note (an optional title heading
plus the content), writes it under the agent's orchestrator memory root
(`<groups_dir>/orchestrator/memory/notes/<memory_id>.md`, the `memory_id` derived
content-stably via assistant-memory `generate_memory_id`), and projects its front matter
into the `memory_entries` catalog via assistant-memory `upsert_entry` (authoring no SQL),
keyed under the single host agent group. The host fixes the entry's front matter —
`all_chats` scope, `user_said` source, `high` confidence, `same_scope` reuse — so a
later turn's all-chats retrieval surfaces it. It also stamps provenance from the
turn that produced the memory: `source_ref` (the Slack channel and its thread root)
and `source_user_id` (the sender; absent for a scheduler-driven turn). Provenance is
recorded for citation, never filtered on — the scope stays `all_chats`. The write is
central + on-disk only,
mirroring `schedule_message`'s host-mediated projection; the agent never touches its
memory root. A `save_memory` emitted when memory is not configured is dropped with a
log. Durable per-session reconstruction (Option-B) remains deferred.

`scheduler::sweep_once` reads and writes the central DB only through the owning
modules' public APIs (authors no SQL): it expires due `sticky_engagement` rows
(assistant-router `expire_sticky`), then over `scheduled_items`/`scheduled_occurrences`
it claims every due occurrence (assistant-scheduler `claim_due`, which writes the lease
hold), resolves each to its target session via the agent's item projection
(`list_items`), and marks the occurrence done after its turn runs
(`complete_occurrence`). Exactly-once is the scheduler's lease, not this crate's:
a turn that fails leaves the occurrence uncompleted so its lease expires and a
later sweep retries it. The sweep takes a borrowed `Connection` (the caller owns
the open DB) and never holds a session DB open across occurrences — each fired
occurrence derives its own `SessionLayout` and a discrete `Host` that is shut down
after the turn.

## Config
Reads the instance layout (home/namespace/instance → sessions root) from
`assistant-config` and the session id/agent group from `RunOptions`. Both run paths
wire memory injection via `HostConfig::with_memory(central_db_path,
agent_group_id, limit, groups_dir, owner)`: the installation's central DB path,
the single host agent group (`HOST_AGENT_GROUP`), a fixed injection cap
(`MEMORY_INJECTION_LIMIT`, the v1 pre-reply RAG top-N), the per-agent-group memory
root parent (`InstanceLayout::groups_dir`), and the host agent's logical owner id
(`HOST_AGENT_OWNER`, which selects the orchestrator memory root on disk and guards
the write/read against a foreign owner). The same wiring backs both the read
(snippet hydration) and write (`save_memory`) sides of the catalog.

The optional qmd semantic-ranking layer is opt-in via the environment and wired
only on the live Slack serve path. `CLAW_QMD_SIDECAR` is the path to the host-side
node sidecar script (`qmd-sidecar.mjs`); when set (and non-empty) the run-loop
builds a `QmdSidecar` config and layers it onto memory via `with_qmd_sidecar`, and
`CLAW_QMD_NODE` optionally overrides the `node` binary used to run it. The qmd
index db and corpus live under the orchestrator memory root's own `qmd/`
subdirectory (`<root>/qmd/index.sqlite`, `<root>/qmd/corpus`), which the catalog's
markdown walk already skips as derived state — so qmd never re-indexes its own
artifacts. Absent `CLAW_QMD_SIDECAR` (the default, and every offline test) memory
stays catalog-only. When a sidecar is configured the run-loop also spawns a
detached startup thread that re-indexes the orchestrator memory root into the qmd
corpus + index (`reindex_root` driving `NodeQmd`); it is fail-open (every failure
logged, daemon proceeds on the catalog floor) so the serve loop never blocks on a
first-run embed that may download embedding models. Each installation
has its own OneCLI gateway: `CLAW_ONECLI_URL` names this installation's gateway
base URL, and the OneCLI agent identifier is the instance directory name (e.g.
`assistant-work`, `cleoclaw`) so credentials never mix across installations. That
same identifier names the per-session container `{agent}-{session}` (e.g.
`cleoclaw-default`), so two installations sharing a session id never collide on a
Docker name. On
the Claude path the run-loop queries `GET <url>/api/container-config?agent=<id>`
and applies the returned proxy env plus a CA trust anchor to the spawn; the CA is
mounted after `prepare_spawn` to bypass the `.pem` mount block (it is a public
trust anchor, not a secret). The Anthropic credential is never read by this
crate; it lives only in the OneCLI gateway. `CLAW_ANTHROPIC_SECRET_FILE` is
probed for presence/size only, never read.

The Slack serve path uses the same OneCLI gateway for credentials: the real
`xoxb-`/`xapp-` tokens live only in the vault, and `serve_slack_live` takes a
host-facing proxy URL plus the CA path instead of any token. Both the Socket Mode
opener (`apps.connections.open`) and the outbound Web API client send a fixed
non-secret placeholder Bearer through that proxy, which swaps in the real token on
the wire, selected by request path (`apps.connections.open` → app token;
`auth.test`/`chat.postMessage` → bot token). The WSS dial then goes directly to
Slack (its URL carries a Slack-issued ticket, no token), so no MITM CA is needed
for the websocket itself.

The gateway scopes injection per agent and identifies the agent by a token in the
proxy URL's userinfo (the same token the container receives via `HTTPS_PROXY`).
`run_slack` derives the host's authenticated proxy URL by fetching the agent's
container-config and rebinding that userinfo onto the host-reachable proxy
endpoint passed in (`--proxy-url`, default `http://127.0.0.1:10355`) — the
container-facing `host.docker.internal` authority is unreachable from the host.
The derived URL carries the agent token and is fed to curl via stdin config only,
never argv or logs. `CLAW_ONECLI_URL` must therefore be set for `serve-slack` (in
both stub and Claude mode), since the host's Slack calls are injected regardless
of the container runner mode. The live `serve-slack` path sets
`SlackServeOptions.scheduler` to a `SchedulerTickConfig` with a stable
per-installation lease `owner` (the OneCLI agent id, so a restarted daemon
reclaims its own stale leases), a `lease_ttl_secs` comfortably exceeding a turn
(300s), and a `tick_interval` (30s) that throttles the sub-second idle-read
cadence down to a sane central-DB poll. A `None` scheduler (the default, used by
offline tests and any inbound-only caller) leaves the loop purely reactive. The
product also sets `SlackServeOptions.specialists` to the `SpecialistSpec`s it
registers (empty by default — offline tests and any non-delegating product —
which drops any `delegate` row a turn emits). From that list the host derives two
env channels, both carried via `HostConfig::extra_env` (the generic per-spawn env
appended after the auth/gateway env). The **orchestrator** container receives
`CLAW_SPECIALISTS`: a JSON array of `{name, description}` menu entries (one per
spec) the orchestrator's harness turns into the dynamic `delegate` tool enum and
prompt — with no specs the harness omits the `delegate` tool entirely. Each
**specialist** container receives the generic `CLAW_SPECIALIST_*` turn config from
its resolved spec (`CLAW_SPECIALIST_SYSTEM_PROMPT`, `CLAW_SPECIALIST_TOOLS` and
`CLAW_SPECIALIST_ALLOWED_TOOLS` as JSON arrays, `CLAW_SPECIALIST_MAX_TURNS`) plus
any spec-declared `extra_env`; `run_specialist_turn` overwrites `extra_env` so a
specialist never inherits `CLAW_SPECIALISTS`. A specialist runs with
`RunnerAuthMode::Specialist`, a gateway-gated mode, so the run-loop applies the
OneCLI proxy/CA config to the specialist spec just as it does for `ClaudeOAuth`.

`scheduler::sweep_once` reads no config of its own: the caller passes everything
explicitly — the open central-DB `Connection`, the sessions root and `group`
(to derive each fired occurrence's `SessionLayout`), the `agent_group_id`
(`HOST_AGENT_GROUP`), the lease `owner` string and `lease_ttl_secs`, the same
`HostConfig` the inbound loop uses (so a scheduled turn spawns with identical
auth/memory wiring), a `runtime_factory` closure (each occurrence gets a fresh
runtime), and `now` (epoch seconds). assistant-scheduler never reads the wall clock, so
the caller owning `now` is what keeps a sweep deterministic and offline-testable.

## Events
Emits no central-DB events. Surfaces per-turn progress (spawn, reply delivered,
container died/timed out) and scheduler-tick activity (a claimed occurrence fired
and delivered, a recurring item advanced to its next due time, a failed scheduled
turn left for retry) to the host's stdout/stderr and the process exit code.

## CLI/Web Surfaces
Backs the products' `run`/`serve`/`serve-slack`/`register-user`/`schedule`
subcommands. No web surface. A `serve` session outlives a single bad turn: a recoverable per-turn
failure (the container died, or a turn timed out) is reported and the loop keeps
reading, while a configuration, protocol, or IO fault aborts the session. `run
--once` propagates any per-turn failure as the exit code. The Slack serve loop
backs a product's `serve-slack` over Socket Mode: it survives a single bad turn
the same way (a failed session derive, turn, or delivery is logged and skipped)
and returns only on an unrecoverable listener fault (e.g. a rejected app token)
or when its `stop` predicate trips. On return — for any reason — it drains every
per-channel container it spawned before exiting. The live `serve-slack` daemon
installs SIGTERM/SIGINT handlers whose only act is to flip an atomic the `stop`
predicate reads, so a signalled daemon observes the request within one idle read
window and exits through that same drain instead of orphaning its containers to
default-disposition process death. Each Slack channel maps to its own per-channel
session/container; the bot's own posts are filtered (by `bot_user_id`/`bot_id`
from `auth.test`) so a reply never re-drives a turn. The Slack serve gate defaults to `UnknownPolicy::Strict` (only
a registered sender drives a turn) and `EngagementMode::MentionSticky` (a
mention/DM engages, then follow-ups stay engaged while the sticky window is open)
— a failure to open the gate's central DB is fatal (serving ungated would bypass
deny-by-default), but a per-event gate read/write error is logged and the message
is skipped. `register-user` (`--handle/--channel/--address/[--owner]`) is the
admin path that makes a Slack sender known so it clears the gate. `schedule`
(`--session/--in-seconds/--text [--every-seconds <n>]`) is the stopgap admin path
that seeds a scheduled item into the instance's central projection for the live
Slack daemon to fire — one-off by default, or recurring on the given interval with
`--every-seconds` (until message-driven scheduling lands).

## Prompt Fragments
None.

## Readiness Checks
Reuses `assistant-runtime-docker` readiness (Docker daemon reachable, base image
resolves, mount roots ready) and a per-installation OneCLI readiness probe so a
Claude-mode run is refused unless all three hold: `proxy_configured` (the gateway
URL is set and a CA has been fetched/persisted for this installation),
`anthropic_secret_present` (the secret file is present and non-empty), and
`placeholder_injection_ok` (setup recorded that the gateway returned a usable
container-config — proxy env plus CA — for this agent). The `configure_onecli`
setup step provisions this material live (fetch container-config, persist CA,
mark injection ok) only when a gateway URL is set; offline/stub setup leaves it
Pending without failing.

## Conformance Tests
A full inbound→spawn→poll→deliver turn round-trips over real session DBs with
`FakeRuntime` and an in-process fake shim; two turns reuse one warm container;
host-even/container-odd sequence parity holds; with memory injection configured
over a migrated central DB seeded with an all-chats catalog entry, a turn
attaches a `<retrieved_memories>` block carrying that entry to the inbound
message's `metadata` (and still delivers the reply); a Claude-mode spawn is refused
when OneCLI is not ready; the run never accepts output from a runner whose
declared protocol version the shim does not support. A container whose heartbeat
goes stale is reaped (handle cleared, stale heartbeat removed) and the next turn
spawns a fresh container that picks up the orphaned inbound; only death and
timeout are classified recoverable for the serve loop. Over a migrated central DB
(router + scheduler v2 migrations) seeded with one due one-shot scheduled item
bound to a session and one already-expired sticky window, a single
`sweep_once` pass expires the sticky window (`expired_sticky == 1`) and fires the
occurrence once (`fired == 1`), driving the item's intent as a turn into its
target session (the reply lands in `outbound.db`); a second pass at a later `now`
is a no-op (`SweepReport::default()`), proving the scheduler lease completes the
occurrence exactly once. OneCLI gateway handling is unit-covered offline: camelCase
container-config parses, the fetched env+CA are applied to the spawn (CA mounted
read-only), URL-unsafe agents are rejected, distinct installations derive
distinct agents/ports, and setup provisioning persists the CA + injection marker
(and writes neither without a CA). The node-backed qmd backend is unit-covered
offline through an injected `SidecarRunner` (no node binary): `search` parses
runner hits and sends the expected request shape; `reindex` writes one
`<memory_id>.md` per doc and clears stale corpus files before reporting health;
and every transport failure (missing binary → `MissingBinary`, timeout →
`QueryTimeout`/`StartupTimeout`, malformed output) degrades rather than panicking,
with `parse_status_response` covering each `{ indexed }`/`{ error }`/malformed
shape. The Slack serve loop is covered offline with a
scripted `SocketOpener`, a fake Web API, `FakeRuntime`, an in-process fake shim,
and a real migrated central DB: a mention from a registered sender drives a full
turn whose reply is posted back threaded under the triggering message; an unknown
sender is denied (no turn, no session) and audited as a `UnknownSender` drop; a
mention opens a sticky window so a following plain (non-mention) message from the
same sender still engages; and a self-authored event (the bot's own echo) is
filtered without spawning a session or posting a reply. The live scheduler tick
is covered on the same harness over a central DB carrying the scheduler v2
migrations: with the serve loop driven only by `SocketError::Idle` yields and
`scheduler: Some(..)`, a due one-off item seeded for a channel fires exactly once
— its intent drives a turn whose reply is posted **top-level** (no thread root) to
that channel, the occurrence becomes non-claimable (fired), the item is moved to
`Completed` (dropping out of the active listing), and subsequent ticks do not
re-fire it; a due *recurring* item instead fires once per tick, advancing
its projected `process_after` by exactly one interval each time (drift-free,
verified against `orig + 2*interval` after two ticks), staying active and
producing the next claimable occurrence. On the same harness, a turn whose shim
emits a `schedule_message` action records a matching item in the central index
(due in the turn's own session, owned by agent group 1, recurring per the
payload) and posts nothing to Slack, proving the action is intercepted rather than
delivered. Likewise, over a central DB carrying the memory catalog v2 migrations
and with memory configured, a turn whose shim emits a `save_memory` action posts
nothing to Slack but projects exactly one catalog row and writes its markdown note
under the orchestrator memory root; a later sticky-engaged turn in the same channel
then carries that memory in its inbound `metadata` — both the `<retrieved_memories>`
block and the hydrated note body text — proving the full write → catalog →
hydrated-injection loop offline. Delegation is covered on the same harness over a
central DB carrying the agent-graph job migrations and with a registered
`SpecialistSpec` (the test imports the browser spec to exercise the generic seam
end to end): a turn whose shim emits a `delegate(<route>, …)` action runs a
specialist sub-agent in its own job-keyed session under the spec's group (served
by a watcher that polls the group dir for the runtime-minted job id), and its
result is re-injected as a follow-up turn whose echoed reply — carrying the
specialist marker and the original goal — is the single post, threaded under the
trigger; afterwards the spec's group exists exactly once, no job is left
queued/running, one `specialist_jobs` row is `succeeded`, and one
`specialist_job_runs` row links the container that ran it. A
`delegate` to an unknown specialist instead posts an apology in the
trigger's thread and opens neither a specialist group nor a job (the payload is
rejected before either is created). The background nature is pinned by a third
test: a gated specialist watcher withholds its reply until a release file appears,
so a delegated job stays in flight on its worker thread while a *second* inbound is
serviced — the second message's echo posts first, then (once the gate releases and
the shutdown drain joins the worker) the delegated follow-up; the reverse ordering
would be impossible under the old synchronous path. The `register-user`
admin action is unit-covered offline: registering binds a DM route that resolves
(and clears the strict sender gate), and a re-run is idempotent (reuses the
handle, refreshes the DM address).
