/**
 * Host-invoked qmd RAG sidecar for claw v2. The host (`claw_host::qmd::NodeQmd`)
 * shells `node qmd-sidecar.mjs <subcommand>`, writes a JSON request on stdin, and
 * reads a JSON response on stdout. Three subcommands:
 *
 *   retrieve ← { message, topN?, dbPath } → { hits: [...] }
 *   embed    ← { dbPath, corpusDir }      → { indexed }
 *   status   ← { dbPath }                 → { indexed }
 *
 * stdout is reserved for the JSON protocol; console.log is redirected to stderr
 * so qmd's progress/warning chatter never pollutes the channel. Everything is
 * fail-open: any failure is reported as `{ "error": "<reason>" }` on stdout with
 * a zero exit, which the host maps to a degraded-memory state (it then falls back
 * to the catalog-only path) rather than failing the agent turn.
 *
 * The `@tobilu/qmd` import is dynamic (inside the subcommand handlers) so this
 * module can be imported — and its pure helpers unit-tested — without the native
 * qmd stack installed, keeping it in the offline gate via `node --check` +
 * `node --test`.
 */
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

// stdout is the JSON channel; push qmd's console chatter to stderr.
console.log = console.error;

/** Coerce a requested hit count into a sane bounded integer. */
export function clampTopN(raw, def = 8, max = 50) {
  let n = Number(raw);
  if (!Number.isFinite(n)) n = def;
  return Math.min(Math.max(Math.floor(n), 1), max);
}

/** Project qmd search results into the host's hit wire shape (chunk capped). */
export function mapHits(results) {
  return results.map((r) => ({
    displayPath: r.displayPath,
    title: r.title,
    bestChunk: (r.bestChunk ?? '').slice(0, 2000),
    score: r.score,
    docid: r.docid,
    context: r.context,
  }));
}

/**
 * Normalize an arbitrary inbound message into a qmd-safe vec query. qmd's
 * `validateSemanticQuery` rejects two things that occur in ordinary user text:
 * any newline (`queries must be single-line`) and the negation pattern `-\w`/`-"`
 * — which also misfires on innocuous hyphenated words like "sub-agent" or
 * "fail-open". We collapse newlines/whitespace to single spaces and replace every
 * hyphen that precedes a word char or quote with a space, so the query is plain
 * single-line prose with no negation operators left to trip the validator.
 */
