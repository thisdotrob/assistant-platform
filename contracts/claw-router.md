# claw-router Contract

## Public API
Message routing, engagement-mode resolution, sender-scope evaluation, ignored-message policy, and fan-out decisions over normalized inbound messages.

## Persistence Ownership
Owns routing projection and engagement-state tables via central DB migrations, keyed to core session and sender IDs.

## Config
Reads default engagement modes and ignored-message policy defaults from claw-config.

## Events
Consumes normalized channel events; emits routing-decision and fan-out events.

## CLI/Web Surfaces
None directly; routing state is surfaced through the web UI and CLI inspection commands.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies routing projections are consistent with session state and engagement defaults resolve.

## Conformance Tests
Engagement modes, sender scope, and ignored-message policy produce deterministic routing decisions; fan-out never duplicates or drops a message under replay.
