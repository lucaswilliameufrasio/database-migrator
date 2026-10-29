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
| MySQL process killed during a non-transactional statement | `tests/cli.rs::cli_runs_mysql_nontransactional_migration_and_recovery_journey` kills the CLI during `SELECT SLEEP`, checks the unresolved run and released session lock, verifies a new writer is blocked, then reconciles explicitly | Opt-in; requires disposable loopback MySQL 8.4 |
| MySQL/MariaDB failures after earlier DDL steps | The non-transactional CLI journeys inject failures at the second and third statement and inspect partial effects before reconciliation | Opt-in; requires disposable loopback MySQL/MariaDB |
| PostgreSQL migration-session loss during transactional DDL | `src/engine.rs::tests::local_postgres_session_loss_during_fsm_ddl_is_not_retried` terminates the backend while a scoped DDL trigger pauses execution and inspects durable state | Opt-in; requires disposable loopback PostgreSQL |
| Runner absent, incompatible, timed out, or emitting extra frames | `tests/runner_protocol.rs::missing_runner_timeout_and_extra_frames_fail_without_partial_sql` and adjacent handshake tests | Covered locally; timeout test uses a disposable project |
| All twelve crash boundaries, shared-schema namespace races, every metadata bootstrap/upgrade boundary, and rollback crash windows | No complete, repeatable matrix is currently recorded. The repository-wide source-only security review on 2026-10-26 did not execute target code or add fault-injection evidence. | Pending; G11 is not approved |

The opt-in database tests are ignored by default. Use only the test variables
and disposable loopback database instances documented in the README; never run
these drills against a shared or production database. The results recorded for
PostgreSQL 16/17/18, MySQL 8.4, and MariaDB 10.11/11.4/11.8 do not establish
support for unlisted server versions.

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
