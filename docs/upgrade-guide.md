# Upgrade and metadata migration guide (draft)

This guide describes the current explicit metadata upgrade path. It is not a
production cutover approval. The application package, public API, and metadata
format remain provisional pending G11/G12.

## Before updating the application

1. Review the [support matrix](support-matrix.md) and confirm the exact database
   server version and topology are covered.
2. Take and verify an application-consistent backup using the database's own
   documented backup procedure. Database Migrator does not create or verify
   backups.
3. Test the upgrade and recovery procedure against an isolated disposable copy
   of the target. Never use a shared or production target for an opt-in test.
4. Preserve the migration source, current application build, configuration
   without secret values, and read-only `history`, `status`, and `doctor`
   reports. Keep connection secrets in their existing secret provider.
5. Confirm the configured namespace and database/schema identity. A namespace
   filters logical history; it does not establish physical isolation.

## Metadata formats 1–4 to format 5

Format 5 adds the state needed for repeatable migrations and explicit namespace
selection. Reads and planning do not upgrade metadata automatically.

- SQLite can upgrade formats 1–4 transactionally.
- PostgreSQL, MySQL, and MariaDB can inspect formats 3–5. Explicit v3→v5 and
  v4→v5 upgrade journeys were verified on the versions in the support matrix.
- MySQL/MariaDB upgrades use a session named lock, but DDL can commit
  implicitly. If an upgrade is interrupted or partially confirmed, stop and
  inspect before attempting any retry.

First preview the detected upgrade and inspect the target:

```sh
dbm --format json metadata upgrade
dbm --format json doctor
dbm --format json history
```

Only after reviewing the target, backup, upgrade preview, and maintenance window,
an operator may explicitly approve the metadata change with:

```sh
dbm --format json metadata upgrade --yes
```

This command mutates metadata. It is not part of ordinary reads, `plan`, or
application startup. After it completes, inspect `metadata upgrade`, `doctor`,
`history`, and `status` again, and verify that existing migration history and
run/step journal records remain present. If the result is uncertain, do not
repeat the upgrade blindly; preserve evidence and follow
[`recovery-drills.md`](recovery-drills.md).

## Application updates and migration execution

Updating the application binary does not automatically run migrations. Review
the new plan and source checksums first. `up` and `down` are separate database
mutations and must follow the normal review, approval, backup, and environment
policy. A metadata upgrade is also a separate, explicit operation.

After upgrading, check `status`, `history`, and `doctor`; compare the reported
namespace, metadata format, checksums, and unresolved runs with the saved
pre-upgrade reports. Do not proceed if a run is unresolved, the source has
drifted, or the lock may still be live.

## Rollback and recovery

Application rollback does not automatically downgrade metadata. Restore the
previous application only after verifying that it can read the current metadata
format. The current support policy does not promise an automatic v5→v4 downgrade.
Use a verified backup to restore metadata if an application rollback requires
it; test that procedure before any production change.

For an interrupted migration, do not rerun DDL based only on the absence of an
applied-migration row. Inspect every journaled statement and its database
effects. `doctor reconcile` is an explicit audit operation and does not apply or
retry SQL. PostgreSQL automatic catalog reconciliation is restricted to the
documented concurrent-index form; arbitrary MySQL/MariaDB DDL requires operator
inspection and any necessary manual repair first.

## Evidence and release status

The metadata v3/v4→v5 test evidence is local to the exact versions listed in the
support matrix. It does not establish an online, zero-downtime, production-safe,
or reversible upgrade guarantee. G11 and G12 are still pending; this guide is a
prepared artifact, not release sign-off.
