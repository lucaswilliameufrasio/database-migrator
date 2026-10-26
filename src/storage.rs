use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use serde::Serialize;

use crate::config::LoadedConfig;
use crate::error::{DbmError, Result};
use crate::migration::{Migration, MigrationKind};

pub const METADATA_FORMAT_VERSION: i64 = 5;
pub const PREVIOUS_METADATA_FORMAT_VERSION: i64 = 4;
pub const LEGACY_METADATA_FORMAT_VERSION: i64 = 1;
pub const SECOND_PREVIOUS_METADATA_FORMAT_VERSION: i64 = 2;
pub const THIRD_PREVIOUS_METADATA_FORMAT_VERSION: i64 = 3;

#[derive(Clone, Debug, Default)]
pub struct MetadataSnapshot {
    pub applied: Vec<AppliedMigration>,
    pub legacy_applied: Vec<LegacyAppliedMigration>,
    pub legacy_imports: Vec<LegacyImportRecord>,
    pub legacy_import_events: Vec<LegacyImportEventRecord>,
    pub runs: Vec<RunRecord>,
    pub steps: Vec<StepRecord>,
    pub lock: Option<LockRecord>,
    pub initialized: bool,
    pub metadata_format_version: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct AppliedMigration {
    pub migration_key: String,
    pub kind: String,
    pub version: String,
    pub filename: String,
    pub source_path: String,
    pub description: String,
    pub checksum: String,
    pub apply_order: i64,
    pub applied_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyAppliedMigration {
    pub migration_key: String,
    pub import_id: String,
    pub origin_tool: String,
    pub source_identity: String,
    pub origin_identity: String,
    pub original_version: String,
    pub filename: Option<String>,
    pub description: Option<String>,
    pub batch_id: Option<String>,
    pub applied_at: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub checksum_value: Option<String>,
    pub execution_time_ms: Option<i64>,
    pub verification_status: String,
    pub apply_order: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportRecord {
    pub import_id: String,
    pub namespace: String,
    pub origin_tool: String,
    pub source_identity: String,
    pub snapshot_digest: String,
    pub plan_digest: String,
    pub imported_at: String,
    pub event_count: i64,
    pub current_migration_count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportEventRecord {
    pub import_id: String,
    pub event_index: i64,
    pub event_id: String,
    pub original_version: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub batch_id: String,
    pub namespace: String,
    pub migration_id: String,
    pub direction: String,
    pub state: String,
    pub attempt: i64,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    pub checksum_at_execution: String,
    pub plan_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_summary_redacted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_evidence_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciled_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciled_from_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepRecord {
    pub run_id: String,
    pub step_index: i64,
    pub step_key: String,
    pub direction: String,
    pub statement_hash: String,
    pub statement_kind: String,
    pub transaction_class: String,
    pub state: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LockRecord {
    pub scope: String,
    pub owner_id: String,
    pub lock_strategy: String,
    pub acquired_at: String,
    pub last_observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub lock_epoch: i64,
    pub state: String,
}

pub struct PlannedRunData<'a> {
    pub run_id: &'a str,
    pub batch_id: &'a str,
    pub direction: &'a str,
    pub created_at: &'a str,
    pub plan_hash: &'a str,
}

pub struct FinishedRunData<'a> {
    pub run_id: &'a str,
    pub state: &'a str,
    pub ended_at: &'a str,
    pub duration_ms: i64,
    pub error_code: Option<&'a str>,
    pub error_summary_redacted: Option<&'a str>,
}

pub struct StepData<'a> {
    pub run_id: &'a str,
    pub step_index: i64,
    pub step_key: &'a str,
    pub direction: &'a str,
    pub statement_hash: &'a str,
    pub statement_kind: &'a str,
    pub transaction_class: &'a str,
    pub state: &'a str,
    pub started_at: &'a str,
    pub acknowledged_at: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub evidence: Option<&'a str>,
}

pub struct ReconciliationData<'a> {
    pub run_id: &'a str,
    pub reconcile_run_id: &'a str,
    pub result: &'a str,
    pub evidence_ref: &'a str,
    pub repair_plan: &'a str,
    pub reason: &'a str,
    pub actor: &'a str,
    pub now: &'a str,
}

pub struct LockData<'a> {
    pub owner_id: &'a str,
    pub run_id: &'a str,
    pub now: &'a str,
}

pub fn open_readonly(config: &LoadedConfig) -> Result<Option<Connection>> {
    let database_path = config.sqlite_database_path()?;
    if !database_path.exists() {
        return Ok(None);
    }

    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(
        config.config.execution.busy_timeout_seconds,
    ))?;
    Ok(Some(connection))
}

pub fn open_mutable(config: &LoadedConfig) -> Result<Connection> {
    let connection = Connection::open(config.sqlite_database_path()?)?;
    connection.busy_timeout(Duration::from_secs(
        config.config.execution.busy_timeout_seconds,
    ))?;
    Ok(connection)
}

pub fn read_snapshot(config: &LoadedConfig) -> Result<MetadataSnapshot> {
    let Some(connection) = open_readonly(config)? else {
        return Ok(MetadataSnapshot::default());
    };
    read_snapshot_from_connection(&connection, &config.config.namespace)
}

