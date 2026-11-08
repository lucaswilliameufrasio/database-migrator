# Recovery behavior and fault-injection evidence

This runbook describes safe investigation after a migration interruption and
records which development fault scenarios currently have evidence. It does not
authorize a production migration or repair.

## Operator response

After a process exit, timeout, disconnect, or uncertain commit:

1. Do not retry `up`, `down`, or a DDL statement just because the command did not
   return success.
2. Preserve the run ID, source files, output, timestamps, and independent
   database observations. Do not put credentials in evidence files.
3. Use read-only `dbm --format json history`, `status`, and `doctor` against the
   same target and namespace. Check whether the migration source still matches
   the journaled checksum and whether a lock may still belong to a live process.
4. Inspect each statement's effects using database-native evidence. For MySQL
   and MariaDB, assume DDL may have committed or partially taken effect; the
   engine cannot infer arbitrary schema effects.
5. If the result cannot be demonstrated, keep writes blocked. Reconcile only
   after the operator has inspected or manually repaired the database and can
   supply a complete, ordered repair plan, evidence reference, reason, actor,
   and explicit approval.
6. Re-run `doctor` and `status` after the explicit reconciliation. Recompute the
   plan and review it before any subsequent write.

`doctor reconcile` records the decision; it does not execute or retry SQL.
PostgreSQL catalog reconciliation is restricted to the supported concurrent
index operation. SQLite's transactional state can prove rollback for a
confirmed transactional failure. MySQL/MariaDB require operator inspection of
all non-transactional effects. A session lock becoming available is not proof
that the statement did not run.

## Existing fault-injection coverage

The development plan's required matrix is in
[`database-migrator-rust-plano-desenvolvimento.md`](database-migrator-rust-plano-desenvolvimento.md),
section 13.3. Current source evidence is narrower than the complete matrix:

| Scenario | Current evidence | Status |
|---|---|---|
| SQLite process death and competing CLI | `tests/cli.rs::killing_cli_during_a_transaction_keeps_the_next_write_blocked` starts a second `up` while the first CLI holds the writer lock, kills the first during SQL, and checks the second CLI is safety-blocked without creating a second run | Covered locally by the default CLI test suite; SQLite only |
| MySQL process killed during a non-transactional statement | `tests/cli.rs::cli_runs_mysql_nontransactional_migration_and_recovery_journey` kills the CLI during `SELECT SLEEP`, checks the unresolved run and released session lock, verifies a new writer is blocked, then reconciles explicitly | Passed twice each on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4 on 2026-11-07 |
| MySQL/MariaDB failures after earlier DDL steps | The non-transactional CLI journeys inject failures at the second and third statement and inspect partial effects before reconciliation | Passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2 on 2026-11-07 |
| PostgreSQL migration-session loss during transactional DDL | `src/engine.rs::tests::local_postgres_session_loss_during_fsm_ddl_blocks_competing_writer` terminates the backend while a scoped DDL trigger pauses execution, inspects durable state, and attempts a second migration through the real engine | Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6 on 2026-11-07; competing write was rejected without creating a second run |
| PostgreSQL failed non-transactional concurrent-index migration | `src/engine.rs::tests::local_postgres_nontransactional_failure_is_journaled_indeterminate` checks the indeterminate journal and catalog-gated explicit reconciliation | Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6 on 2026-11-07 |
| PostgreSQL transaction-pooling PgBouncer backend switch | `database::postgres::tests::local_pgbouncer_backend_change_is_rejected_after_session_lock_acquisition` verifies the adapter rejects a changed physical backend after acquiring its session lock | Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6 via PgBouncer 1.26.0 on 2026-11-08; test-only Bookworm base override, details below |
| Runner absent, incompatible, timed out, or emitting extra frames | `tests/runner_protocol.rs::missing_runner_timeout_and_extra_frames_fail_without_partial_sql` and adjacent handshake tests | Covered locally; timeout test uses a disposable project |
| Complete crash matrix across supported adapters and versions | Targeted scenarios are tracked below. The repository-wide source-only security review on 2026-10-26 did not execute target code or add fault-injection evidence. | Partial; driver/version coverage and additional boundaries remain pending; G11 is not approved |

