# assistant-db Contract

## Public API
Central DB connection pool, migration registry, repository helpers, and transaction utilities. Migrations run through one registry built from the platform plus product manifests.

## Persistence Ownership
Owns two central bookkeeping tables and no domain tables. `schema_migrations` records each applied migration by module ID, version, name, checksum, and timestamp (the checksum drives drift detection). `module_versions` records, per module, the module/product/platform versions and product ID active at migration time for compatibility reporting.

## Config
Reads the central DB path and connection settings from assistant-config; owns no config keys of its own.

## Events
Emits migration-applied and migration-skipped events; surfaces checksum-mismatch and ordering violations as fatal diagnostics.

## CLI/Web Surfaces
None directly; backs the CLI migration-status and web readiness views.

## Prompt Fragments
None in the first release.

## Readiness Checks
Verifies all required module migration namespaces are registered and applied checksums match the current manifest.

## Conformance Tests
Migration ordering follows declared module dependencies then local sequence; checksum mismatches and unknown namespaces are rejected at startup.
