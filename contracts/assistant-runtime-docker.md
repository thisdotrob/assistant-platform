# assistant-runtime-docker Contract

## Public API
Docker image naming, container spawn/stop, mount construction, lifecycle management, and idle/stale container handling for agent runners.

## Persistence Ownership
Owns container runtime-state projections (running/idle/stale) via central DB migrations. Does not mutate session or core tables.

## Config
Reads image tags/digests, mount roots, and idle/stale timeouts from assistant-config and the platform manifest.

## Events
Emits container-spawned, container-stopped, and stale-container-reaped events; surfaces spawn failures as diagnostics.

## CLI/Web Surfaces
None directly; runtime state is shown through CLI status and web readiness views.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies the Docker daemon is reachable, required image tags/digests resolve, and mount roots exist and are writable.

## Conformance Tests
Mounts grant the container read-only inbound and writable outbound access; idle/stale containers are reaped per policy; a runner is never started against an unsupported session schema.
