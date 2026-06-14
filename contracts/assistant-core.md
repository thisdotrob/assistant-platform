# assistant-core Contract

## Public API
Shared ID types, timestamps, error shapes, product descriptor traits, and the module registry consumed by every other crate. No runtime side effects.

## Persistence Ownership
Owns no database tables in PR 0/1. Defines the stable core ID types that other modules key their extension tables to.

## Config
Owns no config keys directly; defines the descriptor traits and version structs that assistant-config and the platform manifest populate.

## Events
Defines the shared event and error envelopes used across modules. Emits no events of its own.

## CLI/Web Surfaces
None. Exposes only the Rust metadata and registry API that CLI and web crates build on.

## Prompt Fragments
None in the first release.

## Readiness Checks
Provides the readiness-check trait and result types. Registers no checks itself.

## Conformance Tests
Core ID, timestamp, and error round-trips are stable; the module registry rejects duplicate or unknown module IDs.
