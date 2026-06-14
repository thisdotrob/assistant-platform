# assistant-cli Contract

## Public API
Host CLI, in-container CLI request/response dispatcher, command registry, and output formatting. Provides the `doctor`/status entry points product binaries wire up.

## Persistence Ownership
Owns no database tables directly in PR 0/1. Reads through assistant-db, assistant-session, and other modules' APIs.

## Config
Reads CLI output and verbosity defaults from assistant-config; owns no schema of its own.

## Events
Emits no domain events; renders events and diagnostics produced by other modules.

## CLI/Web Surfaces
Owns the host CLI command surface and the in-container CLI request/response bridge.

## Prompt Fragments
None in the first release.

## Readiness Checks
Aggregates and renders readiness-check results from all registered modules; registers none of its own.

## Conformance Tests
The command registry rejects duplicate command names; in-container requests round-trip against the response contract; doctor output reflects aggregated readiness state.
