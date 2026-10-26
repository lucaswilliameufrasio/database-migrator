# Changelog

All notable changes to this project will be documented here. This file is a
draft for the planned V1; no stable release has been published.

## Unreleased

### Added

- SQL migration engine and `dbm` CLI for SQLite, PostgreSQL, MySQL, and MariaDB.
- Per-statement journal, read-only diagnostics, explicit recovery, and native
  database locking.
- Typed schema DSL, compiled migration registry/runner, and legacy history
  import for Knex, Goose, and SQLx formats.
- Repeatable migrations, explicit namespaces, metadata v5, and opt-in AI
  proposals that never apply changes automatically.
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
