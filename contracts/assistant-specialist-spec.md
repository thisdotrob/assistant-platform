# assistant-specialist-spec Contract

## Public API
`SpecialistSpec`, the declarative description of a specialist sub-agent (routing name + description, agent-graph profile identity and concurrency limits, custom container image reference, and in-container turn config: system prompt, enabled tools, allow-listed tool patterns, max turns, extra env), plus `SpecialistMenuEntry` and `SpecialistSpec::menu_entry`. Plain owned data with no dependency on the host, the Docker runtime, or the agent-graph engine, so a specialist crate can build one without pulling in core internals.

## Persistence Ownership
None. Owns no tables and touches no database; it is pure data the host translates into runtime objects at registration time.

## Config
None of its own. A `SpecialistSpec` is itself the configuration a product supplies to the host to register a specialist.

## Events
None.

## CLI/Web Surfaces
None directly. Backs the orchestrator's `delegate` routing menu (via `SpecialistMenuEntry`) and the host's specialist registration.

## Prompt Fragments
None owned here. Carries a specialist's complete `system_prompt` string as data; the owning specialist crate authors that prompt.

## Readiness Checks
None. The host and the owning specialist crate own readiness (image presence, network, artifact roots).

## Conformance Tests
`SpecialistSpec` and `SpecialistMenuEntry` round-trip through JSON; `menu_entry` projects exactly the `route_name`/`description` pair the orchestrator routes by.
