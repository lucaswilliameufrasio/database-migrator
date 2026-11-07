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
| Process killed during SQLite transactional SQL | `tests/cli.rs::killing_cli_during_a_transaction_keeps_the_next_write_blocked` checks transactional rollback, retained diagnostic lock, and blocked writes | Covered locally by the default CLI test suite |
| MySQL process killed during a non-transactional statement | `tests/cli.rs::cli_runs_mysql_nontransactional_migration_and_recovery_journey` kills the CLI during `SELECT SLEEP`, checks the unresolved run and released session lock, verifies a new writer is blocked, then reconciles explicitly | Passed twice each on disposable loopback MySQL 8.4.11, 8.4.13, and 9.7.4 on 2026-11-07 |
| MySQL/MariaDB failures after earlier DDL steps | The non-transactional CLI journeys inject failures at the second and third statement and inspect partial effects before reconciliation | Passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2 on 2026-11-07 |
| PostgreSQL migration-session loss during transactional DDL | `src/engine.rs::tests::local_postgres_session_loss_during_fsm_ddl_is_not_retried` terminates the backend while a scoped DDL trigger pauses execution and inspects durable state | Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6 on 2026-11-07 |
| PostgreSQL failed non-transactional concurrent-index migration | `src/engine.rs::tests::local_postgres_nontransactional_failure_is_journaled_indeterminate` checks the indeterminate journal and catalog-gated explicit reconciliation | Passed twice each on disposable loopback PostgreSQL 16.15 and 18.6 on 2026-11-07 |
| Runner absent, incompatible, timed out, or emitting extra frames | `tests/runner_protocol.rs::missing_runner_timeout_and_extra_frames_fail_without_partial_sql` and adjacent handshake tests | Covered locally; timeout test uses a disposable project |
| All twelve crash boundaries, shared-schema namespace races, every metadata bootstrap/upgrade boundary, and rollback crash windows | No complete, repeatable matrix is currently recorded. The repository-wide source-only security review on 2026-10-26 did not execute target code or add fault-injection evidence. | Pending; G11 is not approved |

### Matrix traceability as of 2026-11-07

The numbered rows below correspond to section 13.3 of the development plan.
They are a gap inventory, not a claim that source inspection or a related test
proves a whole scenario across every database and failure boundary.

| # | Required boundary | Evidence and remaining gap |
|---|---|---|
| 1 | Death before intent | No targeted kill-boundary evidence identified; pending. |
| 2 | Death after intent, before SQL | No targeted kill-boundary evidence identified; pending. |
| 3 | Death during SQL | SQLite CLI kill, MySQL/MariaDB CLI interruption during a non-transactional statement, and PostgreSQL backend loss during transactional DDL now pass on the versions listed above. The fault mechanisms differ by database semantics; other crash boundaries and versions remain unverified. |
| 4 | SQL completed, death before acknowledgement | No targeted kill-boundary evidence identified; pending. |
| 5 | Acknowledgement persisted, death before final state | No targeted kill-boundary evidence identified; pending. |
| 6 | MySQL/MariaDB partial DDL, third step fails | Second- and third-statement partial-failure journeys passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB 11.8.2/11.8.9/13.0.2; earlier DDL effects were inspected and reconciled explicitly. Other server versions remain unverified. |
| 7 | Native-lock session loss and competing instance | PostgreSQL 16.15/18.6 session-loss-during-DDL tests passed twice each and confirmed durable diagnostic state/no automatic retry. A competing writer at this exact boundary remains unverified. |
| 8 | Two CLIs, same DB and namespace | No targeted two-CLI fault-matrix evidence identified; pending. |
| 9 | Two namespaces sharing schema object | No targeted race evidence identified; pending. |
| 10 | Failure during metadata bootstrap/upgrade | No complete injected-failure boundary evidence identified; pending. |
| 11 | Rollback then crash before run recorded | No targeted kill-boundary evidence identified; pending. |
| 12 | Plan and source changed between approval and execution | Import-specific drift/digest tests passed, but they do not establish the general migration approval/execution boundary; pending. |

The isolated all-features workspace run recorded in `docs/supply-chain.md`
passed `killing_cli_during_a_transaction_keeps_the_next_write_blocked`. It uses
a disposable SQLite database, kills a CLI after observing a started run and
writer lock, checks the transaction's schema effects were rolled back and the
run/diagnostic lock remained, and checks a subsequent `up` returns
`SAFETY_BLOCKED`. It does not prove the other databases or exact completion
boundaries. The same log explicitly marks
`local_postgres_session_loss_during_fsm_ddl_is_not_retried`,
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