pub fn read_snapshot_from_connection(
    connection: &Connection,
    namespace: &str,
) -> Result<MetadataSnapshot> {
    let presence = metadata_presence(connection)?;
    if presence == 0 {
        if metadata_v4_presence(connection)? != 0 {
            return Err(DbmError::Diverged(
                "legacy import metadata exists without the core metadata tables".to_owned(),
            ));
        }
        return Ok(MetadataSnapshot::default());
    }
    if presence != 5 {
        return Err(DbmError::Diverged(
            "metadata tables are incomplete or use a legacy format; inspect with doctor and use `dbm metadata upgrade --yes` when offered".to_owned(),
        ));
    }

    let (format_version, installation_id): (i64, String) = connection
        .query_row(
            "SELECT metadata_format_version, installation_id \
             FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| {
            DbmError::Diverged(format!("metadata identity row is invalid: {error}"))
        })?;

    if format_version == LEGACY_METADATA_FORMAT_VERSION {
        return Err(DbmError::Diverged(
            "metadata format 1 requires explicit upgrade with `dbm metadata upgrade --yes`"
                .to_owned(),
        ));
    }
    if !matches!(
        format_version,
        SECOND_PREVIOUS_METADATA_FORMAT_VERSION
            | THIRD_PREVIOUS_METADATA_FORMAT_VERSION
            | PREVIOUS_METADATA_FORMAT_VERSION
            | METADATA_FORMAT_VERSION
    ) || installation_id.is_empty()
    {
        return Err(DbmError::Diverged(format!(
            "unsupported or invalid metadata format {format_version}"
        )));
    }
    if format_version >= PREVIOUS_METADATA_FORMAT_VERSION && metadata_v4_presence(connection)? != 8
    {
        return Err(DbmError::Diverged(
            "metadata format 4 is missing one or more import/adoption tables".to_owned(),
        ));
    }

    let applied = if format_version >= METADATA_FORMAT_VERSION {
        let mut statement = connection.prepare(
            "SELECT migration_key, kind, \
                    CASE kind WHEN 'repeatable' THEN repeatable_name ELSE native_version END, \
                    filename, source_path, description, content_checksum, apply_order, applied_at \
             FROM main.schema_migrations \
             WHERE namespace = ?1 AND kind IN ('versioned', 'repeatable') \
             ORDER BY apply_order ASC, kind ASC, native_version ASC, repeatable_name ASC",
        )?;
        statement
            .query_map([namespace], |row| {
                Ok(AppliedMigration {
                    migration_key: row.get(0)?,
                    kind: row.get(1)?,
                    version: row.get(2)?,
                    filename: row.get(3)?,
                    source_path: row.get(4)?,
                    description: row.get(5)?,
                    checksum: row.get(6)?,
                    apply_order: row.get(7)?,
                    applied_at: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let mut statement = connection.prepare(
            "SELECT migration_key, native_version, filename, source_path, \
                    description, content_checksum, apply_order, applied_at \
             FROM main.schema_migrations \
             WHERE namespace = ?1 AND kind = 'versioned' \
             ORDER BY apply_order ASC, native_version ASC",
        )?;
        statement
            .query_map([namespace], |row| {
                Ok(AppliedMigration {
                    migration_key: row.get(0)?,
                    kind: "versioned".to_owned(),
                    version: row.get(1)?,
                    filename: row.get(2)?,
                    source_path: row.get(3)?,
                    description: row.get(4)?,
                    checksum: row.get(5)?,
                    apply_order: row.get(6)?,
                    applied_at: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };

    let (legacy_applied, legacy_imports, legacy_import_events) =
        if format_version >= PREVIOUS_METADATA_FORMAT_VERSION {
            let mut legacy_statement = connection.prepare(
                "SELECT migration_key, legacy_import_id, origin_tool, origin_database_identity, \
                        origin_identity, original_version, filename, description, batch_id, \
                        origin_applied_at, checksum_algorithm, checksum_value, execution_time_ms, \
                        verification_status, apply_order \
                 FROM main.schema_migrations WHERE namespace = ?1 AND kind = 'legacy' \
                 ORDER BY apply_order ASC, migration_key ASC",
            )?;
            let legacy_applied = legacy_statement
                .query_map([namespace], |row| {
                    Ok(LegacyAppliedMigration {
                        migration_key: row.get(0)?,
                        import_id: row.get(1)?,
                        origin_tool: row.get(2)?,
                        source_identity: row.get(3)?,
                        origin_identity: row.get(4)?,
                        original_version: row.get(5)?,
                        filename: row.get(6)?,
                        description: row.get(7)?,
                        batch_id: row.get(8)?,
                        applied_at: row.get(9)?,
                        checksum_algorithm: row.get(10)?,
                        checksum_value: row.get(11)?,
                        execution_time_ms: row.get(12)?,
                        verification_status: row.get(13)?,
                        apply_order: row.get(14)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let mut import_statement = connection.prepare(
                "SELECT import_id, namespace, origin_tool, source_identity, snapshot_digest, \
                        plan_digest, imported_at, event_count, current_migration_count \
                 FROM main.schema_migration_imports WHERE namespace = ?1 \
                 ORDER BY imported_at ASC, import_id ASC",
            )?;
            let legacy_imports = import_statement
                .query_map([namespace], |row| {
                    Ok(LegacyImportRecord {
                        import_id: row.get(0)?,
                        namespace: row.get(1)?,
                        origin_tool: row.get(2)?,
                        source_identity: row.get(3)?,
                        snapshot_digest: row.get(4)?,
                        plan_digest: row.get(5)?,
                        imported_at: row.get(6)?,
                        event_count: row.get(7)?,
                        current_migration_count: row.get(8)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let mut event_statement = connection.prepare(
                "SELECT events.import_id, events.event_index, events.event_id, \
                        events.original_version, events.state, events.filename, events.batch_id, \
                        events.applied_at, events.checksum_algorithm, events.checksum_value, \
                        events.execution_time_ms \
                 FROM main.schema_migration_import_events AS events \
                 INNER JOIN main.schema_migration_imports AS imports USING (import_id) \
                 WHERE imports.namespace = ?1 \
                 ORDER BY events.import_id ASC, events.event_index ASC",
            )?;
            let legacy_import_events = event_statement
                .query_map([namespace], |row| {
                    Ok(LegacyImportEventRecord {
                        import_id: row.get(0)?,
                        event_index: row.get(1)?,
                        event_id: row.get(2)?,
                        original_version: row.get(3)?,
                        state: row.get(4)?,
                        filename: row.get(5)?,
                        batch_id: row.get(6)?,
                        applied_at: row.get(7)?,
                        checksum_algorithm: row.get(8)?,
                        checksum_value: row.get(9)?,
                        execution_time_ms: row.get(10)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            (legacy_applied, legacy_imports, legacy_import_events)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

    let mut runs_statement = connection.prepare(
        "SELECT run_id, batch_id, namespace, migration_version, direction, state, attempt, \
                created_at, started_at, ended_at, duration_ms, checksum_at_execution, \
                plan_hash, error_code, error_summary_redacted, reconciliation_result, \
                reconciliation_plan, reconciliation_evidence_ref, reconciliation_reason, reconciled_by, \
                reconciled_from_state, related_run_id \
         FROM main.schema_migration_runs \
         WHERE namespace = ?1 \
         ORDER BY created_at ASC, run_id ASC",
    )?;
    let runs = runs_statement
        .query_map([namespace], |row| {
            Ok(RunRecord {
                run_id: row.get(0)?,
                batch_id: row.get(1)?,
                namespace: row.get(2)?,
                migration_id: row.get(3)?,
                direction: row.get(4)?,
                state: row.get(5)?,
                attempt: row.get(6)?,
                created_at: row.get(7)?,
                started_at: row.get(8)?,
                ended_at: row.get(9)?,
                duration_ms: row.get(10)?,
                checksum_at_execution: row.get(11)?,
                plan_hash: row.get(12)?,
                error_code: row.get(13)?,
                error_summary_redacted: row.get(14)?,
                reconciliation_result: row.get(15)?,
                reconciliation_plan: row.get(16)?,
                reconciliation_evidence_ref: row.get(17)?,
                reconciliation_reason: row.get(18)?,
                reconciled_by: row.get(19)?,
                reconciled_from_state: row.get(20)?,
                related_run_id: row.get(21)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut steps_statement = connection.prepare(
        "SELECT schema_migration_steps.run_id, schema_migration_steps.step_index, \
                schema_migration_steps.step_key, schema_migration_steps.direction, \
                schema_migration_steps.statement_hash, schema_migration_steps.statement_kind, \
                schema_migration_steps.transaction_class, schema_migration_steps.state, \
                schema_migration_steps.started_at, schema_migration_steps.acknowledged_at, \
                schema_migration_steps.error_code, schema_migration_steps.evidence \
         FROM main.schema_migration_steps \
         INNER JOIN main.schema_migration_runs USING (run_id) \
         WHERE schema_migration_runs.namespace = ?1 \
         ORDER BY step_index ASC",
    )?;
    let steps = steps_statement
        .query_map([namespace], |row| {
            Ok(StepRecord {
                run_id: row.get(0)?,
                step_index: row.get(1)?,
                step_key: row.get(2)?,
                direction: row.get(3)?,
                statement_hash: row.get(4)?,
                statement_kind: row.get(5)?,
                transaction_class: row.get(6)?,
                state: row.get(7)?,
                started_at: row.get(8)?,
                acknowledged_at: row.get(9)?,
                error_code: row.get(10)?,
                evidence: row.get(11)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let lock = connection
        .query_row(
            "SELECT scope, owner_id, lock_strategy, acquired_at, last_observed_at, \
                    run_id, lock_epoch, state \
             FROM main.schema_migration_locks WHERE scope = 'database'",
            [],
            |row| {
                Ok(LockRecord {
                    scope: row.get(0)?,
                    owner_id: row.get(1)?,
                    lock_strategy: row.get(2)?,
                    acquired_at: row.get(3)?,
                    last_observed_at: row.get(4)?,
                    run_id: row.get(5)?,
                    lock_epoch: row.get(6)?,
                    state: row.get(7)?,
                })
            },
        )
        .optional()?;

    Ok(MetadataSnapshot {
        applied,
        legacy_applied,
        legacy_imports,
        legacy_import_events,
        runs,
        steps,
        lock,
        initialized: true,
        metadata_format_version: Some(format_version),
    })
}

pub fn ensure_schema(connection: &Connection, initialized_at: &str) -> Result<()> {
    let presence = metadata_presence(connection)?;
    if presence == 5 {
        let snapshot = read_snapshot_from_connection(connection, "__dbm_metadata_probe__")?;
        if !snapshot.initialized {
            return Err(DbmError::Diverged(
                "metadata tables exist without a valid metadata identity".to_owned(),
            ));
        }
        if snapshot.metadata_format_version != Some(METADATA_FORMAT_VERSION)
            || metadata_v4_presence(connection)? != 8
        {
            return Err(DbmError::Safety(
                "SQLite metadata requires an explicit upgrade with `dbm metadata upgrade --yes` before migration writes"
                    .to_owned(),
            ));
        }
        return Ok(());
    }
    if presence != 0 || metadata_v4_presence(connection)? != 0 {
        return Err(DbmError::Diverged(
            "metadata tables are incomplete or legacy; refusing automatic repair or upgrade"
                .to_owned(),
        ));
    }

    connection.execute_batch(
        "CREATE TABLE main.schema_migration_meta (\
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\
             metadata_format_version INTEGER NOT NULL,\
             installation_id TEXT NOT NULL,\
             initialized_at TEXT NOT NULL,\
             upgraded_at TEXT\
         );\
         CREATE TABLE main.schema_migrations (\
              namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
              migration_key TEXT NOT NULL CHECK (length(migration_key) = 64),\
               kind TEXT NOT NULL CHECK (kind IN ('versioned', 'repeatable', 'legacy')),\
               native_version TEXT CHECK (native_version IS NULL OR length(native_version) = 20),\
               repeatable_name TEXT CHECK (repeatable_name IS NULL OR length(repeatable_name) BETWEEN 1 AND 128),\
              original_version TEXT,\
              filename TEXT,\
              source_path TEXT,\
              description TEXT,\
              authoring_type TEXT NOT NULL CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),\
              content_checksum TEXT,\
              origin_tool TEXT,\
              origin_database_identity TEXT,\
              origin_identity TEXT,\
              legacy_import_id TEXT REFERENCES schema_migration_imports(import_id),\
              batch_id TEXT,\
              checksum_algorithm TEXT,\
              checksum_value TEXT,\
              execution_time_ms INTEGER CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
              verification_status TEXT CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),\
              apply_order INTEGER NOT NULL,\
              applied_at TEXT,\
              origin_applied_at TEXT,\
              observed_at TEXT,\
               PRIMARY KEY (namespace, migration_key),\
               UNIQUE (namespace, kind, native_version),\
               UNIQUE (namespace, kind, repeatable_name),\
                CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND repeatable_name IS NULL AND original_version IS NULL \
                         AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                       (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL \
                         AND original_version IS NULL AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                       (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL \
                        AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL \
                        AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL \
                        AND verification_status IS NOT NULL AND observed_at IS NOT NULL))\
          );\
         CREATE TABLE main.schema_migration_runs (\
              run_id TEXT PRIMARY KEY,\
              batch_id TEXT NOT NULL,\
              namespace TEXT NOT NULL,\
              migration_key TEXT NOT NULL,\
              migration_version TEXT NOT NULL,\
               direction TEXT NOT NULL CHECK (direction IN ('up', 'down', 'repeat', 'reconcile')),\
              state TEXT NOT NULL CHECK (state IN ('planned', 'started', 'succeeded', 'failed', 'indeterminate', 'reconciled')),\
              attempt INTEGER NOT NULL CHECK (attempt >= 1),\
              created_at TEXT NOT NULL,\
              started_at TEXT,\
              ended_at TEXT,\
              duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),\
              checksum_at_execution TEXT NOT NULL,\
              plan_hash TEXT NOT NULL,\
              error_code TEXT,\
              error_summary_redacted TEXT,\
              reconciliation_result TEXT CHECK (reconciliation_result IS NULL OR reconciliation_result IN ('operation_applied', 'operation_not_applied')),\
              reconciliation_plan TEXT,\
              reconciliation_evidence_ref TEXT,\
              reconciliation_reason TEXT,\
              reconciled_by TEXT,\
              reconciled_from_state TEXT,\
              related_run_id TEXT REFERENCES schema_migration_runs(run_id),\
              actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ci', 'embedded', 'agent_proposal'))\
          );\
          CREATE INDEX main.schema_migration_runs_namespace_created_idx ON \
              schema_migration_runs(namespace, created_at, run_id);\
          CREATE TABLE main.schema_migration_steps (\
              run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
              step_index INTEGER NOT NULL CHECK (step_index >= 0),\
              step_key TEXT NOT NULL,\
              direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
              statement_hash TEXT NOT NULL,\
              statement_kind TEXT NOT NULL,\
              transaction_class TEXT NOT NULL CHECK (transaction_class IN ('transactional', 'non_transactional')),\
              state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
              started_at TEXT NOT NULL,\
              acknowledged_at TEXT,\
              error_code TEXT,\
              evidence TEXT,\
              attempt_id TEXT NOT NULL,\
              PRIMARY KEY (run_id, step_index)\
          );\
           CREATE TABLE main.schema_migration_locks (\
              scope TEXT PRIMARY KEY,\
              owner_id TEXT NOT NULL,\
              session_id_redacted TEXT,\
              lock_strategy TEXT NOT NULL,\
              acquired_at TEXT NOT NULL,\
              last_observed_at TEXT NOT NULL,\
              run_id TEXT,\
              lock_epoch INTEGER NOT NULL CHECK (lock_epoch >= 1),\
               state TEXT NOT NULL CHECK (state IN ('observed_active', 'released', 'lost', 'unknown'))\
           );\
           CREATE TABLE main.schema_migration_imports (\
               import_id TEXT PRIMARY KEY,\
               namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
               origin_tool TEXT NOT NULL CHECK (origin_tool IN ('knex', 'goose', 'sqlx')),\
               source_identity TEXT NOT NULL,\
               snapshot_digest TEXT NOT NULL,\
               plan_digest TEXT NOT NULL,\
               imported_at TEXT NOT NULL,\
               event_count INTEGER NOT NULL CHECK (event_count >= 0),\
               current_migration_count INTEGER NOT NULL CHECK (current_migration_count >= 0),\
               UNIQUE (namespace, import_id)\
           );\
           CREATE TABLE main.schema_migration_import_events (\
               import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),\
               event_index INTEGER NOT NULL CHECK (event_index >= 0),\
               event_id TEXT NOT NULL,\
               original_version TEXT NOT NULL,\
               state TEXT NOT NULL CHECK (state IN ('applied', 'reverted', 'failed', 'sentinel')),\
               filename TEXT,\
               batch_id TEXT,\
               applied_at TEXT,\
               checksum_algorithm TEXT,\
               checksum_value TEXT,\
               execution_time_ms INTEGER CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
               PRIMARY KEY (import_id, event_index)\
           );\
           CREATE TABLE main.schema_migration_adoptions (\
               adoption_id TEXT PRIMARY KEY,\
               namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
               import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),\
               legacy_migration_key TEXT NOT NULL CHECK (length(legacy_migration_key) = 64),\
               native_migration_key TEXT NOT NULL CHECK (length(native_migration_key) = 64),\
               native_version TEXT NOT NULL CHECK (length(native_version) = 20),\
               native_checksum TEXT NOT NULL,\
               evidence_ref TEXT NOT NULL,\
               approved_by TEXT NOT NULL,\
               reason TEXT NOT NULL,\
               adopted_at TEXT NOT NULL,\
               UNIQUE (namespace, legacy_migration_key)\
           );",
    )?;
    connection.execute(
        "INSERT INTO main.schema_migration_meta \
         (singleton, metadata_format_version, installation_id, initialized_at) \
         VALUES (1, ?1, lower(hex(randomblob(16))), ?2)",
        rusqlite::params![METADATA_FORMAT_VERSION, initialized_at],
    )?;
    Ok(())
}

pub fn insert_applied(
    transaction: &Connection,
    migration: &Migration,
    applied_at: &str,
) -> Result<()> {
    let key = migration.migration_key();
    let apply_order: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(apply_order), 0) + 1 \
         FROM main.schema_migrations WHERE namespace = ?1",
        [&migration.identity.namespace],
        |row| row.get(0),
    )?;

    match migration.kind {
        MigrationKind::Versioned => {
            transaction.execute(
                "INSERT INTO main.schema_migrations \
                 (namespace, migration_key, kind, native_version, repeatable_name, filename, \
                  source_path, description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES (?1, ?2, 'versioned', ?3, NULL, ?4, ?5, ?6, 'sql', ?7, ?8, ?9)",
                rusqlite::params![
                    migration.identity.namespace,
                    key,
                    migration.identity.version,
                    migration.filename,
                    migration.source_path,
                    migration.description,
                    migration.checksum,
                    apply_order,
                    applied_at,
                ],
            )?;
        }
        MigrationKind::Repeatable => {
            transaction.execute(
                "INSERT INTO main.schema_migrations \
                 (namespace, migration_key, kind, native_version, repeatable_name, filename, \
                  source_path, description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES (?1, ?2, 'repeatable', NULL, ?3, ?4, ?5, ?6, 'sql', ?7, ?8, ?9) \
                 ON CONFLICT(namespace, migration_key) DO UPDATE SET \
                    repeatable_name = excluded.repeatable_name, filename = excluded.filename, \
                    source_path = excluded.source_path, description = excluded.description, \
                    content_checksum = excluded.content_checksum, apply_order = excluded.apply_order, \
                    applied_at = excluded.applied_at",
                rusqlite::params![
                    migration.identity.namespace,
                    key,
                    migration.identity.version,
                    migration.filename,
                    migration.source_path,
                    migration.description,
                    migration.checksum,
                    apply_order,
                    applied_at,
                ],
            )?;
        }
    }
    Ok(())
}

pub fn delete_applied(transaction: &Connection, migration: &Migration) -> Result<()> {
    if migration.kind == MigrationKind::Repeatable {
        return Err(DbmError::Safety(
            "repeatable migrations do not support automatic rollback".to_owned(),
        ));
    }
    let key = migration.migration_key();
    let deleted = transaction.execute(
        "DELETE FROM main.schema_migrations \
         WHERE namespace = ?1 AND migration_key = ?2 AND native_version = ?3",
        rusqlite::params![
            migration.identity.namespace,
            key,
            migration.identity.version,
        ],
    )?;
    if deleted != 1 {
        return Err(DbmError::Diverged(format!(
            "applied state for migration {} changed during rollback",
            migration.identity.version
        )));
    }
    Ok(())
}

pub fn insert_planned_run(
    transaction: &Connection,
    migration: &Migration,
    run: PlannedRunData<'_>,
) -> Result<()> {
    let attempt: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(attempt), 0) + 1 FROM main.schema_migration_runs \
         WHERE namespace = ?1 AND migration_key = ?2 AND direction = ?3",
        rusqlite::params![
            migration.identity.namespace,
            migration.migration_key(),
            run.direction,
        ],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO main.schema_migration_runs \
         (run_id, batch_id, namespace, migration_key, migration_version, direction, \
          state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', ?7, ?8, ?9, ?10, 'human')",
        rusqlite::params![
            run.run_id,
            run.batch_id,
            migration.identity.namespace,
            migration.migration_key(),
            migration.identity.version,
            run.direction,
            attempt,
            run.created_at,
            migration.checksum,
            run.plan_hash,
        ],
    )?;
    Ok(())
}

pub fn mark_run_started(transaction: &Connection, run_id: &str, started_at: &str) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_runs SET state = 'started', started_at = ?1 \
         WHERE run_id = ?2 AND state = 'planned'",
        rusqlite::params![started_at, run_id],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(format!(
            "run {run_id} cannot transition from its current state to started"
        )));
    }
    Ok(())
}

pub fn finish_run(transaction: &Connection, run: FinishedRunData<'_>) -> Result<()> {
    if !matches!(run.state, "succeeded" | "failed" | "indeterminate") {
        return Err(DbmError::Execution(format!(
            "internal error: invalid terminal run state {}",
            run.state
        )));
    }
    let changed = transaction.execute(
        "UPDATE main.schema_migration_runs \
         SET state = ?1, ended_at = ?2, duration_ms = ?3, error_code = ?4, \
             error_summary_redacted = ?5 \
         WHERE run_id = ?6 AND state = 'started'",
        rusqlite::params![
            run.state,
            run.ended_at,
            run.duration_ms,
            run.error_code,
            run.error_summary_redacted,
            run.run_id,
        ],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(format!(
            "run {} cannot transition from its current state to {}",
            run.run_id, run.state
        )));
    }
    Ok(())
}

pub fn reconcile_run(
    transaction: &Transaction<'_>,
    reconciliation: ReconciliationData<'_>,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_runs \
         SET reconciled_from_state = state, state = 'reconciled', \
             reconciliation_result = ?1, reconciliation_plan = ?2, \
             reconciliation_evidence_ref = ?3, reconciliation_reason = ?4, reconciled_by = ?5 \
         WHERE run_id = ?6 AND state != 'reconciled'",
        rusqlite::params![
            reconciliation.result,
            reconciliation.repair_plan,
            reconciliation.evidence_ref,
            reconciliation.reason,
            reconciliation.actor,
            reconciliation.run_id,
        ],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(format!(
            "run {} is missing or already reconciled",
            reconciliation.run_id
        )));
    }

    transaction.execute(
        "INSERT INTO main.schema_migration_runs \
         (run_id, batch_id, namespace, migration_key, migration_version, direction, state, \
          attempt, created_at, started_at, ended_at, duration_ms, checksum_at_execution, \
          plan_hash, reconciliation_result, reconciliation_plan, reconciliation_evidence_ref, \
          reconciliation_reason, reconciled_by, related_run_id, actor_kind) \
         SELECT ?1, batch_id, namespace, migration_key, migration_version, 'reconcile', 'succeeded', \
                1, ?2, ?2, ?2, 0, checksum_at_execution, plan_hash, ?3, ?4, ?5, ?6, ?7, run_id, 'human' \
         FROM main.schema_migration_runs WHERE run_id = ?8",
        rusqlite::params![
            reconciliation.reconcile_run_id,
            reconciliation.now,
            reconciliation.result,
            reconciliation.repair_plan,
            reconciliation.evidence_ref,
            reconciliation.reason,
            reconciliation.actor,
            reconciliation.run_id,
        ],
    )?;
    transaction.execute(
        "UPDATE main.schema_migration_steps \
         SET state = 'reconciled', evidence = ?1 \
         WHERE run_id = ?2 AND state IN ('intent_recorded', 'indeterminate')",
        rusqlite::params![
            "step state was reconciled by an approved operator decision",
            reconciliation.run_id,
        ],
    )?;
    Ok(())
}

pub fn insert_step(transaction: &Connection, step: StepData<'_>) -> Result<()> {
    transaction.execute(
        "INSERT INTO main.schema_migration_steps \
         (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
          transaction_class, state, started_at, acknowledged_at, error_code, evidence, attempt_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?1)",
        rusqlite::params![
            step.run_id,
            step.step_index,
            step.step_key,
            step.direction,
            step.statement_hash,
            step.statement_kind,
            step.transaction_class,
            step.state,
            step.started_at,
            step.acknowledged_at,
            step.error_code,
            step.evidence,
        ],
    )?;
    Ok(())
}

pub fn acknowledge_step(
    transaction: &Connection,
    run_id: &str,
    step_index: i64,
    acknowledged_at: &str,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_steps SET state = 'acknowledged', acknowledged_at = ?1 \
         WHERE run_id = ?2 AND step_index = ?3 AND state = 'intent_recorded'",
        rusqlite::params![acknowledged_at, run_id, step_index],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(format!(
            "step {step_index} for run {run_id} cannot be acknowledged"
        )));
    }
    Ok(())
}

pub fn mark_step_indeterminate(
    transaction: &Connection,
    run_id: &str,
    step_index: i64,
    error_code: &str,
    evidence: &str,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_steps \
         SET state = 'indeterminate', error_code = ?1, evidence = ?2 \
         WHERE run_id = ?3 AND step_index = ?4 AND state = 'intent_recorded'",
        rusqlite::params![error_code, evidence, run_id, step_index],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(format!(
            "step {step_index} for run {run_id} cannot be marked indeterminate"
        )));
    }
    Ok(())
}

pub fn acquire_lock(transaction: &Connection, lock: LockData<'_>) -> Result<i64> {
    let current: Option<(String, String, i64)> = transaction
        .query_row(
            "SELECT owner_id, state, lock_epoch FROM main.schema_migration_locks \
             WHERE scope = 'database'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    match current {
        Some((_, state, _)) if state != "released" => Err(DbmError::Lock(
                "database migration lock metadata is unresolved; inspect with `dbm doctor` and reconcile explicitly".to_owned(),
            )),
        Some((_, _, epoch)) => {
            let next_epoch = epoch
                .checked_add(1)
                .ok_or_else(|| DbmError::Diverged("SQLite lock epoch is exhausted".to_owned()))?;
            transaction.execute(
                "UPDATE main.schema_migration_locks \
                 SET owner_id = ?1, lock_strategy = 'sqlite_begin_immediate', \
                     acquired_at = ?2, last_observed_at = ?2, run_id = ?3, \
                     lock_epoch = ?4, state = 'observed_active' \
                 WHERE scope = 'database'",
                rusqlite::params![lock.owner_id, lock.now, lock.run_id, next_epoch],
            )?;
            Ok(next_epoch)
        }
        None => {
            transaction.execute(
                "INSERT INTO main.schema_migration_locks \
                 (scope, owner_id, lock_strategy, acquired_at, last_observed_at, \
                  run_id, lock_epoch, state) \
                 VALUES ('database', ?1, 'sqlite_begin_immediate', ?2, ?2, ?3, 1, 'observed_active')",
                rusqlite::params![lock.owner_id, lock.now, lock.run_id],
            )?;
            Ok(1)
        }
    }
}

pub fn release_lock(transaction: &Connection, owner_id: &str, now: &str) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_locks \
         SET state = 'released', run_id = NULL, last_observed_at = ?1 \
         WHERE scope = 'database' AND owner_id = ?2 AND state = 'observed_active'",
        rusqlite::params![now, owner_id],
    )?;
    if changed != 1 {
        return Err(DbmError::Lock(
            "the current process could not confirm release of the SQLite diagnostic lock"
                .to_owned(),
        ));
    }
    Ok(())
}

pub fn release_reconciled_lock(
    transaction: &Transaction<'_>,
    run_id: &str,
    now: &str,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE main.schema_migration_locks \
         SET state = 'released', run_id = NULL, last_observed_at = ?1 \
         WHERE scope = 'database' AND run_id = ?2 AND state = 'observed_active'",
        rusqlite::params![now, run_id],
    )?;
    if changed != 1 {
        let active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.schema_migration_locks \
             WHERE scope = 'database' AND state = 'observed_active')",
            [],
            |row| row.get(0),
        )?;
        if active {
            return Err(DbmError::Lock(
                "active diagnostic lock is not associated with the selected run".to_owned(),
            ));
        }
    }
    Ok(())
}

pub fn ensure_no_unresolved_runs(transaction: &Transaction<'_>) -> Result<()> {
    let unresolved: Option<(String, String)> = transaction
        .query_row(
            "SELECT run_id, state FROM main.schema_migration_runs \
             WHERE state IN ('planned', 'started', 'indeterminate') \
             ORDER BY created_at, run_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((run_id, state)) = unresolved {
        return Err(DbmError::Safety(format!(
            "run {run_id} is {state}; inspect `dbm doctor` and reconcile before new writes"
        )));
    }
    Ok(())
}

pub fn ensure_run_lock_ownership(
    transaction: &Transaction<'_>,
    run_id: &str,
    owner_id: &str,
) -> Result<()> {
    let owns_lock: bool = transaction.query_row(
        "SELECT EXISTS(\
            SELECT 1 \
            FROM main.schema_migration_runs AS run \
            INNER JOIN main.schema_migration_locks AS lock \
                ON lock.scope = 'database' AND lock.run_id = run.run_id \
            WHERE run.run_id = ?1 AND run.state = 'started' \
              AND lock.owner_id = ?2 AND lock.state = 'observed_active'\
         )",
        rusqlite::params![run_id, owner_id],
        |row| row.get(0),
    )?;
    if !owns_lock {
        return Err(DbmError::Safety(format!(
            "run {run_id} is no longer started under this process's diagnostic lock; refusing SQL execution"
        )));
    }
    Ok(())
}

pub fn upgrade_metadata_v1_to_current(
    transaction: &Transaction<'_>,
    upgraded_at: &str,
) -> Result<()> {
    let presence = metadata_presence(transaction)?;
    if presence != 3 {
        return Err(DbmError::Diverged(format!(
            "metadata upgrade requires exactly the three format-1 tables; found {presence}"
        )));
    }
    let (format_version, installation_id): (i64, String) = transaction
        .query_row(
            "SELECT metadata_format_version, installation_id \
             FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| {
            DbmError::Diverged(format!("legacy metadata identity is invalid: {error}"))
        })?;
    if format_version != LEGACY_METADATA_FORMAT_VERSION || installation_id.is_empty() {
        return Err(DbmError::Diverged(format!(
            "explicit upgrade supports metadata format 1 only; found {format_version}"
        )));
    }

    transaction.execute_batch(
        "ALTER TABLE main.schema_migration_runs RENAME TO schema_migration_runs_v1;\
         ALTER TABLE main.schema_migration_meta ADD COLUMN upgraded_at TEXT;\
         CREATE TABLE main.schema_migration_runs (\
             run_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, namespace TEXT NOT NULL,\
             migration_key TEXT NOT NULL, migration_version TEXT NOT NULL,\
             direction TEXT NOT NULL CHECK (direction IN ('up', 'down', 'reconcile')),\
             state TEXT NOT NULL CHECK (state IN ('planned', 'started', 'succeeded', 'failed', 'indeterminate', 'reconciled')),\
             attempt INTEGER NOT NULL CHECK (attempt >= 1), created_at TEXT NOT NULL,\
             started_at TEXT, ended_at TEXT,\
             duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),\
              checksum_at_execution TEXT NOT NULL, plan_hash TEXT NOT NULL,\
              error_code TEXT, error_summary_redacted TEXT,\
              reconciliation_result TEXT CHECK (reconciliation_result IS NULL OR reconciliation_result IN ('operation_applied', 'operation_not_applied')),\
              reconciliation_plan TEXT,\
              reconciliation_evidence_ref TEXT, reconciliation_reason TEXT, reconciled_by TEXT,\
             reconciled_from_state TEXT,\
             related_run_id TEXT REFERENCES schema_migration_runs(run_id),\
             actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ci', 'embedded', 'agent_proposal'))\
         );\
         CREATE INDEX main.schema_migration_runs_namespace_created_idx ON \
             schema_migration_runs(namespace, created_at, run_id);\
         CREATE TABLE main.schema_migration_steps (\
             run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
             step_index INTEGER NOT NULL CHECK (step_index >= 0), step_key TEXT NOT NULL,\
             direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
             statement_hash TEXT NOT NULL, statement_kind TEXT NOT NULL,\
              transaction_class TEXT NOT NULL CHECK (transaction_class IN ('transactional', 'non_transactional')),\
             state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
             started_at TEXT NOT NULL, acknowledged_at TEXT, error_code TEXT, evidence TEXT,\
             attempt_id TEXT NOT NULL, PRIMARY KEY (run_id, step_index)\
         );\
         CREATE TABLE main.schema_migration_locks (\
             scope TEXT PRIMARY KEY, owner_id TEXT NOT NULL, session_id_redacted TEXT,\
             lock_strategy TEXT NOT NULL, acquired_at TEXT NOT NULL, last_observed_at TEXT NOT NULL,\
             run_id TEXT,\
             lock_epoch INTEGER NOT NULL CHECK (lock_epoch >= 1),\
             state TEXT NOT NULL CHECK (state IN ('observed_active', 'released', 'lost', 'unknown'))\
         );\
         INSERT INTO main.schema_migration_runs \
             (run_id, batch_id, namespace, migration_key, migration_version, direction, state,\
              attempt, created_at, started_at, ended_at, duration_ms, checksum_at_execution,\
              plan_hash, actor_kind) \
         SELECT old.run_id, old.batch_id, old.namespace, old.migration_key, old.migration_version,\
                old.direction, 'succeeded',\
                 (SELECT COUNT(*) FROM main.schema_migration_runs_v1 AS prior \
                  WHERE prior.namespace = old.namespace AND prior.migration_key = old.migration_key \
                    AND prior.direction = old.direction \
                    AND (prior.started_at < old.started_at \
                         OR (prior.started_at = old.started_at AND prior.run_id <= old.run_id))), \
                 old.started_at, old.started_at, old.ended_at, old.duration_ms,\
                 old.checksum_at_execution, '', 'human' \
         FROM main.schema_migration_runs_v1 AS old;\
         DROP TABLE main.schema_migration_runs_v1;",
    )?;
    create_v4_import_tables(transaction)?;
    upgrade_schema_migrations_to_v4(transaction)?;
    transaction.execute(
        "UPDATE main.schema_migration_meta \
         SET metadata_format_version = ?1, upgraded_at = ?2 WHERE singleton = 1",
        rusqlite::params![PREVIOUS_METADATA_FORMAT_VERSION, upgraded_at],
    )?;
    Ok(())
}

pub fn upgrade_metadata_v2_to_current(
    transaction: &Transaction<'_>,
    upgraded_at: &str,
) -> Result<()> {
    let presence = metadata_presence(transaction)?;
    if presence != 5 {
        return Err(DbmError::Diverged(format!(
            "metadata format 2 upgrade requires exactly five metadata tables; found {presence}"
        )));
    }
    let (format_version, installation_id): (i64, String) = transaction
        .query_row(
            "SELECT metadata_format_version, installation_id \
             FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| {
            DbmError::Diverged(format!("metadata format 2 identity is invalid: {error}"))
        })?;
    if format_version != SECOND_PREVIOUS_METADATA_FORMAT_VERSION || installation_id.is_empty() {
        return Err(DbmError::Diverged(format!(
            "explicit upgrade supports metadata format 2 only; found {format_version}"
        )));
    }

    transaction.execute_batch(
        "ALTER TABLE main.schema_migration_steps RENAME TO schema_migration_steps_v2;\
         CREATE TABLE main.schema_migration_steps (\
             run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
             step_index INTEGER NOT NULL CHECK (step_index >= 0),\
             step_key TEXT NOT NULL,\
             direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
             statement_hash TEXT NOT NULL,\
             statement_kind TEXT NOT NULL,\
             transaction_class TEXT NOT NULL CHECK (transaction_class IN ('transactional', 'non_transactional')),\
             state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
             started_at TEXT NOT NULL,\
             acknowledged_at TEXT,\
             error_code TEXT,\
             evidence TEXT,\
             attempt_id TEXT NOT NULL,\
             PRIMARY KEY (run_id, step_index)\
         );\
         INSERT INTO main.schema_migration_steps \
             (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
              transaction_class, state, started_at, acknowledged_at, error_code, evidence, attempt_id) \
         SELECT run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                transaction_class, state, started_at, acknowledged_at, error_code, evidence, attempt_id \
         FROM main.schema_migration_steps_v2;\
         DROP TABLE main.schema_migration_steps_v2;",
    )?;
    create_v4_import_tables(transaction)?;
    upgrade_schema_migrations_to_v4(transaction)?;
    transaction.execute(
        "UPDATE main.schema_migration_meta \
         SET metadata_format_version = ?1, upgraded_at = ?2 WHERE singleton = 1",
        rusqlite::params![PREVIOUS_METADATA_FORMAT_VERSION, upgraded_at],
    )?;
    Ok(())
}

