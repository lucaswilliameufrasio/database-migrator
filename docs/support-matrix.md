# Provisional support matrix

This matrix records versions exercised by the available project evidence; it is
not a production-readiness claim or a complete V1 compatibility promise. The
application and metadata contracts remain provisional.

## Database servers

| Database | Versions exercised | Verified behavior and limits |
|---|---|---|
| SQLite | Bundled through `rusqlite 0.40.2` as SQLite 3.53.2 by default; SQLite 3.54.1 was linked for the 2026-11-07 local suite | Default automated suite covers transactional migrations, rollback, metadata, and recovery. The full suite also passed against official SQLite 3.54.1 using a local `pkg-config` override; this does not change the runtime bundled in normal builds. No external server required. |
| PostgreSQL | 16.15, 17.11, 18.6 | CLI and adapter integration, metadata v3/v4 inspection and explicit v3→v5/v4→v5 upgrades, TLS checks, session locks, transactional migrations, supported concurrent-index reconciliation, and runner interactions. Focused recovery tests passed on 16.15 and 18.6; the complete fault matrix remains pending. |
| MySQL | 8.4.11; 8.4.13 and 9.7.4 (focused recovery drills only) | 8.4.11: non-transactional migration journey, per-statement journal, locks, partial failure, process interruption, explicit reconciliation, TLS checks, and metadata upgrades. 8.4.13 and 9.7.4: focused recovery journey and partial-failure cases. |
| MariaDB | 10.11.14, 11.4.13, 11.8.2; 11.8.9 and 13.0.2 (focused recovery drills only) | Earlier versions: non-transactional migration and metadata upgrade journeys, locks, partial failure/recovery, and selected TLS checks. 11.8.9 and 13.0.2: focused recovery journey and partial-failure cases. |

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
