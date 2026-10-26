# Provisional support matrix

This matrix records versions exercised by the available project evidence; it is
not a production-readiness claim or a complete V1 compatibility promise. The
application and metadata contracts remain provisional.

## Database servers

| Database | Versions exercised | Verified behavior and limits |
|---|---|---|
| SQLite | Bundled through `rusqlite`; the exact SQLite runtime version is not recorded in the current gate evidence | Default automated suite; transactional migrations, rollback, metadata, and recovery tests. No external server required. |
| PostgreSQL | 16.15, 17.11, 18.6 | CLI and adapter integration, metadata v3/v4 inspection and explicit v3→v5/v4→v5 upgrades, TLS checks, session locks, transactional migrations, supported concurrent-index reconciliation, and runner interactions. Some recovery cases are opt-in. |
| MySQL | 8.4.11 | Non-transactional migration journey with explicit `transaction = never`, per-statement journal, locks, partial failure, process interruption, explicit reconciliation, TLS checks, and metadata v3/v4→v5 upgrades. |
| MariaDB | 10.11.14, 11.4.13, 11.8.2 | Separate non-transactional migration and metadata upgrade journeys, locks, partial failure/recovery, and selected TLS checks. |

MySQL/MariaDB migrations must declare `transaction = never`; their DDL can
commit implicitly. PostgreSQL and SQLite support transactional migrations.
Mutations against PostgreSQL, MySQL, and MariaDB require metadata format 5;
read-only inspection supports the formats described in the README. Metadata
upgrades are explicit and can fail closed when an upgrade is interrupted.

## Rust, operating systems, and optional integrations

| Area | Declared/current evidence | Qualification |
|---|---|---|
| Rust | `rust-version = 1.89`; development and CI toolchain 1.99.1 | 1.89 is the declared MSRV, but the current full-feature gate is run on 1.99.1; a complete MSRV matrix is not recorded. |
| Operating system | GitHub CI uses `ubuntu-latest`; local work is Linux x86_64 | macOS and Windows builds/tests are not verified by the current CI. |
| Optional AI | `ai` Cargo feature; fake HTTP/provider tests | No real AI provider was called. Provider behavior, availability, and output quality are not certified. |
| Fuzzing | Scanner/parser targets and seed corpus are prepared under `fuzz/` | No fuzz run evidence is recorded yet. Nightly Rust and `cargo-fuzz` are required. |

The server matrix is limited to the exact versions above and the behaviors
named here. It does not imply support for other releases, topologies, cloud
proxies, all TLS configurations, production deployments, or a stable V1 API.
