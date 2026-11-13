# Changelog

All notable changes to this project will be documented here. This file is a
draft for the planned V1; no stable release has been published.

## Unreleased

### Fixed

- Production `up` and `down` check the write session's target fingerprint again
  before the first metadata mutation. Creating a missing SQLite file still
  matches an `uninitialized` approval when the path, schema, and database are
  unchanged. This does not close G11 or G12.

### Changed

- `environment = production` can apply `up` and `down` when the operator passes
  `--approve-plan` and `--approve-target` from the current read-only plan.
  `dbm plan --down --steps N` prints the rollback approval hash. Development
  and test targets stay unchanged and reject those flags. Reconciliation,
  metadata upgrade, and legacy apply/adopt keep their existing approvals and
  also require `--approve-target` in production. This does not close G11 or
  G12 and does not authorize a tag or publication.

### Added

- SQL migration engine and `dbm` CLI for SQLite, PostgreSQL, MySQL, and MariaDB.
- Per-statement journal, read-only diagnostics, explicit recovery, and native
  database locking.
- Typed schema DSL, compiled migration registry/runner, and legacy history
  import for Knex, Goose, and SQLx formats.
- Repeatable migrations, explicit namespaces, metadata v5, and opt-in AI
  proposals that never apply changes automatically.
- Local SQL example with bundled SQLite, PostgreSQL 18.6, and MariaDB 11.8.9.
- Executable SQLite registry/DSL examples, SQL scanner/parser fuzz targets,
  dependency policy checks, and a CI-generated CycloneDX SBOM artifact.

### Compatibility and release notes

- Metadata upgrades are explicit; SQLite supports formats 1–4 to v5, while
  PostgreSQL/MySQL/MariaDB have verified explicit v3/v4 to v5 paths for the
  database versions listed in `docs/support-matrix.md`.
- Public Rust APIs and metadata remain provisional. This changelog does not
  declare the planned V1 released.
- F11 hardening and G11 are incomplete; G12 remains blocked. See
  `docs/release-checklist.md` before any release preparation or publication.
