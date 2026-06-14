// Session DB protocol — the byte-for-byte runner side of the host/container
// contract defined by `assistant-session` (see crates/assistant-session/src/control.rs,
// `FakeContainer`). The container owns `outbound.db`, reads `inbound.db`
// read-only, maintains the heartbeat file and `container_state`, and records
// `processing_ack` claims. Any divergence here breaks `verify_sequence_parity`
// on the host: host messages take even seqs, container replies take odd seqs.
//
// Session DBs run `journal_mode=DELETE` (not WAL) so host/container mount
// visibility is predictable. Each operation opens its own short-lived
// connection — exactly as the Rust `FakeContainer` does — and is wrapped in a
// bounded retry, because concurrent host/container access over a bind mount can
// surface transient `SQLITE_BUSY`/`SQLITE_IOERR_LOCK` that `busy_timeout` alone
// does not cover.

import { DatabaseSync } from 'node:sqlite';
import { writeFileSync, rmSync } from 'node:fs';

const SESSION_DIR = process.env.ASSISTANT_SESSION_DIR ?? '/session';
export const INBOUND_DB = `${SESSION_DIR}/inbound.db`;
export const OUTBOUND_DB = `${SESSION_DIR}/outbound.db`;
export const HEARTBEAT = `${SESSION_DIR}/.heartbeat`;

// Schema versions this runner can read/write (mirrors CURRENT_OUTBOUND_VERSION
// and the host's SchemaRange::new(1, CURRENT_OUTBOUND_VERSION)).
const SUPPORTED_MIN = 1;
const SUPPORTED_MAX = 2;

// SQLite primary result codes that indicate transient contention worth a retry.
// We mask the extended code to its low byte so e.g. SQLITE_IOERR_LOCK (3850)
// reduces to SQLITE_IOERR (10).
const TRANSIENT = new Set([5 /* BUSY */, 6 /* LOCKED */, 10 /* IOERR */]);

function isTransient(err) {
  const code = err?.errcode;
  return typeof code === 'number' && TRANSIENT.has(code & 0xff);
}

async function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Retry a one-shot DB op through transient lock/IO blips before surfacing.
async function withRetry(op) {
  const RETRIES = 4;
  for (let attempt = 0; attempt < RETRIES; attempt += 1) {
    try {
      return op();
    } catch (err) {
      if (!isTransient(err)) throw err;
      await sleep(10 * (attempt + 1));
    }
  }
  return op();
}

function openRw(path) {
  const db = new DatabaseSync(path);
  db.exec('PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;');
  return db;
}

function openRo(path) {
  const db = new DatabaseSync(path, { readOnly: true });
  db.exec('PRAGMA busy_timeout = 5000;');
  return db;
}

function schemaVersion(db) {
  const row = db.prepare('SELECT schema_version FROM schema_meta WHERE id = 1').get();
  return row ? Number(row.schema_version) : 0;
}

export class Session {
  // Lay the container alive and write a fresh heartbeat.
  async start(runId) {
    await this.setStatus('alive', runId);
    this.heartbeat();
  }

  // Touch the heartbeat file to "now" (mtime is the host's liveness signal).
  heartbeat() {
    writeFileSync(HEARTBEAT, 'alive');
  }

  // Read inbound messages the way the host expects: read-only, ordered by seq.
  // `metadata` carries the host-injected `<retrieved_memories>` block (see
  // crates/assistant-host/src/run.rs `memory_block`); it is a side-channel separate
  // from `content`, consumed only by the Claude path.
  async readInbound() {
    return withRetry(() => {
      const db = openRo(INBOUND_DB);
      try {
        return db
          .prepare('SELECT seq, content, metadata FROM messages_in ORDER BY seq')
          .all()
          .map((r) => ({ seq: Number(r.seq), content: r.content, metadata: r.metadata ?? null }));
      } finally {
        db.close();
      }
    });
  }