pub fn upgrade_metadata_v3_to_current(
    transaction: &Transaction<'_>,
    upgraded_at: &str,
) -> Result<()> {
    let presence = metadata_presence(transaction)?;
    if presence != 5 {
        return Err(DbmError::Diverged(format!(
            "metadata format 3 upgrade requires exactly five core metadata tables; found {presence}"
        )));
    }
    let (format_version, installation_id): (i64, String) = transaction
        .query_row(
            "SELECT metadata_format_version, installation_id \
             FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| {
            DbmError::Diverged(format!("metadata format 3 identity is invalid: {error}"))
        })?;
    if format_version != THIRD_PREVIOUS_METADATA_FORMAT_VERSION || installation_id.is_empty() {
        return Err(DbmError::Diverged(format!(
            "explicit upgrade supports metadata format 3 only; found {format_version}"
        )));
    }

    create_v4_import_tables(transaction)?;
    upgrade_schema_migrations_to_v4(transaction)?;
    transaction.execute(
        "UPDATE main.schema_migration_meta \
         SET metadata_format_version = ?1, upgraded_at = ?2 WHERE singleton = 1",
        rusqlite::params![PREVIOUS_METADATA_FORMAT_VERSION, upgraded_at],
    )?;
    Ok(())
}

pub fn upgrade_metadata_v4_to_current(
    transaction: &Transaction<'_>,
    upgraded_at: &str,
) -> Result<()> {
    let presence = metadata_v4_presence(transaction)?;
    if presence != 8 {
        return Err(DbmError::Diverged(format!(
            "metadata format 4 upgrade requires exactly eight intact metadata tables; found {presence}"
        )));
    }
    let (format_version, installation_id): (i64, String) = transaction
        .query_row(
            "SELECT metadata_format_version, installation_id \
             FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| {
            DbmError::Diverged(format!("metadata format 4 identity is invalid: {error}"))
        })?;
    if format_version != PREVIOUS_METADATA_FORMAT_VERSION || installation_id.is_empty() {
        return Err(DbmError::Diverged(format!(
            "explicit upgrade supports metadata format 4 only; found {format_version}"
        )));
    }

    transaction.execute_batch(
        "PRAGMA defer_foreign_keys = ON;\
         ALTER TABLE main.schema_migrations RENAME TO schema_migrations_v4;\
         CREATE TABLE main.schema_migrations (\
             namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
             migration_key TEXT NOT NULL CHECK (length(migration_key) = 64),\
             kind TEXT NOT NULL CHECK (kind IN ('versioned', 'repeatable', 'legacy')),\
             native_version TEXT CHECK (native_version IS NULL OR length(native_version) = 20),\
             repeatable_name TEXT CHECK (repeatable_name IS NULL OR length(repeatable_name) BETWEEN 1 AND 128),\
             original_version TEXT, filename TEXT, source_path TEXT, description TEXT,\
             authoring_type TEXT NOT NULL CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),\
             content_checksum TEXT, origin_tool TEXT, origin_database_identity TEXT, origin_identity TEXT,\
             legacy_import_id TEXT REFERENCES schema_migration_imports(import_id),\
             batch_id TEXT, checksum_algorithm TEXT, checksum_value TEXT,\
             execution_time_ms INTEGER CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
             verification_status TEXT CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),\
             apply_order INTEGER NOT NULL, applied_at TEXT, origin_applied_at TEXT, observed_at TEXT,\
             PRIMARY KEY (namespace, migration_key),\
             UNIQUE (namespace, kind, native_version),\
             UNIQUE (namespace, kind, repeatable_name),\
              CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND repeatable_name IS NULL AND original_version IS NULL \
                     AND legacy_import_id IS NULL AND origin_tool IS NULL) OR\
                     (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL AND original_version IS NULL \
                     AND legacy_import_id IS NULL AND origin_tool IS NULL) OR\
                     (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL \
                      AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL AND origin_database_identity IS NOT NULL \
                     AND origin_identity IS NOT NULL AND verification_status IS NOT NULL AND observed_at IS NOT NULL))\
         );\
         INSERT INTO main.schema_migrations\
             (namespace, migration_key, kind, native_version, repeatable_name, original_version, filename, source_path,\
              description, authoring_type, content_checksum, origin_tool, origin_database_identity, origin_identity,\
              legacy_import_id, batch_id, checksum_algorithm, checksum_value, execution_time_ms, verification_status,\
              apply_order, applied_at, origin_applied_at, observed_at)\
         SELECT namespace, migration_key, kind, native_version, NULL, original_version, filename, source_path,\
                description, authoring_type, content_checksum, origin_tool, origin_database_identity, origin_identity,\
                legacy_import_id, batch_id, checksum_algorithm, checksum_value, execution_time_ms, verification_status,\
                 apply_order, applied_at, origin_applied_at, observed_at \
         FROM main.schema_migrations_v4;\
         DROP TABLE main.schema_migrations_v4;\
         ALTER TABLE main.schema_migration_runs RENAME TO schema_migration_runs_v4;\
         ALTER TABLE main.schema_migration_steps RENAME TO schema_migration_steps_v4;\
         DROP INDEX main.schema_migration_runs_namespace_created_idx;\
         CREATE TABLE main.schema_migration_runs (\
             run_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, namespace TEXT NOT NULL,\
             migration_key TEXT NOT NULL, migration_version TEXT NOT NULL,\
             direction TEXT NOT NULL CHECK (direction IN ('up', 'down', 'repeat', 'reconcile')),\
             state TEXT NOT NULL CHECK (state IN ('planned', 'started', 'succeeded', 'failed', 'indeterminate', 'reconciled')),\
             attempt INTEGER NOT NULL CHECK (attempt >= 1), created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT,\
             duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),\
             checksum_at_execution TEXT NOT NULL, plan_hash TEXT NOT NULL, error_code TEXT, error_summary_redacted TEXT,\
             reconciliation_result TEXT CHECK (reconciliation_result IS NULL OR reconciliation_result IN ('operation_applied', 'operation_not_applied')),\
             reconciliation_plan TEXT, reconciliation_evidence_ref TEXT, reconciliation_reason TEXT, reconciled_by TEXT,\
             reconciled_from_state TEXT, related_run_id TEXT REFERENCES schema_migration_runs(run_id),\
             actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ci', 'embedded', 'agent_proposal'))\
         );\
         INSERT INTO main.schema_migration_runs SELECT * FROM main.schema_migration_runs_v4;\
         CREATE TABLE main.schema_migration_steps (\
             run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
             step_index INTEGER NOT NULL CHECK (step_index >= 0), step_key TEXT NOT NULL,\
             direction TEXT NOT NULL CHECK (direction IN ('up', 'down')), statement_hash TEXT NOT NULL,\
             statement_kind TEXT NOT NULL,\
             transaction_class TEXT NOT NULL CHECK (transaction_class IN ('transactional', 'non_transactional')),\
             state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
             started_at TEXT NOT NULL, acknowledged_at TEXT, error_code TEXT, evidence TEXT, attempt_id TEXT NOT NULL,\
             PRIMARY KEY (run_id, step_index)\
         );\
         INSERT INTO main.schema_migration_steps SELECT * FROM main.schema_migration_steps_v4;\
         DROP TABLE main.schema_migration_steps_v4;\
         DROP TABLE main.schema_migration_runs_v4;\
          CREATE INDEX main.schema_migration_runs_namespace_created_idx ON \
             schema_migration_runs(namespace, created_at, run_id);",
    )?;
    transaction.execute(
        "UPDATE main.schema_migration_meta SET metadata_format_version = ?1, upgraded_at = ?2 \
         WHERE singleton = 1",
        rusqlite::params![METADATA_FORMAT_VERSION, upgraded_at],
    )?;
    Ok(())
}

