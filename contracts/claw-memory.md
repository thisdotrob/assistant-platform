# claw-memory Contract

## Public API
qmd-backed markdown memory roots, reindex loop, pre-reply RAG retrieval, memory-health reporting, and archive-pointer management.

Owns the *pure* qmd backend seam — no process is shelled here. The `MemoryBackend`
trait (`reindex`/`search`/`health`) is the boundary a real (host-shelled) or
`FakeQmd` backend implements; `reindex_root` walks an agent's markdown root into
`IndexDoc`s and drives `reindex`; `parse_qmd_response` turns the sidecar's
search-hits JSON into a `SearchOutcome` (`Hits`/`Degraded`), recovering each hit's
`memory_id` from its `displayPath` stem; `corpus_stem_for_memory_id` is the shared
contract for that recovery — the corpus writer (in `claw-host`) names each file by
the hex encoding of its memory id, a fixed point of qmd's path normalization, and
the parser decodes the stem back, so the id survives the round trip rather than
being mangled by lowercasing/separator folding; and
`DegradedReason::from_wire` is the public, shared wire vocabulary (e.g.
`corrupt_fts`, `empty_index`) so a backend's status parser degrades with the same
names the search parser uses. `inject_from_search` composes a `SearchOutcome` over
catalog `candidates` into ranked `InjectionEnvelope`s for `render_memory_block`.
Everything is fail-open: a degraded or missing backend is reported as health, never
raised, so retrieval falls back to the unranked catalog floor. The process that
actually shells qmd (a node sidecar) lives in `claw-host`, not here, so this crate
stays buildable and unit-testable offline without the native qmd stack.

## Persistence Ownership
Owns memory-health projections and derived metadata/index catalogs via central DB migrations. The markdown memory roots themselves are files, keyed by product-supplied namespace.

## Config
Reads memory root paths and reindex cadence from claw-config; the product supplies the memory taxonomy.

## Events
Emits reindex-completed and memory-health events; surfaces stale-index and missing-root diagnostics.

## CLI/Web Surfaces
None directly; backs the web memory editor and CLI memory-health commands.

## Prompt Fragments
Owns `rag-injection`, which defines pre-reply RAG block placement, citation, and snippet bounds. Products supply the taxonomy, not the mechanics.

## Readiness Checks
Verifies memory roots exist, the qmd index is current, and memory health is within thresholds.

## Conformance Tests
Retrieved snippets are injected before the reply turn and bounded by the configured limit; reindex is idempotent; archive pointers stay readable after migration. `parse_qmd_response` maps a well-formed hits payload to ranked `Hits` and any `{ error }` / malformed / empty output to a `Degraded` outcome (via `from_wire`) rather than panicking; a degraded `SearchOutcome` from `inject_from_search` yields the catalog floor, never an injected block claiming a failed search succeeded.