### Matrix traceability as of 2026-11-08

The numbered rows below correspond to section 13.3 of the development plan.
They are a gap inventory, not a claim that source inspection or a related test
proves a whole scenario across every database and failure boundary.

| # | Required boundary | Evidence and remaining gap |
|---|---|---|
| 1 | Death before intent | `tests/cli.rs::cli_process_death_at_postgres_recovery_boundaries_keeps_runs_unresolved` pauses the intent insert, kills the CLI/backend, and confirms a started run with no durable step or migration effect. Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. `cli_mysql_process_death_before_intent_keeps_run_unresolved` and the MariaDB counterpart hold the prepared intent insert before forwarding; both confirm a started run, zero durable steps/effects/applied rows, and blocked retry. Each passed twice on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. SQLite does not expose this as an independently durable intent boundary under its transactional profile; other server versions remain unverified. |
| 2 | Death after intent, before SQL | `tests/cli.rs::cli_process_death_after_postgres_intent_before_sql_keeps_the_intent` holds the non-transactional index statement in a loopback protocol proxy after the durable intent, kills the CLI, and confirms no index/applied row plus blocked retry. Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. `cli_mysql_process_death_after_intent_before_sql_keeps_the_intent` and the MariaDB counterpart hold the DDL before forwarding, then confirm the durable intent, no SQL/applied row, and blocked retry. Each passed twice on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. SQLite does not expose this as an independently durable intent boundary under its transactional profile; other server versions remain unverified. |
| 3 | Death during SQL | SQLite CLI kill, MySQL/MariaDB CLI interruption during a non-transactional statement, and PostgreSQL backend loss during transactional DDL now pass on the versions listed above. The fault mechanisms differ by database semantics; other crash boundaries and versions remain unverified. |
| 4 | SQL completed, death before acknowledgement | The combined PostgreSQL CLI boundary test pauses the acknowledgement update after `CREATE INDEX CONCURRENTLY` completed; it confirms the valid index, durable `intent_recorded`, unresolved run, and blocked retry. Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. `cli_mysql_process_death_after_sql_before_ack_keeps_intent` and the MariaDB counterpart hold the ACK update after a real `CREATE TABLE`; they confirm the effect exists, the intent remains durable, no applied row exists, and retry is blocked. Each passed twice on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. SQLite does not expose this as an independently durable acknowledgement boundary under its transactional profile; other server versions remain unverified. |
| 5 | Acknowledgement persisted, death before final state | The combined PostgreSQL CLI boundary test pauses applied-row insertion after the acknowledgement commit; it confirms the valid index, `acknowledged` step, no applied row, unresolved run, and blocked retry. Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. `cli_mysql_process_death_after_ack_before_finalization_keeps_ack` and the MariaDB counterpart hold applied-row insertion after ACK; they confirm the effect and acknowledged step, no applied row, and blocked retry. Each passed twice on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. SQLite does not expose this as an independently durable acknowledgement boundary under its transactional profile; other server versions remain unverified. |
| 6 | MySQL/MariaDB partial DDL, third step fails | Second- and third-statement partial-failure journeys passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2; earlier DDL effects were inspected and reconciled explicitly. Other server versions remain unverified. |
| 7 | Native-lock session loss and competing instance | `local_postgres_session_loss_during_fsm_ddl_blocks_competing_writer` attempts a competing engine migration after terminating the original backend. The competing migration was rejected and the started run, observed-active diagnostic lock, and empty transactional journal remained unchanged in two runs each on PostgreSQL 16.15/18.6. The MySQL/MariaDB CLI recovery journeys kill the process during `SELECT SLEEP`, confirm the session `GET_LOCK` was released, and verify that a new writer stays blocked until reconciliation; passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2. Other lock-loss boundaries and server versions remain unverified. |
| 8 | Two CLIs, same DB and namespace | SQLite coverage remains as described at left. `cli_postgres_competing_writers_same_namespace_are_serialized` holds the first CLI's migration SQL before forwarding it; the competitor waits for the configured two-second timeout, returns `LOCK_UNAVAILABLE`, and creates neither a second run nor schema effect. Passed twice each on disposable loopback PostgreSQL 16.15/18.6. `cli_mysql_competing_writers_same_namespace_are_serialized` and its MariaDB counterpart observe the first CLI inside `SLEEP`, then verify the competing CLI returns `LOCK_UNAVAILABLE` with one successful run and one schema effect only. Passed twice each on disposable MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. Other server versions and concurrent-start boundaries remain unverified. |
| 9 | Two namespaces sharing schema object | `engine::tests::local_postgres_serializes_writers_across_namespaces_sharing_schema` starts distinct namespaces against one isolated PostgreSQL schema. The second writer does not enter SQL while the first is paused; after the first commits, the second fails on the already-created shared table and records a failed run only in its own namespace. Passed twice each on disposable PostgreSQL 16.15 and 18.6. `cli_serializes_sqlite_writers_across_namespaces_sharing_schema_objects` holds the first SQLite CLI inside a transactional migration; the second namespace has no run while the first owns the writer lock, then fails on the committed shared table with its own failed run and no applied row. Passed twice locally. `cli_mysql_serializes_namespaces_sharing_database_objects` and its MariaDB counterpart run two namespaces in the same database against one object: the second waits on the database-wide named lock, creates no run before release, then records an indeterminate run/step after the first creates the object; no applied row is recorded for the second namespace. Each passed twice on disposable MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2. Other adapters and races remain unverified. |
| 10 | Failure during metadata bootstrap/upgrade | `tests/cli.rs::cli_postgres_metadata_bootstrap_failure_rolls_back_all_reserved_tables` injects a PostgreSQL DDL-start failure midway through bootstrap and confirms no reserved metadata tables remain; passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. `database::postgres::tests::local_postgres_v4_upgrade_failure_rolls_back_prior_ddl` injects failure on the second `ALTER TABLE` of v4→v5, confirms the prior `ADD COLUMN` was rolled back, the v4 identity and native/run/step rows remain, and `repeatable_name` is absent; passed twice each on PostgreSQL 16.15 and 18.6. `cli_mysql_metadata_bootstrap_failure_preserves_partial_state_for_reconciliation` and its MariaDB counterpart pause the third bootstrap DDL, create a conflicting object, then verify `RECONCILIATION_REQUIRED`, two created metadata tables, no identity or migration effect, and `STATE_DIVERGED` on retry; passed twice each on disposable loopback MySQL 9.7.4 and MariaDB 13.0.2. `cli_mysql_metadata_upgrade_failure_preserves_partial_ddl_for_reconciliation` and the MariaDB counterpart disconnect before the second v4→v5 DDL after the first `schema_migrations` ALTER committed. Both confirm `RECONCILIATION_REQUIRED`, v4 identity plus the committed repeatable column, intact native/run/step rows, unchanged v4 run-journal column, no staging tables, and a `STATE_DIVERGED` retry with no further mutation; passed twice each on disposable loopback MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2. SQLite bootstrap is fault-injected through the real adapter session: a pre-existing table collides with the index created after the first metadata tables, initialization fails, explicit transaction rollback leaves zero reserved metadata tables and preserves the collision object. `database::sqlite::tests::sqlite_metadata_bootstrap_failure_rolls_back_partial_tables` passed twice locally. `storage::tests::metadata_v3_upgrade_preserves_native_state_and_adds_relational_import_tables` separately injects a name collision partway through SQLite v4→v5 upgrade and verifies transactional rollback preserves intact v4 metadata. Other bootstrap/upgrade versions remain unverified. |
| 11 | Rollback then crash before run recorded | `sqlite_crash_after_rollback_before_failure_record_keeps_run_unresolved` first applies a committed baseline, then fails a transactional migration after DDL. A test-only SQLite trigger slows the separate failed-run update; the test observes the rollback journal disappear and reappear for that update, kills the CLI before commit, and verifies the baseline remains applied, failed DDL is absent, the run remains `started` with zero steps, the diagnostic lock remains `observed_active`, and retry is safety-blocked. Passed twice locally. The combined PostgreSQL CLI boundary test pauses failed-run persistence and passed twice each on disposable loopback PostgreSQL 16.15 and 18.6; it passed again on PostgreSQL 18.6 on 2026-11-08. PostgreSQL 18.6 is the current stable minor of the latest stable major per the official versioning page; the project owner selected that current stable line instead of requiring this G11 boundary on 17.11. Other versions remain unverified. MySQL/MariaDB are not applicable to this rollback boundary under ADR-016: their profile requires `transaction=never`; `source::tests::mysql_migrations_require_explicit_never_and_match_the_target_dialect` verifies that `transaction=always` is rejected during discovery before database execution. No transactional production contract was changed. |
| 12 | Plan and source changed between approval and execution | `engine::tests::adapter_rejects_source_changed_after_plan_before_sql` captures a SQLite plan and manifest, changes the source, and verifies execution returns `DbmError::Safety`, records no steps, and creates neither version of the table; passed twice locally. `engine::tests::local_postgres_rejects_source_changed_after_plan_before_sql` exercises the same engine boundary through PostgreSQL, confirming `Safety`, a failed run without steps/applied rows, a released lock, and neither DDL effect; passed twice each on disposable loopback PostgreSQL 16.15 and 18.6, and again on 18.6 on 2026-11-08. `engine::tests::local_mysql_rejects_source_changed_after_plan_before_sql` and the MariaDB counterpart confirm changed source stops execution before intent/SQL, leaves a started run with no steps/applied rows, and creates neither candidate table. After restoring the original source and independently confirming both tables are absent, each test explicitly reconciles `operation_not_applied` and releases the diagnostic lock. Each test passed twice on MySQL 8.4.11, 8.4.13, and 9.7.4 and MariaDB 11.8.2, 11.8.9, and 13.0.2, all on disposable loopback servers. The project owner accepted the current CLI contract on 2026-11-08: `plan` is read-only, `up` recalculates, and reusable CLI approval is not part of the product contract. The engine-level source-revalidation tests are the applicable #12 criterion; no new CLI flow is authorized or required. Revisit only if a reusable approved-plan feature is proposed. |

