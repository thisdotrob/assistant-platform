# claw-upgrade Contract

## Public API
Hardens the upgrade flow for long-lived instances created by this rewrite. Exposes an instance schema inventory (recorded central module versions, applied central migration versions, and per-session inbound/outbound schema versions), an idempotent upgrade runner that applies pending central migrations and eagerly migrates every per-session DB, and a runtime compatibility matrix that compares product version, platform version, the platform module graph, the instance DB schema state, the base container image contract version, and the orchestrator profile version. Backs the product `upgrade` and `conformance` commands.

## Persistence Ownership
Owns no tables of its own. Reads and advances the central DB meta tables owned by claw-db (`schema_migrations`, `module_versions`) through claw-db's public API, and reads/advances each session DB's `schema_meta`/`schema_migrations` through claw-session's lazy migration API. All writes are confined to the instance root and refuse to touch a protected source repo.

## Config
Reads the instance layout (central DB path, sessions root) from claw-config and the platform release manifest (platform/module/image/profile versions) from claw-core; takes the product identity and the platform/container/profile versions to verify against from the caller.

## Events
Emits upgrade-started, central-migration-applied, session-migrated, upgrade-completed, and upgrade-failed progress events through an injected progress sink; the runner returns a structured report of applied/skipped migrations per DB and the sessions touched.

## CLI/Web Surfaces
None directly; backs the product CLI `upgrade` (apply pending migrations to an existing instance, with dry-run) and `conformance` (run the full compatibility matrix after a version bump) commands, and the web display of upgrade/compatibility results.

## Prompt Fragments
None in the first release.

## Readiness Checks
Surfaces incompatible version detection as readiness-style diagnostics: refuses to downgrade when an instance was written by a newer platform than the running one — and refuses equally when the recorded version cannot be ordered against the runner (unparseable on either side), since an unverifiable version cannot be proven not-newer — flags an unsupported per-session schema range, and flags container image or profile version mismatches against the platform manifest.

## Conformance Tests
The upgrade runner is idempotent and resumable (a rerun applies nothing), dry-run writes nothing, no tracked source mutation occurs during an upgrade, a prior-version instance migrates forward (central and per-session), and an instance newer than the runner is detected and refused. Write confinement is enforced by canonicalizing both the instance root and each write target before writing, so a symlinked directory planted inside the instance tree cannot redirect a write outside the real root.

## Rollback And Failure Semantics
Migrations are forward-only: the platform ships no down-migrations, so an upgrade never "undoes" a schema. Recovery is by re-running forward, not by reversing.

Each migration applies in its own transaction — the schema change and the row recorded in `schema_migrations` commit together or not at all. There is no transaction spanning the whole upgrade: when a migration fails, its own transaction rolls back so that DB is left exactly at the last successfully recorded version, while migrations that already committed (earlier migrations, or earlier session DBs in the sweep) stay applied. An upgrade can therefore fail partway and leave the instance at a mix of versions; that state is well-defined, not corrupt.

Recovery from a partial or failed upgrade is to fix the underlying cause and re-run the upgrade. The run is idempotent and resumable: already-applied migrations are checksum-verified and skipped, so a re-run advances only the DBs still behind and applies nothing twice. A checksum mismatch (a recorded migration whose SQL no longer matches) aborts the run rather than silently rewriting history; resolve it by restoring the instance from backup, not by editing migration history.

A true rollback (returning an instance to an older schema) is out of scope for this crate. The supported path is to restore the entire instance root — the central DB plus every per-session DB — from a backup taken before the upgrade, since downgrading a live instance in place is unsafe. The runner is also one-directional by design: it refuses to operate on an instance recorded as written by a platform newer than the running one, so an accidental downgrade cannot begin a destructive partial migration.
