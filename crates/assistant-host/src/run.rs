//! The per-session run-loop: enqueue a terminal line, ensure a container is
//! spawned, poll `outbound.db` for the reply, render and deliver it.
//!
//! The loop is generic over [`ContainerRuntime`] so production drives the real
//! CLI-backed Docker runtime while tests drive `FakeRuntime` with an in-process
//! fake shim over real session DBs. The host writes only `inbound.db` and reads
//! `outbound.db` read-only, all through the `assistant-session` public API.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assistant_agent_protocol::{check_runner_protocol, RUNNER_PROTOCOL_VERSION};
use assistant_channel_cli::{local_sender, render_message};
use assistant_db::open_central;
use assistant_memory::{
    entries_for_agent, inject_from_search, render_memory_block, retrieve, MemoryBackend,
    RetrievalContext,
};
use rusqlite::Connection;
use assistant_runtime_docker::{
    build_session_mounts, classify, prepare_spawn, ContainerId, ContainerRuntime, ImageRef,
    LifecyclePolicy, OneCliReadiness, RunnerAuthMode, RuntimeState, SchemaRange, SessionPaths,
};
use assistant_session::{
    current_outbound_compat, enqueue_inbound_keyed, init_session, mark_delivered,
    max_delivered_seq, read_outbound, session_exists, InboundMessage, OutboundMessage,
    SessionError, SessionLayout, CURRENT_OUTBOUND_VERSION,
};

use crate::error::HostError;
use crate::qmd::{NodeQmd, QmdSidecar};

/// Tunables for the run-loop. Defaults match the production cadence; tests
/// shrink the intervals so the offline loop completes fast.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Base agent image to spawn (`assistant-base:<version>`).
    pub image: ImageRef,
    /// Roots every session mount must live under (the sessions directory).
    pub allowed_roots: Vec<PathBuf>,
    /// Idle/stale heartbeat thresholds used for death detection.
    pub policy: LifecyclePolicy,
    /// How often to re-read `outbound.db` while waiting for a reply.
    pub poll_interval: Duration,
    /// Backstop: give up on a turn after this long with no reply.
    pub turn_timeout: Duration,
    /// Stub (no creds) or real Claude OAuth (OneCLI-gated).
    pub auth_mode: RunnerAuthMode,
    /// OneCLI readiness; only consulted on the ClaudeOAuth path.
    pub onecli: OneCliReadiness,
    /// This installation's identifier — the instance directory name (e.g.
    /// `cleoclaw`, `assistant`, `assistant-work`). Always names the per-session
    /// container (`{agent}-{session}`); additionally, on the ClaudeOAuth path it
    /// is the OneCLI agent whose container config the run-loop fetches.
    pub onecli_agent: String,
    /// Directory the fetched OneCLI CA is written to before being mounted into
    /// the container. Only used on the ClaudeOAuth path.
    pub onecli_ca_dir: PathBuf,
    /// Protocol version this run declares to the shim.
    pub declared_protocol: String,
    /// Protocol versions the configured shim advertises support for.
    pub shim_supported: Vec<String>,
    /// Catalog-backed memory retrieval for the turn seam. When set, each turn
    /// loads this agent group's eligible catalog entries and injects a rendered
    /// `<retrieved_memories>` block into the inbound message's metadata. `None`
    /// disables injection (the default; offline loop tests that don't seed a
    /// catalog leave it off).
    pub memory: Option<MemoryRetrieval>,
    /// Extra environment passed straight into the container spec, appended after
    /// the auth-path env. The orchestrator path carries the `ASSISTANT_SPECIALISTS`
    /// routing menu; the specialist path carries its spec's per-image env plus the
    /// generic `ASSISTANT_SPECIALIST_*` turn config.
    pub extra_env: Vec<(String, String)>,
}

