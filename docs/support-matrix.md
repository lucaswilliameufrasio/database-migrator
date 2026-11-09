# Provisional support matrix

This matrix records versions exercised by the available project evidence; it is
not a production-readiness claim or a complete V1 compatibility promise. The
application and metadata contracts remain provisional.

## Database servers

| Database | Versions exercised | Verified behavior and limits |
|---|---|---|
| SQLite | Bundled through `rusqlite 0.40.2` as SQLite 3.53.2 by default; SQLite 3.54.1 was linked for the 2026-11-07 local suite | Default automated suite covers transactional migrations, rollback, metadata, and recovery. The full suite also passed against official SQLite 3.54.1 using a local `pkg-config` override; this does not change the runtime bundled in normal builds. The #11 rollback-before-failure-record test passed CI on `de529ce` (run `42538270662`) and on the read-lock harness at `8d47569` (run `42548111248`); run `42553905101` later timed out before observing rollback because the 5M-row fixture CTE had not completed within 45 seconds. The fixture CTE was reduced to 1M rows and passed all three CI jobs on `894d984` (run `42555474213`), but run `42560317968` subsequently failed the same test after observing rollback and before capturing failed-run persistence. The current working correction writes a sentinel before the trigger's slow CTE; it passed six focused local runs, the full local gate, and the complete workspace tests on Rust 1.89.0. Remote CI has not yet verified this correction; see `recovery-drills.md`. The 2026-11-08 local regression verifies `SQLITE_OPEN_NOFOLLOW` rejects a final-component database symlink swapped after config validation in both read-only and mutable opens; ancestor-directory and migration-file races are outside that assertion. No external server required. |
| PostgreSQL | 16.15, 17.11, 18.6 | CLI and adapter integration, metadata v3/v4 inspection and explicit v3→v5/v4→v5 upgrades, session locks, transactional migrations, supported concurrent-index reconciliation, and runner interactions. Focused recovery tests passed on 16.15 and 18.6; the 2026-11-08 supplemental 17.11 run passed 12 library/engine and 5 CLI tests, plus the complete 45-test ignored workspace suite paired with MySQL 8.4.13/MariaDB 11.8.9 (PgBouncer and three TLS tests excluded). The project owner selected current stable 18.6 as the required G11 line for scenario #11; 17.11 evidence is supplemental, not a new requirement. PostgreSQL #12 engine-level source revalidation is verified on all three listed versions. The configured-CA TLS test passed on 16.15. The PgBouncer 1.26.0 transaction-pooling backend-switch rejection test passed twice each on 16.15 and 18.6; it used a test-only Debian Bookworm base override and does not validate the repo's default Trixie image. The complete fault matrix remains pending. |
| MySQL | 8.4.11; 8.4.13 and 9.7.4 (focused recovery drills only) | 8.4.11: non-transactional migration journey, per-statement journal, locks, partial failure, process interruption, explicit reconciliation, and metadata upgrades. Eight opt-in G11 recovery/metadata journeys passed twice on each of 8.4.11, 8.4.13, and 9.7.4; the full 45-test ignored suite passed across these versions in four PostgreSQL/MySQL/MariaDB combinations. TLS CA/hostname validation passed on 8.4.13. Other focused recovery evidence is listed in `recovery-drills.md`. |
| MariaDB | 10.11.14, 11.4.13, 11.8.2; 11.8.9 and 13.0.2 (focused recovery drills only) | On 2026-11-08, each of 10.11.14 and 11.4.13 passed two independent clean loopback passes, each with 9 non-TLS CLI recovery/metadata tests, 2 adapter metadata/import tests, 1 engine source-revalidation test, 1 DSL Up/Down test, and 1 SQLx legacy-import test. TLS CA/hostname validation also passed once per version. The full 45-test ignored workspace suite passed once on each version paired with PostgreSQL 18.6 and MySQL 8.4.13; PgBouncer and three TLS tests were excluded, with MariaDB TLS verified separately. Eight opt-in G11 recovery/metadata journeys passed twice on each of 11.8.2, 11.8.9, and 13.0.2; the full 45-test ignored suite passed across these versions in four PostgreSQL/MySQL/MariaDB combinations, and two direct adapter tests also passed on 11.8.2 and 13.0.2. TLS CA/hostname validation passed on 11.8.9. Other focused recovery evidence is listed in `recovery-drills.md`. |

MySQL/MariaDB migrations must declare `transaction = never`; their DDL can
commit implicitly. PostgreSQL and SQLite support transactional migrations.
Mutations against PostgreSQL, MySQL, and MariaDB require metadata format 5;
read-only inspection supports the formats described in the README. Metadata
upgrades are explicit and can fail closed when an upgrade is interrupted.

The current-checkout 45-test opt-in workspace selection also passed on
2026-11-09 with PostgreSQL 18.6 and MySQL 8.4.13 paired separately with
MariaDB 10.11.14, 11.4.13, and 11.8.9. PgBouncer and three TLS cases were
excluded from this selection; those have separate evidence. Execution used
fresh rootless loopback containers and `--test-threads=1`. The initial parallel
attempt failed with lock timeouts, a MariaDB EOF, and a PostgreSQL assertion;
the cause remains unisolated. See `recovery-drills.md` for exact image digests
and logs. This strengthens the named versions but does not complete the support
matrix or security review.

## Rust, operating systems, and optional integrations

| Area | Declared/current evidence | Qualification |
|---|---|---|
| Rust | `rust-version = 1.89`; development and CI toolchain 1.99.1; `cargo +1.89.0 test --workspace --all-features --locked` passed on 2026-11-08 | Full workspace tests are verified on the declared MSRV; the complete format/Clippy/build/examples/supply-chain gate is recorded on 1.99.1, not 1.89. |
| Operating system | GitHub CI uses `ubuntu-latest`; local work is Linux x86_64 | macOS and Windows builds/tests are not verified by the current CI. |
| Optional AI | `ai` Cargo feature; optional `cap-std`/`cap-fs-ext` 4.0.3; fake HTTP/provider tests; Unix regression tests reject final and intermediate symlinks swapped after path canonicalization | Local Linux tests only; no real AI provider was called. Provider behavior, availability, output quality, and macOS/Windows filesystem behavior are not certified. Under the aggressive post-fix path-race reproduction all 160 previews failed closed, so it established no successful-preview rate under contention. |
| Fuzzing | Scanner/parser targets and seed corpus are under `fuzz/`; both targets completed bounded runs on 2026-11-07 with retained generated corpora | Nightly Rust and `cargo-fuzz` were used in a rootless, network-disabled sandbox. `sql_scanner`: 2,309,020 executions; `migration_parser`: 2,404,475; 301 seconds each with configured 300-second limit. No crash artifacts were reported. This is bounded evidence, not proof of correctness; details and retained-corpus manifest are in `supply-chain.md`. |

The server matrix is limited to the exact versions above and the behaviors
named here. It does not imply support for other releases, topologies, cloud
proxies, all TLS configurations, production deployments, or a stable V1 API.