fn create_v4_import_tables(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE main.schema_migration_imports (\
             import_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
             origin_tool TEXT NOT NULL CHECK (origin_tool IN ('knex', 'goose', 'sqlx')),\
             source_identity TEXT NOT NULL,\
             snapshot_digest TEXT NOT NULL,\
             plan_digest TEXT NOT NULL,\
             imported_at TEXT NOT NULL,\
             event_count INTEGER NOT NULL CHECK (event_count >= 0),\
             current_migration_count INTEGER NOT NULL CHECK (current_migration_count >= 0),\
             UNIQUE (namespace, import_id)\
         );\
         CREATE TABLE main.schema_migration_import_events (\
             import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),\
             event_index INTEGER NOT NULL CHECK (event_index >= 0),\
             event_id TEXT NOT NULL,\
             original_version TEXT NOT NULL,\
             state TEXT NOT NULL CHECK (state IN ('applied', 'reverted', 'failed', 'sentinel')),\
             filename TEXT, batch_id TEXT, applied_at TEXT,\
             checksum_algorithm TEXT, checksum_value TEXT,\
             execution_time_ms INTEGER CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
             PRIMARY KEY (import_id, event_index)\
         );\
         CREATE TABLE main.schema_migration_adoptions (\
             adoption_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
             import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),\
             legacy_migration_key TEXT NOT NULL CHECK (length(legacy_migration_key) = 64),\
             native_migration_key TEXT NOT NULL CHECK (length(native_migration_key) = 64),\
             native_version TEXT NOT NULL CHECK (length(native_version) = 20),\
             native_checksum TEXT NOT NULL, evidence_ref TEXT NOT NULL,\
             approved_by TEXT NOT NULL, reason TEXT NOT NULL, adopted_at TEXT NOT NULL,\
             UNIQUE (namespace, legacy_migration_key)\
         );",
    )?;
    Ok(())
}