/// Where and how a turn loads its pre-reply memory context. The catalog table
/// lives in the installation's central DB; `agent_group_id` is the hard
/// isolation boundary the catalog query is scoped to. This is the catalog-only
/// (host-side eligibility) layer — qmd semantic ranking composes on top later by
/// intersecting on `memory_id`, so this is the floor, not the ceiling.
#[derive(Clone, Debug)]
pub struct MemoryRetrieval {
    pub central_db_path: PathBuf,
    pub agent_group_id: i64,
    pub limit: usize,
    /// Root for this installation's per-agent memory trees (`<root>/groups`).
    /// The host reads each retrieved entry's body from here to hydrate the
    /// injected block, and writes new `save_memory` notes under it.
    pub groups_dir: PathBuf,
    /// The owning agent group's logical string id (the front-matter
    /// `owner_agent_group_id`), which selects its memory root on disk.
    pub owner: String,
    /// The optional qmd semantic-ranking layer. When set and healthy, each turn
    /// ranks this agent's eligible catalog entries by relevance to the message
    /// (and supplies snippets) instead of injecting the unranked catalog floor.
    /// Any degraded qmd state (missing sidecar, timeout, malformed output) falls
    /// back to the catalog-only path — qmd composes on top, it is never required.
    pub qmd: Option<QmdSidecar>,
}

impl HostConfig {
    /// A config with production-cadence defaults for the given image, mount
    /// allowlist, auth mode, and OneCLI readiness.
    pub fn new(
        image: ImageRef,
        allowed_roots: Vec<PathBuf>,
        auth_mode: RunnerAuthMode,
        onecli: OneCliReadiness,
    ) -> Self {
        Self {
            image,
            allowed_roots,
            policy: LifecyclePolicy::new(Duration::from_secs(60), Duration::from_secs(300)),
            poll_interval: Duration::from_millis(250),
            turn_timeout: Duration::from_secs(120),
            auth_mode,
            onecli,
            onecli_agent: String::new(),
            onecli_ca_dir: PathBuf::new(),
            declared_protocol: RUNNER_PROTOCOL_VERSION.to_string(),
            shim_supported: vec![RUNNER_PROTOCOL_VERSION.to_string()],
            memory: None,
            extra_env: Vec::new(),
        }
    }

    /// Enable catalog-backed memory injection for every turn, reading this
    /// agent group's eligible entries from the central DB at `central_db_path`
    /// and hydrating their bodies from the on-disk root under `groups_dir`.
    pub fn with_memory(
        mut self,
        central_db_path: PathBuf,
        agent_group_id: i64,
        limit: usize,
        groups_dir: PathBuf,
        owner: String,
    ) -> Self {
        self.memory = Some(MemoryRetrieval {
            central_db_path,
            agent_group_id,
            limit,
            groups_dir,
            owner,
            qmd: None,
        });
        self
    }

    /// Layer qmd semantic ranking onto an already-configured memory retrieval.
    /// No-op when [`Self::with_memory`] has not been called (qmd has nothing to
    /// rank without the catalog floor it composes on top of).
    pub fn with_qmd_sidecar(mut self, sidecar: QmdSidecar) -> Self {
        if let Some(memory) = self.memory.as_mut() {
            memory.qmd = Some(sidecar);
        }
        self
    }

    /// Set this installation's identifier and the OneCLI CA directory. The agent
    /// always names the per-session container; on the ClaudeOAuth path the
    /// run-loop also queries this installation's gateway for the agent's
    /// container config and writes the returned CA under `ca_dir` before mounting
    /// it (the CA dir is unused on the stub path).
    pub fn with_onecli_agent(mut self, agent: String, ca_dir: PathBuf) -> Self {
        self.onecli_agent = agent;
        self.onecli_ca_dir = ca_dir;
        self
    }
}

/// Drives one session: holds the session layout (inbound/outbound DBs), the
/// runtime (spawn/stop), the spawned container id, and the high-water mark of
/// delivered outbound seqs.
///
/// The host is channel-agnostic: it enqueues a neutral [`InboundMessage`] and
/// returns the [`OutboundMessage`]s a turn produces. Where the inbound comes
/// from (a terminal line, a Slack event) and where the reply goes (stdout, a
/// `chat.postMessage`) is the caller's concern.
pub struct Host<R: ContainerRuntime> {
    layout: SessionLayout,
    runtime: R,
    config: HostConfig,
    container: Option<ContainerId>,
    last_delivered_seq: i64,
    started: bool,
}

