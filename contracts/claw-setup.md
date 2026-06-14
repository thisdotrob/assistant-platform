# claw-setup Contract

## Public API
Deterministic, resumable setup steps, progression logs, service templates, and first orchestrator/channel setup, exposed as the setup command API for product binaries.

## Persistence Ownership
Owns the resumable setup step-state and readiness-registry records, persisted as JSON sidecars under the instance setup directory (`setup/state.json`, `setup/readiness.json`) so they survive before the central DB migration step has run. A later milestone may also project readiness results into the central DB for CLI/web display.

## Config
Reads setup defaults and service templates from claw-config; verifies product/platform version compatibility before running.

## Events
Emits setup-step-started, setup-step-completed, and setup-failed events.

## CLI/Web Surfaces
None directly; backs the CLI setup command and web setup progression display.

## Prompt Fragments
None in the first release.

## Readiness Checks
Aggregates module readiness and refuses to proceed when migration checksums, required namespaces, or product/platform versions do not match the manifests.

## Conformance Tests
Setup steps are idempotent and resumable; setup refuses to run on incompatible version state; no source mutation occurs during setup.