**Additional MySQL/MariaDB CLI runs — 2026-11-07:** the eight opt-in tests for
death before intent, after intent/before SQL, SQL-before-ACK, ACK-before-final
state, same-namespace contention, shared-object namespaces, bootstrap failure,
and interrupted metadata upgrade each passed twice on disposable loopback
MySQL 8.4.11, 8.4.13, and 9.7.4, and MariaDB 11.8.2, 11.8.9, and 13.0.2.
This adds #1, #2, #4, #5, #8, #9, and #10 evidence on those patch lines; it
does not fill the other matrix gaps.

**Complete opt-in suite and TLS checks — 2026-11-07:** 45 ignored tests (library,
CLI, DSL, and legacy-import network tests) passed in each of four full workspace
runs with fresh loopback databases: (1) PostgreSQL 16.15, MySQL 8.4.13, MariaDB
11.8.9; (2) PostgreSQL 18.6, MySQL 8.4.13, MariaDB 11.8.9; (3) PostgreSQL
16.15, MySQL 8.4.11, MariaDB 11.8.2; and (4) PostgreSQL 18.6, MySQL 9.7.4,
MariaDB 13.0.2. Four tests were excluded in each run because their local
dependencies were unavailable: PgBouncer and three server-TLS cases. Those
three TLS tests subsequently passed with ephemeral local CAs on PostgreSQL
16.15, MySQL 8.4.13, and MariaDB 11.8.9, including rejection without the trusted
CA or with a hostname mismatch. At the time of those four runs, the PgBouncer
case had not run because no local binary/image was available; no image was
downloaded. The later PgBouncer run is recorded immediately below.