  // Emit an outbound message at the next odd seq (1, then MAX(seq)+2).
  async emit(kind, content) {
    return withRetry(() => {
      const db = openRw(OUTBOUND_DB);
      try {
        const found = schemaVersion(db);
        if (found < SUPPORTED_MIN || found > SUPPORTED_MAX) {
          throw new Error(
            `unsupported outbound schema version ${found} (supported ${SUPPORTED_MIN}..=${SUPPORTED_MAX})`,
          );
        }
        const row = db.prepare('SELECT MAX(seq) AS m FROM messages_out').get();
        const max = row && row.m != null ? Number(row.m) : null;
        const seq = max == null ? 1 : max + 2;
        db.prepare(
          `INSERT INTO messages_out (seq, kind, content, metadata, created_at)
           VALUES (?, ?, ?, NULL, datetime('now'))`,
        ).run(seq, kind, content);
        return seq;
      } finally {
        db.close();
      }
    });
  }

  // Emit several outbound rows in ONE transaction, each at the next odd seq, AND
  // record the `processing_ack` for `inSeq` in that same transaction. Committing
  // the ack atomically with the reply makes it a crash-safe "this inbound was
  // fully processed" marker: a container that dies mid-turn (before this commit)
  // leaves no ack, so a respawn reprocesses that inbound rather than skipping a
  // turn that never produced a reply. A host poll over the bind mount sees all of
  // a turn's rows together (never a partial turn). `rows` is
  // `[{ kind, content }, ...]`; returns the assigned seqs. A turn that both
  // schedules work and replies emits the schedule rows and the reply text here as
  // a single unit.
  async emitBatch(rows, inSeq, claimedBy) {
    if (rows.length === 0) return [];
    return withRetry(() => {
      const db = openRw(OUTBOUND_DB);
      try {
        const found = schemaVersion(db);
        if (found < SUPPORTED_MIN || found > SUPPORTED_MAX) {
          throw new Error(
            `unsupported outbound schema version ${found} (supported ${SUPPORTED_MIN}..=${SUPPORTED_MAX})`,
          );
        }
        const head = db.prepare('SELECT MAX(seq) AS m FROM messages_out').get();
        let seq = head && head.m != null ? Number(head.m) + 2 : 1;
        const insert = db.prepare(
          `INSERT INTO messages_out (seq, kind, content, metadata, created_at)
           VALUES (?, ?, ?, NULL, datetime('now'))`,
        );
        const ack = db.prepare(
          `INSERT OR REPLACE INTO processing_ack (in_seq, claimed_by, claimed_at)
           VALUES (?, ?, datetime('now'))`,
        );
        const seqs = [];
        db.exec('BEGIN IMMEDIATE');
        try {
          for (const { kind, content } of rows) {
            insert.run(seq, kind, content);
            seqs.push(seq);
            seq += 2;
          }
          ack.run(inSeq, claimedBy);
          db.exec('COMMIT');
        } catch (err) {
          db.exec('ROLLBACK');
          throw err;
        }
        return seqs;
      } finally {
        db.close();
      }
    });
  }

  // The inbound seqs already fully processed (reply committed with its ack).
  // Seeded into the runner's in-memory `handled` set at startup so a respawned
  // container does not reprocess inbound a prior container already answered.
  async claimedSeqs() {
    return withRetry(() => {
      const db = openRo(OUTBOUND_DB);
      try {
        return db
          .prepare('SELECT in_seq FROM processing_ack')
          .all()
          .map((r) => Number(r.in_seq));
      } finally {
        db.close();
      }
    });
  }

  // Mark stopped and remove the heartbeat (idempotent on a missing file).
  async stop() {
    await this.setStatus('stopped', null);
    try {
      rmSync(HEARTBEAT);
    } catch (err) {
      if (err?.code !== 'ENOENT') throw err;
    }
  }

  async setStatus(status, runId) {
    return withRetry(() => {
      const db = openRw(OUTBOUND_DB);
      try {
        db.prepare(
          `INSERT INTO container_state (id, status, run_id, updated_at)
           VALUES (1, ?, ?, datetime('now'))
           ON CONFLICT(id) DO UPDATE SET
               status = excluded.status,
               run_id = excluded.run_id,
               updated_at = excluded.updated_at`,
        ).run(status, runId);
      } finally {
        db.close();
      }
    });
  }
}
