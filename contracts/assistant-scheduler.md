# assistant-scheduler Contract

## Public API
One-off and recurring task scheduling, due-work sweeps, and pause/resume/cancel lifecycle over message-primary scheduled work.

## Persistence Ownership
Owns the central scheduled-work projection tables and the scheduling fields stored in session messages. Representation changes require both central and per-session migrations.

## Config
Reads sweep cadence and default timezone from assistant-config.

## Events
Emits work-due, work-completed, paused, resumed, and cancelled events.

## CLI/Web Surfaces
None directly; backs CLI schedule/list/pause/resume/cancel commands and web schedule views.

## Prompt Fragments
Owns `scheduling-wording`, which standardizes one-off and recurring scheduling language and lifecycle phrasing.

## Readiness Checks
Verifies the sweep loop is running, projections are consistent with session message state, and no due work is stuck past lease expiry.

## Conformance Tests
Due-work sweeps fire each task exactly once under leases; pause/resume/cancel are honored idempotently; projection repair reconciles session and central state.
