# assistant-permissions Contract

## Public API
User/role/membership model, unknown-sender and unknown-channel policy, admin gates, and access-check helpers used by host, CLI, setup, and web.

## Persistence Ownership
Owns the user, role, and membership tables via central DB migrations. Only this module migrates those base schemas.

## Config
Reads default roles, admin bootstrap, and unknown-sender policy defaults from assistant-config.

## Events
Emits membership-changed and access-denied events.

## CLI/Web Surfaces
None directly; backs CLI user/role administration commands and web permission views.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies at least one admin exists and role/membership references are intact.

## Conformance Tests
Access checks deny by default for unknown senders/channels; admin gates cannot be bypassed; role and membership changes are atomic.