**PgBouncer transaction-pooling regression — 2026-11-08:** ignored test
`database::postgres::tests::local_pgbouncer_backend_change_is_rejected_after_session_lock_acquisition`
passed twice each against disposable loopback PostgreSQL 16.15 and 18.6 through
PgBouncer 1.26.0 built from
`lucaswilliameufrasio/pgbouncer-docker` at commit
`04dc81d4ab0a191bdc90e01d2b6c882c8c5b620f`. The repository pins 1.26.0; upstream
1.26.1 was available, but the repository's declared version was used without
editing its source. Its Debian Dockerfile was built with
`DEBIAN_CODENAME=bookworm` because the default `debian:trixie-slim` image was not
present locally. This test-only base override does not verify the default
Trixie image. The cached `debian:bookworm-slim` base was reused; no Docker images
were pulled. The Dockerfile did fetch Debian build packages and the checksum-
verified PgBouncer source archive. The test used transaction pooling with
`DEFAULT_POOL_SIZE=2`, an explicit test user in a disposable trust-auth
userlist, and a user-defined network; only PgBouncer was published on loopback
and PostgreSQL had no host port. An initial fixture attempt failed before the
test assertions because the trust userlist was empty; after adding the
ephemeral test user, all four executions passed. The evidence covers adapter
rejection of a changed physical backend, not the complete crash-recovery matrix.

