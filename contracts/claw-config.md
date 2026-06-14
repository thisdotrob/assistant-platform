# claw-config Contract

## Public API
Instance config loading, environment overlays, path derivation, and product-namespace resolution. Returns a typed, validated config view to dependents.

## Persistence Ownership
Owns the on-disk config file schema. Owns no database tables in PR 0/1.

## Config
Owns the top-level instance config schema: data root, product namespace, env-overlay precedence, and derived path layout.

## Events
Emits a config-loaded event with the resolved namespace and version; surfaces config-validation errors as typed diagnostics.

## CLI/Web Surfaces
None directly; exposes the resolved config to the CLI and web crates for display.

## Prompt Fragments
None in the first release.

## Readiness Checks
Checks that the data root is writable, required namespaces are present, and env overlays resolve without conflict.

## Conformance Tests
Env overlays apply in declared precedence; missing required keys fail loudly; derived paths are deterministic for a given namespace.
