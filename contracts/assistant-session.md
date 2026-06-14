# assistant-session Contract

## Public API
Per-session folder layout, inbound/outbound DB schemas, sequence parity checks, routing projections, and the shared version-aware session-opening API. Old sessions are read only through this API. Inbound enqueue is available both plain (`enqueue_inbound`) and idempotent (`enqueue_inbound_keyed`): when a caller-supplied key matches an existing row, that row's sequence is returned and nothing is written, so a retried enqueue (e.g. a scheduler re-running the same occurrence) never duplicates the inbound row. NULL keys are unconstrained, so ordinary human messages always insert.

## Persistence Ownership
Owns `inbound.db` and `outbound.db` schemas and their per-session migration bookkeeping. `inbound.db` migrations are host-owned and preserve container read-only access; `outbound.db` is runner-protocol-owned and host-coordinated.

## Config
Owns the per-session DB schema-version declarations (current inbound/outbound versions and the runner protocol version). The host supplies the derived sessions root (read from assistant-config) when deriving a session layout; archive policy is a later milestone.

## Events
Emits session-opened, session-migrated, and sequence-parity-violation events.

## CLI/Web Surfaces
None directly; provides the session-opening API used by CLI inspection and web transcript views.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies session DB schema versions are within the runner's supported range and inbound/outbound sequence parity holds.

## Conformance Tests
Lazy per-session migrations run on open for wake, sweep, inspection, archival, or export; migrations are fixture-tested against old active and archived sessions; a host never starts a runner against an unreadable session DB version.