fn upgrade_schema_migrations_to_v4(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "ALTER TABLE main.schema_migrations RENAME TO schema_migrations_v3;\
         CREATE TABLE main.schema_migrations (\
             namespace TEXT NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),\
             migration_key TEXT NOT NULL CHECK (length(migration_key) = 64),\
             kind TEXT NOT NULL CHECK (kind IN ('versioned', 'legacy')),\
             native_version TEXT CHECK (native_version IS NULL OR length(native_version) = 20),\
             original_version TEXT, filename TEXT, source_path TEXT, description TEXT,\
             authoring_type TEXT NOT NULL CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),\
             content_checksum TEXT, origin_tool TEXT, origin_database_identity TEXT,\
             origin_identity TEXT, legacy_import_id TEXT REFERENCES schema_migration_imports(import_id),\
             batch_id TEXT, checksum_algorithm TEXT, checksum_value TEXT,\
             execution_time_ms INTEGER CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
              verification_status TEXT CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),\
              apply_order INTEGER NOT NULL, applied_at TEXT, origin_applied_at TEXT, observed_at TEXT,\
             PRIMARY KEY (namespace, migration_key),\
             UNIQUE (namespace, kind, native_version),\
             CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND original_version IS NULL \
                      AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                    (kind = 'legacy' AND native_version IS NULL AND original_version IS NOT NULL \
                      AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL \
                      AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL \
                      AND verification_status IS NOT NULL AND observed_at IS NOT NULL))\
         );\
         INSERT INTO main.schema_migrations (\
             namespace, migration_key, kind, native_version, original_version, filename,\
             source_path, description, authoring_type, content_checksum, apply_order,\
             applied_at, observed_at\
         )\
         SELECT namespace, migration_key, kind, native_version, NULL, filename, source_path, \
                description, authoring_type, content_checksum, apply_order, applied_at, applied_at \
         FROM main.schema_migrations_v3;\
         DROP TABLE main.schema_migrations_v3;",
    )?;
    Ok(())
}