**Additional adapter-version opt-ins — 2026-11-07:** a MySQL-focused ignored
workspace run passed 14 tests each on disposable MySQL 8.4.11 and 9.7.4
(adapter metadata/legacy history, engine source revalidation, nine CLI journeys,
DSL, and network import). A MariaDB-focused run passed 12 MariaDB-specific
tests each on 11.8.2 and 13.0.2; the two direct MySQL-family adapter tests for
metadata upgrade and legacy history also passed by exact test name on each of
those MariaDB versions. The MariaDB run included the aggregate non-transactional
journey for #3/#6/#7. A broad `local_mysql_` selector also picked a MySQL-only
engine assertion on MariaDB and correctly rejected that vendor; the two
MariaDB-compatible direct adapter tests were rerun individually and passed.

**Additional #12 MySQL-family version coverage — 2026-11-08:**
`engine::tests::local_mysql_rejects_source_changed_after_plan_before_sql` passed
twice each against fresh loopback-only MySQL 8.4.11 and 8.4.13 containers.
`engine::tests::local_mariadb_rejects_source_changed_after_plan_before_sql`
passed twice each against fresh loopback-only MariaDB 11.8.2 and 11.8.9
containers. The previously recorded 9.7.4 and 13.0.2 results complete this
engine-level #12 evidence across all MySQL/MariaDB versions named in the
provisional support matrix. Each container used an ephemeral database and was
removed. This does not close unrelated gaps in the recovery matrix or the
security-review requirement.

### Adapter-by-scenario audit

This view makes the cross-driver gaps explicit; “covered” refers only to the
named tests and versions above, not every supported version.