impl<R> Host<R>
where
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
{
    pub fn new(layout: SessionLayout, runtime: R, config: HostConfig) -> Self {
        Self {
            layout,
            runtime,
            config,
            container: None,
            last_delivered_seq: 0,
            started: false,
        }
    }

    /// This session's stable id — its validated folder name. Used to name the
    /// per-session container.
    fn session_id(&self) -> String {
        self.layout
            .dir()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    }

    /// The deterministic container name for this session, `{agent}-{session}`
    /// (also accepted by `docker stop`). The agent is this installation's
    /// identifier (the instance directory name, e.g. `cleoclaw`, `assistant`,
    /// `assistant-work`) so two installations sharing a session id (the common
    /// `default`) never collide on one Docker name; the session part is the
    /// validated session folder name.
    fn container_name(&self) -> String {
        format!("{}-{}", self.config.onecli_agent, self.session_id())
    }

    /// Initialize the session folder/DBs once, lazily, before the first turn.
    /// Also hydrate the delivered watermark from disk: a fresh `Host` starts at 0,
    /// but a process restart mid-session would otherwise re-read (and re-deliver)
    /// the prior turn's outbound. Resuming from the persisted `delivered` marker
    /// keeps delivery at-most-once across restarts.
    fn ensure_started(&mut self) -> Result<(), HostError> {
        if !self.started {
            // Both ops touch the session DBs the container also writes (a job
            // specialist's first turn races its container's own connections). Like
            // the enqueue/mark-delivered writes below, absorb transient
            // `SQLITE_IOERR` through a bounded retry; both are idempotent
            // (`init_session` re-creates dirs/schema as a no-op and upserts
            // `container_state`; `max_delivered_seq` is a pure read).
            if !session_exists(&self.layout) {
                retry_transient(|| init_session(&self.layout))?;
            }
            self.last_delivered_seq = retry_transient(|| max_delivered_seq(&self.layout))?;
            self.started = true;
        }
        Ok(())
    }

    /// Time since the container last touched its heartbeat, or `None` when no
    /// heartbeat exists yet (booting or stopped).
    fn since_heartbeat(&self) -> Option<Duration> {
        let path = self.layout.heartbeat_path();
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
        // A just-written heartbeat can read as "in the future" under clock skew;
        // treat that as fresh rather than missing.
        Some(modified.elapsed().unwrap_or(Duration::ZERO))
    }

    /// Spawn the per-session container once. Re-checks the protocol contract and
    /// builds/validates mounts and auth env via `prepare_spawn`; any refusal
    /// aborts before a container is started.
    fn ensure_spawned(&mut self) -> Result<(), HostError> {
        if self.container.is_some() {
            return Ok(());
        }

        let shim_supported: Vec<&str> =
            self.config.shim_supported.iter().map(String::as_str).collect();
        check_runner_protocol(&self.config.declared_protocol, &shim_supported)?;

        let layout = &self.layout;
        // Docker bind-mounts each session dir/file by path, so every mount source
        // must already exist on the host before `docker run`. `init_session`
        // creates the inbox/outbox dirs (and DBs) on first turn, but it is skipped
        // once `inbound.db` exists (`session_exists`). A session left partial — DB
        // present but inbox/outbox dirs gone (a manual reset, a file-sharing
        // hiccup) — would otherwise reach `docker run` with a missing bind source
        // and fail with "bind source path does not exist". Re-assert the managed
        // dirs idempotently so any spawn can rebuild them regardless of how the
        // session was left.
        for dir in layout.managed_dirs() {
            std::fs::create_dir_all(&dir)?;
        }
        // The heartbeat is runner-owned (its presence is the liveness signal) and
        // so does not exist yet. Lay (or refresh) an empty placeholder with a
        // current mtime: a heartbeat left by a previous process can be arbitrarily
        // old (e.g. a daemon down past `stale_after` between runs), and judging the
        // about-to-boot container against that stale mtime would reap it on the
        // first poll. Truncating in place keeps the inode stable so the bind mount
        // still resolves; the runner overwrites it once it starts.
        let heartbeat = layout.heartbeat_path();
        std::fs::File::create(&heartbeat)?;
        let paths = SessionPaths {
            inbound_db: layout.inbound_db_path(),
            outbound_db: layout.outbound_db_path(),
            heartbeat: layout.heartbeat_path(),
            inbox_dir: layout.inbox_dir(),
            outbox_dir: layout.outbox_dir(),
        };
        let mounts = build_session_mounts(&paths, None);
        let mut spec = prepare_spawn(
            self.container_name(),
            self.config.image.clone(),
            mounts,
            &self.config.allowed_roots,
            CURRENT_OUTBOUND_VERSION,
            SchemaRange::new(1, CURRENT_OUTBOUND_VERSION),
            self.config.auth_mode,
            self.config.onecli,
        )?;
        // On every Claude-credentialed path (the orchestrator's ClaudeOAuth and
        // the specialist, which also runs a real Claude turn) the container is
        // spawned with only a placeholder token; the real credential lives in
        // this installation's OneCLI gateway, which rewrites the placeholder on
        // outbound traffic. Query the gateway for this agent's proxy env + CA and
        // apply them to the spec. This runs after `prepare_spawn` on purpose: the
        // CA mount must bypass `validate_mounts`' `.pem` block (it's a public
        // trust anchor, not a secret). `prepare_spawn` already refused the spawn
        // if OneCLI was not ready, so reaching here means a gateway query is
        // expected to succeed.
        if matches!(
            self.config.auth_mode,
            RunnerAuthMode::ClaudeOAuth | RunnerAuthMode::Specialist
        ) {
            crate::onecli::apply_gateway_config(
                &mut spec,
                &self.config.onecli_ca_dir,
                &self.config.onecli_agent,
            )?;
        }
        // Caller-supplied env (e.g. the specialist's network policy) is appended
        // last so it travels into the container alongside the auth/gateway env.
        spec.env
            .extend(self.config.extra_env.iter().cloned());
        let id = self
            .runtime
            .spawn(&spec)
            .map_err(|e| HostError::Runtime(e.to_string()))?;
        self.container = Some(id);
        Ok(())
    }

    /// The combined context preamble to inject into this turn's inbound metadata,
    /// or `None` when memory injection is disabled, the caller already set
    /// metadata, or nothing composed. Concatenates (blank-line separated) the
    /// memory block and the agent's `<active_schedules>` block — both optional —
    /// from a single central-DB connection. The shim prepends the whole thing as
    /// context ahead of the user's message. Fail-open: a DB error skips injection.
    fn context_block(&self, inbound: &InboundMessage) -> Option<String> {
        let cfg = self.config.memory.as_ref()?;
        if inbound.metadata.is_some() {
            return None;
        }
        let conn = match open_central(&cfg.central_db_path) {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("context injection skipped (open central db: {e})");
                return None;
            }
        };

        let mut blocks: Vec<String> = Vec::new();
        if let Some(block) = self.memory_block(cfg, &conn, &inbound.content) {
            blocks.push(block);
        }
        // The agent's active schedules, so it can answer "what's scheduled?" and
        // knows each item's id to pass to cancel_schedule. Agent-group scoped (the
        // instance is the isolation boundary), capped at the same limit as memory.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(block) =
            crate::scheduler::render_active_schedules_block(&conn, cfg.agent_group_id, now, cfg.limit)
        {
            blocks.push(block);
        }

        if blocks.is_empty() {
            None
        } else {
            Some(blocks.join("\n\n"))
        }
    }

    /// The rendered `<retrieved_memories>` block for this turn, or `None` when
    /// nothing eligible matched or retrieval failed (fail-open — never an error).
    /// When a healthy qmd sidecar is configured the block is its relevance ranking
    /// (with snippets); otherwise — and on any degraded qmd state — it is the
    /// unranked catalog floor. The caller owns opening the central connection.
    fn memory_block(&self, cfg: &MemoryRetrieval, conn: &Connection, query: &str) -> Option<String> {
        // Semantic layer first: when qmd is configured and healthy its ranked
        // result is authoritative (even an empty one suppresses injection). Only
        // a degraded/unconfigured qmd falls through to the catalog-only floor.
        if let Some(block) = self.qmd_memory_block(cfg, conn, query) {
            return block;
        }
        // Retrieval is intentionally unscoped: the isolation boundary is the
        // instance (a separate instance per person/household), not a per-entry
        // scope. A default context selects every `all_chats` entry for the agent
        // group — which is all the host writes. The scope-filtering machinery in
        // `assistant_memory` is kept as latent capability, not a TODO to finish here.
        let envelopes = match retrieve(conn, cfg.agent_group_id, &RetrievalContext::default(), cfg.limit)
        {
            Ok(envelopes) => envelopes,
            Err(e) => {
                eprintln!("memory injection skipped ({e})");
                return None;
            }
        };
        // Catalog retrieval yields metadata-only envelopes; read each entry's
        // body off disk so the injected block carries the remembered text. This
        // is what qmd would otherwise supply via ranked snippets.
        let envelopes = crate::memory::hydrate_snippets(envelopes, &cfg.groups_dir, &cfg.owner);
        render_memory_block(&envelopes, false)
    }

    /// The qmd-ranked memory block for this turn, if qmd should own retrieval.
    ///
    /// Returns `Some(block)` when qmd ran and its result is authoritative — the
    /// inner `Option` is the rendered block, where `None` means "ranked, nothing
    /// relevant, inject nothing". Returns the outer `None` when qmd is
    /// unconfigured or degraded (missing sidecar, timeout, malformed output, or a
    /// catalog read error), so the caller falls back to the catalog-only floor.
    /// Snippets come from the qmd hit, so no on-disk hydration is needed.
    fn qmd_memory_block(
        &self,
        cfg: &MemoryRetrieval,
        conn: &Connection,
        query: &str,
    ) -> Option<Option<String>> {
        let sidecar = cfg.qmd.as_ref()?;
        let candidates = match entries_for_agent(conn, cfg.agent_group_id) {
            Ok(candidates) => candidates,
            Err(e) => {
                eprintln!("qmd memory ranking skipped, falling back to catalog ({e})");
                return None;
            }
        };
        let backend = NodeQmd::new(sidecar.clone());
        let outcome = backend.search(query, cfg.limit);
        if outcome.is_degraded() {
            eprintln!("qmd search degraded; falling back to catalog-only memory");
            return None;
        }
        let (envelopes, _) =
            inject_from_search(&candidates, &outcome, &RetrievalContext::default(), cfg.limit);
        Some(render_memory_block(&envelopes, false))
    }

    /// Run one turn end-to-end: enqueue the inbound message, ensure the
    /// container is up, poll `outbound.db` for the reply, mark each new message
    /// delivered, and return them (in seq order). Channel-agnostic — the caller
    /// renders/delivers the returned messages however its channel requires.
    ///
    /// `mark_delivered` advances the host/container protocol's delivered marker
    /// as soon as a message is read here; a caller whose downstream delivery can
    /// fail (e.g. a Slack post) therefore gets at-most-once delivery for now.
    pub fn run_turn(
        &mut self,
        inbound: &InboundMessage,
    ) -> Result<Vec<OutboundMessage>, HostError> {
        self.run_turn_keyed(inbound, None)
    }

    /// Run a turn whose inbound enqueue is idempotent on `idempotency_key`. A
    /// scheduler whose previous attempt failed (the lease expired and the
    /// occurrence is re-claimed) calls this with the occurrence's stable key so
    /// the retry reuses the one inbound row instead of duplicating it. `None`
    /// behaves exactly like [`Self::run_turn`].
    pub fn run_turn_keyed(
        &mut self,
        inbound: &InboundMessage,
        idempotency_key: Option<&str>,
    ) -> Result<Vec<OutboundMessage>, HostError> {
        self.ensure_started()?;

        // Pre-reply context: load the memory block + the active-schedules block
        // and carry them in the inbound metadata (the shim injects it as context).
        // A caller that already set metadata is left untouched. Fail-open — a
        // retrieval error never blocks the turn.
        let enriched;
        let inbound = match self.context_block(inbound) {
            Some(block) => {
                enriched = InboundMessage {
                    metadata: Some(block),
                    ..inbound.clone()
                };
                &enriched
            }
            None => inbound,
        };

        retry_transient(|| enqueue_inbound_keyed(&self.layout, inbound, idempotency_key))?;
        self.ensure_spawned()?;

        let started = Instant::now();
        loop {
            // Heartbeat staleness is the death signal. A missing heartbeat right
            // after spawn means "still booting" (Stopped), not dead — keep
            // waiting until the backstop deadline. On death, reap the container
            // and clear our handle so the host is recoverable: the next turn's
            // `ensure_spawned` (which early-returns while a handle is held) will
            // spawn a fresh container instead of polling the corpse forever.
            if classify(self.since_heartbeat(), self.config.policy) == RuntimeState::Stale {
                self.reap_for_respawn();
                return Err(HostError::ContainerDied {
                    detail: "heartbeat went stale while awaiting a reply".to_string(),
                });
            }

            let messages = match read_outbound(&self.layout, current_outbound_compat()) {
                Ok(messages) => messages,
                // Transient contention over the bind mount while the container
                // commits: read as "nothing yet" and keep polling. Real errors
                // (schema, io) propagate.
                Err(SessionError::Sqlite(_)) => Vec::new(),
                Err(e) => return Err(HostError::Session(e)),
            };

            let new: Vec<OutboundMessage> = messages
                .into_iter()
                .filter(|m| m.seq > self.last_delivered_seq)
                .collect();
            if !new.is_empty() {
                for message in &new {
                    retry_transient(|| mark_delivered(&self.layout, message.seq))?;
                    self.last_delivered_seq = self.last_delivered_seq.max(message.seq);
                }
                return Ok(new);
            }

            if started.elapsed() >= self.config.turn_timeout {
                return Err(HostError::Timeout {
                    seconds: self.config.turn_timeout.as_secs(),
                });
            }
            std::thread::sleep(self.config.poll_interval);
        }
    }

    /// Terminal convenience: enqueue a raw line as the local sender, run the
    /// turn, and render each reply to `out`. Returns how many messages were
    /// delivered. Mirrors the prior terminal behavior exactly.
    pub fn process_turn(&mut self, line: &str, out: &mut dyn Write) -> Result<usize, HostError> {
        let inbound = InboundMessage {
            sender: local_sender(),
            content: line.trim_end_matches(['\n', '\r']).to_string(),
            metadata: None,
        };
        let delivered = self.run_turn(&inbound)?;
        for message in &delivered {
            writeln!(out, "{}", render_message(message))?;
        }
        Ok(delivered.len())
    }

    /// Reap a dead container and reset to a clean, spawnable state. Best-effort
    /// stop (the container has already exited; with `--rm` Docker has removed it,
    /// so a stop error here is benign and ignored), drop our handle, and delete
    /// the stale heartbeat so it is not re-read as another death before the next
    /// container lays its own.
    fn reap_for_respawn(&mut self) {
        if let Some(id) = self.container.take() {
            let _ = self.runtime.stop(&id);
        }
        let _ = std::fs::remove_file(self.layout.heartbeat_path());
    }

    /// Stop the session container if one is running. Idempotent.
    pub fn shutdown(&mut self) -> Result<(), HostError> {
        if let Some(id) = self.container.take() {
            self.runtime
                .stop(&id)
                .map_err(|e| HostError::Runtime(e.to_string()))?;
        }
        Ok(())
    }

    /// Test/inspection accessor: the spawned container id, if any.
    pub fn container_id(&self) -> Option<&ContainerId> {
        self.container.as_ref()
    }

    /// Test/inspection accessor: the underlying runtime (e.g. to assert spawn/
    /// stop bookkeeping on `FakeRuntime`).
    pub fn runtime(&self) -> &R {
        &self.runtime
    }
}

/// Retry a one-shot session write through transient SQLite contention.
///
/// Session DBs run `journal_mode=DELETE` over a bind mount the container also
/// writes; concurrent access can surface a transient `SQLITE_IOERR_LOCK` (a
/// failed POSIX advisory-lock acquisition) that `busy_timeout` does NOT retry —
/// it only covers `SQLITE_BUSY`/`LOCKED`. The poll loop already absorbs these on
/// the read side by re-reading next tick; a one-shot write (enqueue inbound,
/// mark delivered) needs its own short bounded retry. Non-transient errors
/// (schema, real IO) propagate on the first occurrence.
fn retry_transient<T>(mut op: impl FnMut() -> Result<T, SessionError>) -> Result<T, SessionError> {
    const RETRIES: u32 = 4;
    for attempt in 0..RETRIES {
        match op() {
            Err(SessionError::Sqlite(_)) => {
                std::thread::sleep(Duration::from_millis(10 * u64::from(attempt + 1)));
            }
            other => return other,
        }
    }
    op()
}