pub fn ensure_metadata_path_is_not_directory(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Err(DbmError::Configuration(format!(
            "SQLite target path is a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn metadata_presence(connection: &Connection) -> Result<usize> {
    let mut present = 0;
    for table in [
        "schema_migration_meta",
        "schema_migrations",
        "schema_migration_runs",
        "schema_migration_steps",
        "schema_migration_locks",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master \
             WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            present += 1;
        }
    }
    Ok(present)
}

pub fn metadata_v4_presence(connection: &Connection) -> Result<usize> {
    let mut present = metadata_presence(connection)?;
    for table in [
        "schema_migration_imports",
        "schema_migration_import_events",
        "schema_migration_adoptions",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master \
             WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            present += 1;
        }
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        LEGACY_METADATA_FORMAT_VERSION, METADATA_FORMAT_VERSION, ensure_schema, metadata_presence,
        metadata_v4_presence, read_snapshot, read_snapshot_from_connection,
        upgrade_metadata_v1_to_current, upgrade_metadata_v2_to_current,
        upgrade_metadata_v3_to_current, upgrade_metadata_v4_to_current,
    };
    use crate::config::{Environment, ExecutionConfig, LoadedConfig, SourceDir, TargetConfig};

    fn config(path: &std::path::Path) -> LoadedConfig {
        LoadedConfig {
            config: crate::config::Config {
                version: 1,
                namespace: "default".to_owned(),
                source: vec!["migrations".into()],
                target: TargetConfig {
                    driver: "sqlite".to_owned(),
                    database: Some("db.sqlite".into()),
                    url_env: None,
                    tls_ca_cert_env: None,
                    allow_insecure_transport: false,
                    environment: Environment::Development,
                },
                execution: ExecutionConfig::default(),
                ai: crate::config::AiConfig::default(),
            },
            source_dirs: vec![SourceDir {
                path: path.join("migrations"),
                logical_path: "migrations".to_owned(),
            }],
            database_path: Some(path.join("db.sqlite")),
        }
    }

    #[test]
    fn readonly_snapshot_does_not_create_a_missing_database_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = config(directory.path());

        let snapshot = read_snapshot(&config).expect("empty snapshot");

        assert!(!snapshot.initialized);
        assert!(
            !config
                .sqlite_database_path()
                .expect("SQLite database path")
                .exists()
        );
    }

    #[test]
    fn metadata_bootstrap_is_transactional_and_versioned() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("in-memory database");
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin transaction");
        ensure_schema(&transaction, "2026-10-23T00:00:00.000000Z").expect("bootstrap");
        transaction.commit().expect("commit bootstrap");

        let format_version: i64 = connection
            .query_row(
                "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read format version");
        assert_eq!(format_version, METADATA_FORMAT_VERSION);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row
                    .get::<_, i64>(0))
                .expect("read applied count"),
            0
        );

        let directory = tempfile::tempdir().expect("temporary directory");
        let config = config(directory.path());
        fs::write(
            config.sqlite_database_path().expect("SQLite database path"),
            b"not a sqlite db",
        )
        .expect("overwrite target");
        assert!(read_snapshot(&config).is_err());
    }

    #[test]
    fn metadata_v1_upgrade_is_explicit_atomic_and_preserves_run_history() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE schema_migration_meta (\
                     singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\
                     metadata_format_version INTEGER NOT NULL,\
                     installation_id TEXT NOT NULL,\
                     initialized_at TEXT NOT NULL\
                 );\
                 CREATE TABLE schema_migrations (\
                     namespace TEXT NOT NULL, migration_key TEXT NOT NULL,\
                     kind TEXT NOT NULL, native_version TEXT NOT NULL, filename TEXT NOT NULL,\
                     source_path TEXT NOT NULL, description TEXT NOT NULL,\
                     authoring_type TEXT NOT NULL, content_checksum TEXT NOT NULL,\
                     apply_order INTEGER NOT NULL, applied_at TEXT NOT NULL,\
                     PRIMARY KEY (namespace, migration_key),\
                     UNIQUE (namespace, kind, native_version)\
                 );\
                 CREATE TABLE schema_migration_runs (\
                     run_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, namespace TEXT NOT NULL,\
                     migration_key TEXT NOT NULL, migration_version TEXT NOT NULL,\
                     direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
                     state TEXT NOT NULL CHECK (state = 'succeeded'),\
                     started_at TEXT NOT NULL, ended_at TEXT NOT NULL,\
                     duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),\
                     checksum_at_execution TEXT NOT NULL\
                 );\
                 INSERT INTO schema_migration_meta VALUES\
                     (1, 1, 'installation-1', '2026-10-23T00:00:00.000000Z');\
                 INSERT INTO schema_migration_runs VALUES\
                     ('run-1', 'batch-1', 'default', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',\
                      '20261021162643981234', 'up', 'succeeded',\
                      '2026-10-23T00:00:01.000000Z', '2026-10-23T00:00:02.000000Z', 1000, 'sha256:raw:v1:checksum');",
            )
            .expect("create legacy metadata");

        assert_eq!(
            metadata_presence(&connection).expect("count legacy tables"),
            3
        );
        assert!(read_snapshot_from_connection(&connection, "default").is_err());

        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin upgrade transaction");
        upgrade_metadata_v1_to_current(&transaction, "2026-10-24T00:00:00.000000Z")
            .expect("upgrade legacy metadata");
        upgrade_metadata_v4_to_current(&transaction, "2026-10-24T00:00:00.000000Z")
            .expect("upgrade metadata format 4 to 5");
        transaction.commit().expect("commit metadata upgrade");

        assert_eq!(
            metadata_presence(&connection).expect("count current tables"),
            5
        );
        assert_eq!(
            metadata_v4_presence(&connection).expect("count format 4 tables"),
            8
        );
        let version: i64 = connection
            .query_row(
                "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read upgraded version");
        assert_eq!(version, METADATA_FORMAT_VERSION);
        assert_ne!(version, LEGACY_METADATA_FORMAT_VERSION);
        let repair_plan_column_exists: bool = connection
            .query_row(
                "SELECT EXISTS(\
                    SELECT 1 FROM pragma_table_info('schema_migration_runs') \
                    WHERE name = 'reconciliation_plan'\
                 )",
                [],
                |row| row.get(0),
            )
            .expect("check upgraded repair-plan column");
        assert!(repair_plan_column_exists);
        let snapshot =
            read_snapshot_from_connection(&connection, "default").expect("read preserved history");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].run_id, "run-1");
        assert_eq!(snapshot.runs[0].state, "succeeded");
        assert_eq!(snapshot.runs[0].attempt, 1);
    }

    #[test]
    fn metadata_v2_upgrade_is_explicit_atomic_and_preserves_journal_rows() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("in-memory database");
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin current metadata bootstrap");
        ensure_schema(&transaction, "2026-10-24T00:00:00.000000Z").expect("bootstrap format 3");
        transaction
            .execute_batch(
                "INSERT INTO schema_migration_runs \
                     (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                      state, attempt, created_at, started_at, ended_at, duration_ms, \
                      checksum_at_execution, plan_hash, actor_kind) \
                 VALUES ('v2-run', 'v2-batch', 'default', \
                         'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', \
                         '20261021162643981234', 'up', 'succeeded', 1, \
                         '2026-10-24T00:00:01.000000Z', '2026-10-24T00:00:01.000000Z', \
                         '2026-10-24T00:00:02.000000Z', 1000, 'sha256:checksum', \
                          'sha256:plan', 'human');\
                  DROP TABLE schema_migration_adoptions;\
                  DROP TABLE schema_migration_import_events;\
                  DROP TABLE schema_migration_imports;\
                  ALTER TABLE schema_migrations RENAME TO schema_migrations_v3;\
                  CREATE TABLE schema_migrations (\
                      namespace TEXT NOT NULL, migration_key TEXT NOT NULL,\
                      kind TEXT NOT NULL CHECK (kind = 'versioned'),\
                      native_version TEXT NOT NULL, filename TEXT NOT NULL,\
                      source_path TEXT NOT NULL, description TEXT NOT NULL,\
                      authoring_type TEXT NOT NULL CHECK (authoring_type = 'sql'),\
                      content_checksum TEXT NOT NULL, apply_order INTEGER NOT NULL,\
                      applied_at TEXT NOT NULL, PRIMARY KEY (namespace, migration_key),\
                      UNIQUE (namespace, kind, native_version)\
                  );\
                  INSERT INTO schema_migrations (namespace, migration_key, kind, native_version,\
                      filename, source_path, description, authoring_type, content_checksum,\
                      apply_order, applied_at) \
                  SELECT namespace, migration_key, kind, native_version, filename, source_path,\
                      description, authoring_type, content_checksum, apply_order, applied_at \
                  FROM schema_migrations_v3;\
                  DROP TABLE schema_migrations_v3;\
                 ALTER TABLE schema_migration_steps RENAME TO schema_migration_steps_v3;\
                 CREATE TABLE schema_migration_steps (\
                     run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
                     step_index INTEGER NOT NULL CHECK (step_index >= 0),\
                     step_key TEXT NOT NULL,\
                     direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
                     statement_hash TEXT NOT NULL,\
                     statement_kind TEXT NOT NULL,\
                     transaction_class TEXT NOT NULL CHECK (transaction_class = 'transactional'),\
                     state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
                     started_at TEXT NOT NULL, acknowledged_at TEXT, error_code TEXT, evidence TEXT,\
                     attempt_id TEXT NOT NULL, PRIMARY KEY (run_id, step_index)\
                 );\
                 INSERT INTO schema_migration_steps \
                     (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                      transaction_class, state, started_at, acknowledged_at, attempt_id) \
                 VALUES ('v2-run', 0, 'v2-step', 'up', 'sha256:step', 'CREATE', \
                         'transactional', 'acknowledged', '2026-10-24T00:00:01.500000Z', \
                         '2026-10-24T00:00:02.000000Z', 'v2-run');\
                 DROP TABLE schema_migration_steps_v3;\
                 UPDATE schema_migration_meta SET metadata_format_version = 2 WHERE singleton = 1;",
            )
            .expect("prepare version 2 state and journal");
        transaction.commit().expect("commit format 2 fixture");

        let old_snapshot = read_snapshot_from_connection(&connection, "default")
            .expect("format 2 remains readable for inspection");
        assert_eq!(old_snapshot.metadata_format_version, Some(2));
        assert_eq!(old_snapshot.runs[0].run_id, "v2-run");
        assert_eq!(old_snapshot.steps[0].step_key, "v2-step");
        let refused = connection
            .unchecked_transaction()
            .expect("begin check transaction");
        assert!(ensure_schema(&refused, "2026-10-24T00:00:03.000000Z").is_err());
        refused
            .rollback()
            .expect("rollback read-only refusal check");

        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin explicit format 2 upgrade");
        upgrade_metadata_v2_to_current(&transaction, "2026-10-24T00:00:04.000000Z")
            .expect("upgrade format 2 metadata");
        upgrade_metadata_v4_to_current(&transaction, "2026-10-24T00:00:04.000000Z")
            .expect("upgrade metadata format 4 to 5");
        transaction.commit().expect("commit format 2 upgrade");

        let upgraded =
            read_snapshot_from_connection(&connection, "default").expect("read upgraded journal");
        assert_eq!(
            upgraded.metadata_format_version,
            Some(METADATA_FORMAT_VERSION)
        );
        assert_eq!(upgraded.runs.len(), 1);
        assert_eq!(upgraded.runs[0].run_id, "v2-run");
        assert_eq!(upgraded.steps.len(), 1);
        assert_eq!(upgraded.steps[0].step_key, "v2-step");
        assert_eq!(upgraded.steps[0].transaction_class, "transactional");
    }

    #[test]
    fn metadata_v3_upgrade_preserves_native_state_and_adds_relational_import_tables() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("in-memory database");
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin v4 fixture bootstrap");
        ensure_schema(&transaction, "2026-10-24T00:00:00.000000Z").expect("bootstrap v4");
        transaction.commit().expect("commit v4 fixture bootstrap");
        connection
            .execute(
                "INSERT INTO schema_migrations \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES ('default', ?1, 'versioned', '20261021162643981234', \
                         '20261021162643981234_create_users.sql', \
                         'migrations/versioned/20261021162643981234_create_users.sql', \
                         'create_users', 'sql', 'sha256:legacy-row', 1, \
                         '2026-10-23T00:00:01.000000Z')",
                ["a".repeat(64)],
            )
            .expect("insert native row before downgrade fixture conversion");
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;\
                 DROP TABLE schema_migration_adoptions;\
                 DROP TABLE schema_migration_import_events;\
                 DROP TABLE schema_migration_imports;\
                 ALTER TABLE schema_migrations RENAME TO schema_migrations_v4;\
                 CREATE TABLE schema_migrations (\
                     namespace TEXT NOT NULL, migration_key TEXT NOT NULL,\
                     kind TEXT NOT NULL CHECK (kind = 'versioned'),\
                     native_version TEXT NOT NULL, filename TEXT NOT NULL,\
                     source_path TEXT NOT NULL, description TEXT NOT NULL,\
                     authoring_type TEXT NOT NULL CHECK (authoring_type = 'sql'),\
                     content_checksum TEXT NOT NULL, apply_order INTEGER NOT NULL,\
                     applied_at TEXT NOT NULL, PRIMARY KEY (namespace, migration_key),\
                     UNIQUE (namespace, kind, native_version)\
                 );\
                  INSERT INTO schema_migrations (namespace, migration_key, kind, native_version,\
                      filename, source_path, description, authoring_type, content_checksum,\
                      apply_order, applied_at) \
                  SELECT namespace, migration_key, kind, native_version, filename, source_path, \
                      description, authoring_type, content_checksum, apply_order, applied_at \
                 FROM schema_migrations_v4;\
                 DROP TABLE schema_migrations_v4;\
                 UPDATE schema_migration_meta SET metadata_format_version = 3 WHERE singleton = 1;\
                 PRAGMA foreign_keys = ON;",
            )
            .expect("convert fixture to metadata v3 shape");

        let v3 = read_snapshot_from_connection(&connection, "default").expect("read v3 state");
        assert_eq!(v3.metadata_format_version, Some(3));
        assert_eq!(v3.applied.len(), 1);
        assert_eq!(
            v3.applied[0].filename,
            "20261021162643981234_create_users.sql"
        );
        let refused = connection
            .unchecked_transaction()
            .expect("begin mutation refusal check");
        assert!(ensure_schema(&refused, "2026-10-24T00:00:01.000000Z").is_err());
        refused.rollback().expect("rollback refusal check");

        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin explicit v3 upgrade");
        upgrade_metadata_v3_to_current(&transaction, "2026-10-24T00:00:02.000000Z")
            .expect("upgrade v3 to v4");
        transaction.commit().expect("commit v3 to v4 upgrade");

        let upgraded_v4 =
            read_snapshot_from_connection(&connection, "default").expect("read v4 state");
        assert_eq!(upgraded_v4.metadata_format_version, Some(4));
        assert_eq!(upgraded_v4.applied.len(), 1);
        assert_eq!(upgraded_v4.applied[0].migration_key, "a".repeat(64));

        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("begin explicit v4 upgrade");
        upgrade_metadata_v4_to_current(&transaction, "2026-10-24T00:00:03.000000Z")
            .expect("upgrade v4 to v5");
        transaction.commit().expect("commit v4 to v5 upgrade");

        assert_eq!(
            metadata_v4_presence(&connection).expect("count v4 tables"),
            8
        );
        let upgraded =
            read_snapshot_from_connection(&connection, "default").expect("read v5 state");
        assert_eq!(
            upgraded.metadata_format_version,
            Some(METADATA_FORMAT_VERSION)
        );
        assert_eq!(upgraded.applied.len(), 1);
        assert_eq!(upgraded.applied[0].migration_key, "a".repeat(64));
        assert_eq!(upgraded.applied[0].apply_order, 1);
        assert!(upgraded.legacy_applied.is_empty());
        assert!(upgraded.legacy_imports.is_empty());
    }
}