| Adapter | Scenario coverage | Remaining gaps or applicability |
|---|---|---|
| SQLite | #3, #8, #9; #10 for fault-injected initial bootstrap and v4→v5 upgrade; #11 for crash after rollback and during failed-run persistence; #12 at engine/source-revalidation level. | #6 is MySQL/MariaDB-specific. #1, #2, #4 and #5 have no separate crash-boundary test; SQLite keeps migration SQL and per-step journal changes in one transaction, so those are not independently durable states. #7 is partially represented by process death and file-lock recovery in #3/#8, but is not a native server-session lock test. CLI approval reuse is not part of the owner-accepted current contract for #12. |
| PostgreSQL | #1–#5 and #7–#11 on disposable 16.15 and 18.6; includes the CLI recovery boundary bundle, shared-schema race, same-namespace contention, bootstrap rollback, v4→v5 prior-DDL rollback and rollback-before-failure-record. #12 has engine-level revalidation coverage on both versions and a fresh 18.6 run on 2026-11-08. | #6 is MySQL/MariaDB-specific. The project owner selected current stable 18.6 for G11 rather than requiring #11 on 17.11; 17.11 remains in the provisional support matrix but is unverified for this boundary. Other PostgreSQL versions remain unverified for these focused runs. |
| MySQL | #1, #2, #4, #5, #8, #9 and #10 have focused CLI evidence on 8.4.11, 8.4.13, and 9.7.4; the eight MySQL opt-in recovery/metadata tests passed twice on all three versions. The aggregated non-transactional journey for #3/#6/#7 also passed twice on the documented lines. Engine source-revalidation #12 passed twice on all three versions. The full 45-test ignored suite passed on 8.4.11, 8.4.13, and 9.7.4 in the four combinations above. | #11 is not applicable under ADR-016 because this profile requires `transaction=never`; source discovery regression-tests rejection of `transaction=always`. CLI approval reuse is not part of the owner-accepted current contract for #12. Other versions and some sub-boundaries remain unverified. |
| MariaDB | #1, #2, #4, #5, #8, #9 and #10 have focused CLI evidence on 11.8.2, 11.8.9, and 13.0.2; the eight MariaDB opt-in recovery/metadata tests passed twice on all three versions. The aggregated non-transactional journey for #3/#6/#7 also passed twice on the documented lines. Engine source-revalidation #12 passed twice on all three versions. The full 45-test ignored suite passed on 11.8.2, 11.8.9, and 13.0.2 in the four combinations above, with direct adapter tests additionally confirmed on the two newer lines. | #11 is not applicable under the shared MySQL/MariaDB profile in ADR-016 because migrations require `transaction=never`; source discovery regression-tests rejection of `transaction=always`. CLI approval reuse is not part of the owner-accepted current contract for #12. Other versions and some sub-boundaries remain unverified. |

The isolated all-features workspace run recorded in `docs/supply-chain.md`
passed `killing_cli_during_a_transaction_keeps_the_next_write_blocked`. It uses
a disposable SQLite database, kills a CLI after observing a started run and
writer lock, checks the transaction's schema effects were rolled back and the
run/diagnostic lock remained, and checks a subsequent `up` returns
`SAFETY_BLOCKED`. It does not prove the other databases or exact completion
boundaries. The same log explicitly marks the then-named
`local_postgres_session_loss_during_fsm_ddl_is_not_retried` test (since extended
and renamed to `local_postgres_session_loss_during_fsm_ddl_blocks_competing_writer`),
`cli_runs_mysql_nontransactional_migration_and_recovery_journey`, and
`cli_runs_mariadb_nontransactional_migration_and_recovery_journey` ignored at
that time. Log: `/mnt/storage/projects/.scratch/dbm-fuzz-20261029/attempt-02/logs/tests.log`.

**Fresh isolated-server verification — 2026-11-07:** four opt-in recovery tests
were each run twice against rootless-Docker containers with ports bound only to
`127.0.0.1`: PostgreSQL 16.15 session loss during transactional DDL and
non-transactional index reconciliation; MySQL 8.4.11 and MariaDB 11.8.2
non-transactional CLI recovery journeys. After correcting a stale metadata
format expectation, all four test cases passed twice each (eight successful
executions). The MySQL/MariaDB journeys verify partial failures at statements
two and three,
process interruption during an in-flight statement, release of the session
lock, blocking before reconciliation, explicit reconciliation, and subsequent
recovery. The PostgreSQL transactional test verifies that backend loss does not
retry DDL and leaves no applied row or transactional steps; its separate
concurrent-index test verifies indeterminate journaling and catalog-gated
reconciliation. An initial MySQL run exposed a stale test expectation for
metadata format 3; the test now expects format 5, matching current initialization
behavior. That pre-correction failure is not included in the eight successful
executions. All three disposable containers and their anonymous volumes were
removed. These
results add evidence for scenarios 3 and 6 and part of 7; they do not close the
remaining matrix or G11.

**Latest patches within the previously declared lines — 2026-11-07:** the same
four focused tests passed twice each on PostgreSQL 18.6, MySQL 8.4.13, and
MariaDB 11.8.9, in disposable rootless-Docker containers bound to loopback. These
were the latest stable patches found for those lines in the official release
notes on that date. PostgreSQL 17.11 was not part of these focused recovery
reruns. The eight additional successful executions extend evidence for the
selected lines only; they do not replace the complete compatibility or
fault-injection matrices. The containers and their volumes were removed.

