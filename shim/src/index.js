// assistant-base entrypoint: the persistent per-session runner.
//
// One container serves one session for its lifetime. It lays a heartbeat, polls
// `inbound.db` read-only for new host messages (even seqs), processes each
// exactly once, and emits a single reply row to `outbound.db` (odd seqs). The
// host's "turn done" signal is that new odd-seq row; its death signal is a stale
// heartbeat. See crates/assistant-host/src/run.rs for the host side.
//
// Mode is selected by ASSISTANT_RUNNER_MODE (set by the runtime's auth path):
//   - "stub"            : echo the message back (no credentials; proves plumbing).
//   - "claude_oauth"    : run one real Claude turn via the Agent SDK, through the
//                         OneCLI proxy that swaps the placeholder OAuth token.
//   - "specialist"      : a specialist sub-agent that runs its own real Claude
//                         turn (credentialed like claude_oauth) with a restricted
//                         toolset supplied entirely by the host via ASSISTANT_SPECIALIST_*
//                         env; the harness is specialist-agnostic.

import { Session } from './session.js';

const MODE = process.env.ASSISTANT_RUNNER_MODE ?? 'stub';
const RUN_ID =
  process.env.ASSISTANT_RUN_ID ?? `run-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
const POLL_INTERVAL_MS = Number(process.env.ASSISTANT_POLL_INTERVAL_MS ?? 250);

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Resolve the per-message responder for the active mode. Both modes return the
// same shape — `{ text, scheduled, cancellations, memories, delegations }` — so
// the loop emits a turn's reply and any side-effect requests as one atomic batch
// regardless of mode. The Claude path is
// imported lazily so stub mode never loads (or requires) the Agent SDK.
// `memory` is the host-injected `<retrieved_memories>` block (or null); only the
// Claude path consumes it — echo mode just proves plumbing.
async function buildResponder() {
  if (MODE === 'claude_oauth') {
    const { runClaudeTurn } = await import('./claude.js');
    return (content, memory) => runClaudeTurn(content, memory);
  }
  if (MODE === 'specialist') {
    const { runSpecialistTurn } = await import('./specialist.js');
    return (content) => runSpecialistTurn(content);
  }
  return async (content) => ({
    text: `echo: ${content}`,
    scheduled: [],
    cancellations: [],
    memories: [],
  });
}

async function main() {
  const session = new Session();
  const respond = await buildResponder();
  const handled = new Set();
  let running = true;

  const shutdown = async () => {
    if (!running) return;
    running = false;
    try {
      await session.stop();
    } catch (err) {
      console.error(`shutdown: failed to stop cleanly: ${err.message}`);
    }
    process.exit(0);
  };
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);

  await session.start(RUN_ID);
  console.error(`assistant-base up: mode=${MODE} run_id=${RUN_ID}`);

  // Resume cross-restart dedup: skip inbound a prior container already fully
  // processed (its ack committed atomically with its reply). Without this a
  // respawn reprocesses the entire inbound history and re-emits stale replies.
  try {
    for (const seq of await session.claimedSeqs()) handled.add(seq);
  } catch (err) {
    console.error(`resume processed set failed: ${err.message}`);
  }

  while (running) {
    session.heartbeat();

    let inbound = [];
    try {
      inbound = await session.readInbound();
    } catch (err) {
      // Transient read contention already retried in Session; a hard failure
      // here is logged and re-tried next tick rather than crashing the runner.
      console.error(`read_inbound failed: ${err.message}`);
    }

    for (const { seq, content, metadata } of inbound) {
      if (handled.has(seq)) continue;

      // Refresh liveness before each turn, not just once per outer iteration: a
      // batch of several unacked inbound runs its turns back-to-back, and a long
      // batch could otherwise let the heartbeat go stale mid-batch and have the
      // host reap the container before the later turns run.
      session.heartbeat();

      let result;
      try {
        result = await respond(content, metadata);
      } catch (err) {
        // Surface the failure as the turn's reply so the host doesn't hang
        // waiting for a row that will never come.
        result = { text: `error: ${err.message}`, scheduled: [] };
        console.error(`turn ${seq} failed: ${err.message}`);
      }

      // Emit the turn's side-effect requests and its reply text as ONE atomic
      // batch so a host poll over the bind mount never observes a partial turn (a
      // schedule/memory row without its reply, or vice versa). Side-effect rows
      // carry the serialized request; the host intercepts them (records a
      // scheduled item / writes a memory note) and does not post them — the text
      // row is the user-facing confirmation. Always emit at least one row (empty
      // text if the turn produced nothing) so the host's "turn done" signal — a
      // new odd-seq row — always lands.
      const rows = result.scheduled.map((s) => ({
        kind: 'schedule_message',
        content: JSON.stringify(s),
      }));
      for (const cancellation of result.cancellations ?? []) {
        rows.push({ kind: 'cancel_schedule', content: JSON.stringify(cancellation) });
      }
      for (const memory of result.memories ?? []) {
        rows.push({ kind: 'save_memory', content: JSON.stringify(memory) });
      }
      for (const delegation of result.delegations ?? []) {
        rows.push({ kind: 'delegate', content: JSON.stringify(delegation) });
      }
      if (result.text && result.text.length > 0) {
        rows.push({ kind: 'text', content: result.text });
      }
      if (rows.length === 0) rows.push({ kind: 'text', content: '' });

      try {
        await session.emitBatch(rows, seq, 'assistant-base');
        // Mark handled ONLY once the batch is committed: a dropped emit must be
        // retried next tick, never silently lost. The ack is written in the same
        // transaction, so the in-memory and persisted dedup markers never diverge.
        handled.add(seq);
      } catch (err) {
        console.error(`emit for seq ${seq} failed, will retry: ${err.message}`);
      }
    }

    await sleep(POLL_INTERVAL_MS);
  }
}

main().catch((err) => {
  console.error(`fatal: ${err.stack ?? err.message}`);
  process.exit(1);
});
