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

### Matrix traceability as of 2026-11-06

The numbered rows below correspond to section 13.3 of the development plan.
They are a gap inventory, not a claim that source inspection or a related test
proves a whole scenario across every database and failure boundary.

| # | Required boundary | Evidence and remaining gap |
|---|---|---|
| 1 | Death before intent | No targeted kill-boundary evidence identified; pending. |
| 2 | Death after intent, before SQL | No targeted kill-boundary evidence identified; pending. |
| 3 | Death during SQL | SQLite CLI kill test below passed locally; MySQL and PostgreSQL tests exist but were ignored in that local run. Cross-driver scenario remains incomplete. |
| 4 | SQL completed, death before acknowledgement | No targeted kill-boundary evidence identified; pending. |
| 5 | Acknowledgement persisted, death before final state | No targeted kill-boundary evidence identified; pending. |
| 6 | MySQL/MariaDB partial DDL, third step fails | Opt-in CLI journeys include partial-failure cases; both were ignored locally. Pending fresh isolated-server evidence. |
| 7 | Native-lock session loss and competing instance | PostgreSQL session-loss test exists but was ignored locally; competing-instance behavior at this boundary remains unverified. |
| 8 | Two CLIs, same DB and namespace | No targeted two-CLI fault-matrix evidence identified; pending. |
| 9 | Two namespaces sharing schema object | No targeted race evidence identified; pending. |
| 10 | Failure during metadata bootstrap/upgrade | No complete injected-failure boundary evidence identified; pending. |
| 11 | Rollback then crash before run recorded | No targeted kill-boundary evidence identified; pending. |
| 12 | Plan and source changed between approval and execution | Import-specific drift/digest tests passed, but they do not establish the general migration approval/execution boundary; pending. |

In the isolated all-features workspace run recorded in `docs/supply-chain.md`,
`killing_cli_during_a_transaction_keeps_the_next_write_blocked` passed. It uses
a disposable SQLite database, kills a CLI after observing a started run and
writer lock, checks the transaction's schema effects were rolled back and the
run/diagnostic lock remained, and checks a subsequent `up` returns
`SAFETY_BLOCKED`. It does not prove the other databases or exact completion
boundaries. The same log explicitly marks
`local_postgres_session_loss_during_fsm_ddl_is_not_retried`,
`cli_runs_mysql_nontransactional_migration_and_recovery_journey`, and
`cli_runs_mariadb_nontransactional_migration_and_recovery_journey` ignored.
Log: `/mnt/storage/projects/.scratch/dbm-fuzz-20261029/attempt-02/logs/tests.log`.

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
