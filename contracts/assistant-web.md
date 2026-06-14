# assistant-web Contract

## Public API
Single-instance local web UI server and shared API handlers, including token auth, memory-editor safety, and readiness display.

## Persistence Ownership
Owns web session/token and local UI-state records via central DB migrations where required; reads all domain data through owning modules' APIs.

## Config
Reads bind address, local token settings, and UI flags from assistant-config.

## Events
Emits no domain events; renders events and readiness state produced by other modules.

## CLI/Web Surfaces
Owns the local web pages and the typed local API handlers shared across views.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies the server binds locally, token auth is configured, and dependent module APIs are reachable.

## Conformance Tests
Local API handlers reject unauthenticated requests; the memory editor refuses unsafe writes; readiness display reflects aggregated module state.
