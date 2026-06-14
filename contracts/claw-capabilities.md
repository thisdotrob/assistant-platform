# claw-capabilities Contract

## Public API
Capability/profile declaration traits, capability profile registry, readiness-check declaration, and product-assembly helpers used to compose a product from modules.

## Persistence Ownership
Owns capability registry metadata via central DB migrations where needed. Capability and channel modules own their own extension/readiness/config tables and do not mutate core tables.

## Config
Reads enabled-capability and profile selections from claw-config.

## Events
Emits capability-registered and profile-assembled events.

## CLI/Web Surfaces
None directly; backs CLI capability listing and web capability/readiness views.

## Prompt Fragments
None of its own; provides the assembly helpers that gather fragments declared by capability modules.

## Readiness Checks
Verifies declared capability profiles resolve to registered modules and each declares its required readiness checks.

## Conformance Tests
Profile assembly rejects unknown or duplicate capability IDs; declared readiness checks are discoverable; capability modules cannot mutate core base tables.