export function sanitizeVecQuery(message) {
  return String(message ?? '')
    .replace(/[\r\n]+/g, ' ')
    .replace(/-(?=[\w"])/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/**
 * Extract de-duplicated alphanumeric word tokens from a message, in first-seen
 * order, capped so a long message can't build a pathological FTS query. Dedup is
 * case-insensitive; the original casing is preserved (qmd's tokenizer folds case).
 */
export function lexTokens(message, max = 32) {
  const seen = new Set();
  const tokens = [];
  for (const match of String(message ?? '').matchAll(/[A-Za-z0-9][A-Za-z0-9_]*/g)) {
    const token = match[0];
    const key = token.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    tokens.push(token);
    if (tokens.length >= max) break;
  }
  return tokens;
}

/**
 * Build a qmd FTS lex query from a message: each token double-quoted (so an FTS
 * operator word like `OR`/`NEAR` or a stray special char is treated as a string
 * literal) and OR-joined for recall. Returns '' when there are no tokens, so the
 * caller can omit the lex search and run vec-only.
 */
export function buildLexQuery(message, max = 32) {
  const tokens = lexTokens(message, max);
  if (tokens.length === 0) return '';
  return tokens.map((token) => `"${token}"`).join(' OR ');
}

/** Read and JSON-parse the request on stdin (fd 0); empty stdin is `{}`. */
export function readRequest() {
  const raw = readFileSync(0, 'utf8');
  return JSON.parse(raw.trim() || '{}');
}

function requireDbPath(input) {
  if (typeof input.dbPath !== 'string' || input.dbPath.length === 0) {
    throw new Error('dbPath is required');
  }
  return input.dbPath;
}

async function runRetrieve(input) {
  const message = typeof input.message === 'string' ? input.message : '';
  const topN = clampTopN(input.topN);
  const dbPath = requireDbPath(input);

  const vecQuery = sanitizeVecQuery(message);
  if (!vecQuery) return { hits: [] };

  const { createStore } = await import('@tobilu/qmd');
  const store = await createStore({ dbPath });
  try {
    // Hybrid lex+vec: one embedding-similarity query plus a keyword (FTS) query
    // built from the same message, fused by qmd's structuredSearch via RRF. The
    // vec leg catches paraphrase/semantic matches; the lex leg catches exact terms
    // (names, ids, jargon) the embedding can miss. We deliberately skip
    // store.expandQuery (HyDE) — it cold-loads a ~1.3GB generative model per call,
    // blowing the host's per-turn search timeout; the 333MB embedding model alone
    // serves vec search.
    const vec = { type: 'vec', query: vecQuery };
    const lexQuery = buildLexQuery(message);
    const queries = lexQuery ? [vec, { type: 'lex', query: lexQuery }] : [vec];

    let results;
    try {
      results = await store.search({ queries, rerank: false, limit: topN });
    } catch (searchErr) {
      // The lex leg is the riskier addition; if the hybrid search throws, retry
      // vec-only so a keyword-query problem can't knock out semantic retrieval
      // (which would defeat the whole point of running qmd this turn). A vec-only
      // failure falls through to the module's top-level fail-open.
      if (queries.length > 1) {
        console.error('qmd sidecar: hybrid search failed, retrying vec-only', searchErr);
        results = await store.search({ queries: [vec], rerank: false, limit: topN });
      } else {
        throw searchErr;
      }
    }
    return { hits: mapHits(results) };
  } finally {
    await store.close();
  }
}

async function runEmbed(input) {
  const dbPath = requireDbPath(input);
  if (typeof input.corpusDir !== 'string' || input.corpusDir.length === 0) {
    throw new Error('corpusDir is required');
  }
  const { createStore } = await import('@tobilu/qmd');
  // One collection rooted at the host-written corpus, one markdown file per
  // memory id. The filename is a hex encoding of the id (qmd's path handelize
  // lowercases and maps `_`/punctuation to `-`, which would otherwise mangle the
  // id), so a hit's displayPath stem decodes cleanly back to the memory_id host-side.
  const store = await createStore({
    dbPath,
    config: { collections: { memory: { path: input.corpusDir, pattern: '**/*.md' } } },
  });
  try {
    await store.update();
    await store.embed();
    const status = await store.getStatus();
    return { indexed: status.totalDocuments ?? 0 };
  } finally {
    await store.close();
  }
}

async function runStatus(input) {
  const dbPath = requireDbPath(input);
  const { createStore } = await import('@tobilu/qmd');
  const store = await createStore({ dbPath });
  try {
    const status = await store.getStatus();
    return { indexed: status.totalDocuments ?? 0 };
  } finally {
    await store.close();
  }
}

const HANDLERS = { retrieve: runRetrieve, embed: runEmbed, status: runStatus };

async function main(argv) {
  const subcommand = argv[2];
  const handler = HANDLERS[subcommand];
  if (!handler) {
    process.stdout.write(JSON.stringify({ error: `unknown subcommand: ${subcommand ?? ''}` }) + '\n');
    return;
  }
  let input;
  try {
    input = readRequest();
  } catch (err) {
    process.stdout.write(JSON.stringify({ error: `bad request json: ${err.message}` }) + '\n');
    return;
  }
  try {
    const result = await handler(input);
    process.stdout.write(JSON.stringify(result) + '\n');
  } catch (err) {
    // Fail-open: surface the failure as a degraded signal the host can read,
    // never a nonzero crash that would fail the turn.
    console.error('qmd sidecar error', err);
    process.stdout.write(JSON.stringify({ error: String(err?.message ?? err) }) + '\n');
  }
}

// Only run when invoked directly (`node qmd-sidecar.mjs <sub>`); importing the
// module for unit tests must not execute main.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv);
}
