# claw-agent-graph Contract

## Public API
Orchestrator/specialist agent-group lifecycle, agent-to-agent routing, specialist creation policy, and delegation-job management.

## Persistence Ownership
Owns agent-graph and specialist-job state tables via central DB migrations, keyed to core session and agent IDs.

## Config
Reads specialist creation policy and delegation limits from claw-config; specialist profiles are supplied by capability modules.

## Events
Emits specialist-created, delegation-started, delegation-completed, and agent-routing events.

## CLI/Web Surfaces
None directly; agent-graph state is surfaced through CLI inspection and web views.

## Prompt Fragments
None of its own; consumes specialist-output protocol fragments owned by claw-agent-protocol.

## Readiness Checks
Verifies registered specialist profiles resolve and no orphaned delegation jobs remain.

## Conformance Tests
Specialist creation respects the configured policy; agent-to-agent routing terminates without cycles; delegation jobs reach a terminal state or are reaped.