**Latest stable versions across the tested databases — 2026-11-07:** the
PostgreSQL recovery tests passed twice each on 18.6; the MySQL and MariaDB CLI
recovery journeys passed twice each on MySQL 9.7.4 and MariaDB 13.0.2. The full
workspace suite also passed against SQLite 3.54.1, linked from the official
SQLite amalgamation after verifying its published SHA3-256. The SQLite run used
`LIBSQLITE3_SYS_USE_PKG_CONFIG=1`, a local `PKG_CONFIG_PATH`, and an executable
runpath to the local build; it did not change `Cargo.toml`, `Cargo.lock`, or the
normal bundled SQLite runtime (3.53.2). Release references: [PostgreSQL
18.6](https://www.postgresql.org/docs/release/18.6/), [MySQL
9.7.4](https://dev.mysql.com/doc/relnotes/mysql/9.7/en/news-9-7-4.html),
[MariaDB 13.0.2](https://mariadb.com/docs/release-notes/community-server/changelogs/13.0/13.0.2),
and [SQLite 3.54.1](https://sqlite.org/changes.html). All external servers were
disposable rootless-Docker instances bound only to loopback and removed with
their anonymous volumes after the runs. This focused evidence does not complete
the broader per-engine integration matrices or G11.

**Competing-writer boundary — 2026-11-07:** after extending the PostgreSQL
transactional session-loss test to attempt a second `run_adapter_migration`, the
test passed twice each on disposable loopback PostgreSQL 16.15 and 18.6. The
original backend loss released the native lock; the competing writer acquired
the lock, observed the unresolved `started` run/`observed_active` lock metadata,
and returned `DbmError::Safety` without adding a run, applied row, or step. This
closes only the competing-writer check at this specific boundary; the other
matrix gaps listed above remain pending. Both disposable containers were
stopped and removed.

**SQLite contention and stale-source checks — 2026-11-07:** the default CLI
test now launches a second CLI while the first owns SQLite's writer lock,
terminates the first during SQL, and verifies that the competing process returns
`SAFETY_BLOCKED` without creating a second run. It passed twice. A separate
engine test captures a plan/manifest, changes the migration file, then verifies
the engine rejects the stale operation before either old or changed SQL takes
effect; it also passed twice. These are focused SQLite results, not proof of
equivalent behavior across the server adapters or closure of the remaining
failure boundaries.

**Shared-schema namespace race — 2026-11-07:**
`local_postgres_serializes_writers_across_namespaces_sharing_schema` passed twice
each on disposable loopback PostgreSQL 16.15 and 18.6. Two distinct namespaces
used the same randomly created schema and attempted the same schema object while
the first migration was paused. The test observed one active migration statement
only until the first completed, then verified that the second run failed safely
on the shared object, with the success/failure journals isolated by namespace
and both diagnostic locks released. Both containers were stopped and removed.
This does not establish shared-schema behavior for other database
adapters.

**Additional crash, bootstrap, and same-namespace CLI boundaries — 2026-11-07:**
`cli_process_death_at_postgres_recovery_boundaries_keeps_runs_unresolved` and
`cli_process_death_after_postgres_intent_before_sql_keeps_the_intent` passed twice
each on disposable loopback PostgreSQL 16.15 and 18.6. They exercise death before
intent, after intent/before SQL delivery, after SQL/before acknowledgement, after
acknowledgement/before finalization, and after transactional rollback/before the
failed run is recorded. Each boundary inspects persisted journal/catalog state
and verifies that a retry remains blocked. The metadata-bootstrap failure test
also passed twice each on these PostgreSQL versions and verified transactional
cleanup after a mid-bootstrap DDL error. The SQLite v4-to-v5 upgrade regression
injects a mid-rewrite name collision and verifies rollback to intact v4 state; it
passed as a targeted local test and is included in the final workspace gate.

The competing PostgreSQL CLI test passed twice each on 16.15 and 18.6. With the
first CLI's migration SQL held before it reaches the server, a second CLI waited
until its two-second configured lock timeout and returned `LOCK_UNAVAILABLE`;
only the first run was present and the held migration had no schema effect. The
first CLI, proxy, test schemas, and disposable database containers were stopped
or removed after verification. This adds selected PostgreSQL evidence only; it
does not complete the twelve-scenario matrix across all supported adapters.

**MySQL/MariaDB competing CLI boundary — 2026-11-07:**
`cli_mysql_competing_writers_same_namespace_are_serialized` and
`cli_mariadb_competing_writers_same_namespace_are_serialized` passed twice each
against disposable loopback MySQL 9.7.4 and MariaDB 13.0.2. The first CLI was
observed with a persisted intent while the server was executing `SELECT SLEEP`;
the second CLI used a zero-second lock timeout and returned `LOCK_UNAVAILABLE`.
After the first CLI completed, the test confirmed one successful run and one
schema effect, then rolled it back and dropped the isolated database. Both
containers were stopped and removed. This extends scenario 8 to those selected
server versions only; it does not close the full cross-driver matrix.

**MySQL/MariaDB partial metadata bootstrap — 2026-11-07:**
`cli_mysql_metadata_bootstrap_failure_preserves_partial_state_for_reconciliation`
and its MariaDB counterpart passed twice each against disposable loopback MySQL
9.7.4 and MariaDB 13.0.2. A test-only MySQL-protocol proxy observes and holds the
third `CREATE TABLE` before forwarding it. While held, an administrative test
connection creates the same reserved table name; releasing the request makes
the real server reject that DDL. The CLI returned `RECONCILIATION_REQUIRED`; the
test verified that the two preceding metadata tables and injected conflicting
table remain, no metadata identity or migration effect was written, and a retry
returns `STATE_DIVERGED` without changing the partial state. This exercises
non-transactional bootstrap failure handling and refusal of automatic repair; it
does not demonstrate rollback of MySQL/MariaDB DDL or cover their other supported
versions. Both disposable containers were stopped and removed.

The opt-in database tests are ignored by default. Use only the test variables
and disposable loopback database instances documented in the README; never run
these drills against a shared or production database. Results for MySQL 9.7 and
MariaDB 13.0 apply only to the focused cases named above and do not establish the
complete support matrix, all patch releases, or production readiness.

On 2026-10-29, a local offline all-features workspace test run passed the unit,
SQLite CLI, DSL, legacy-import, and registry groups, but two of five
`runner_protocol` tests exceeded their configured 30-second runner timeout.
A subsequent serial `runner_protocol` run did not finish within the local
command's 420-second limit. Unrelated Cargo test/build jobs were active and host
load was elevated during these attempts; this is a possible environmental
factor, not a demonstrated cause. The remote CI rerun for commit `ef9b172`
passed the complete quality job. Treat the local runner-protocol result as
inconclusive rather than as a passed local gate or a confirmed product defect.
These runs were ordinary development tests, not execution under the audit's
required complete sandbox and not evidence for the 12-scenario matrix.

**Owner-selected current PostgreSQL stable line and #12 scope — 2026-11-08:** the
official [PostgreSQL versioning page](https://www.postgresql.org/support/versioning/)
listed PostgreSQL 18.6 as the current minor of the latest stable major; 19 was
still a release candidate. The cached `postgres:18.6` image was run with
`--pull=never`, no persistent volume, and a port published only on loopback.
`cli_process_death_at_postgres_recovery_boundaries_keeps_runs_unresolved` and
`engine::tests::local_postgres_rejects_source_changed_after_plan_before_sql`
each passed once against that disposable server. The container was removed.
PostgreSQL 17.11 is not a required G11 target for scenario #11 under the owner's
version-selection decision; it remains a listed support-matrix version and is
not claimed verified for that boundary. The owner also accepted the existing
CLI contract for #12: there is no reusable approved-plan flow, so the
engine-level source-revalidation tests are the scoped criterion; the CLI
approval subscenario is not applicable unless a future product feature changes
that contract. Neither decision closes the remaining cross-adapter matrix.
