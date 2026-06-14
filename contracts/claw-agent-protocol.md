# claw-agent-protocol Contract

## Public API
Inbound/outbound message envelopes, typed outbound actions, MCP action content, attachment contracts, and prompt-input formatting contracts shared by host, runner, CLI, and web UI.

## Persistence Ownership
Owns no database tables. Owns the runner protocol version and envelope schema declarations.

## Config
Owns no config keys; the protocol version is declared in the platform manifest.

## Events
Defines the canonical event envelope translated to and from the session DB protocol; owns the final-text fallback contract.

## CLI/Web Surfaces
None directly; provides the shared Rust protocol types the CLI and web crates serialize against.

## Prompt Fragments
Owns `shared-safety`, `output-protocol`, and `specialist-output-protocol` shared fragments. Products parameterize these but must not weaken them.

## Readiness Checks
Verifies the declared runner protocol version is supported by the configured runner shim.

## Conformance Tests
Typed outbound actions and attachments round-trip against the envelope schema; final-text fallback is emitted when no typed action is produced; unsupported protocol versions are refused.
