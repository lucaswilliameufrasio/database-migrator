use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use mysql_async::prelude::{FromRow, FromValue, Queryable};
use mysql_async::{Conn, Opts, OptsBuilder, Params, Row, SslOpts, Value};
use sha2::{Digest, Sha256};

use crate::config::{Environment, LoadedConfig};
use crate::database::{BoxFuture, DatabaseAdapter, DatabaseInfo, DatabaseSession, SessionIntent};
use crate::error::{DbmError, Result};
use crate::identity::legacy_migration_key;
use crate::legacy::{
    GooseRow, KnexLock, KnexRow, LegacyAdoptionWrite, LegacyImportSnapshot, LegacyImportWrite,
    LegacyTool, SqlxRow, inspect_goose, inspect_knex, inspect_sqlx, parse_legacy_bool,
};
use crate::migration::{Migration, MigrationKind};
use crate::storage::{
    AppliedMigration, FinishedRunData, LegacyAppliedMigration, LegacyImportEventRecord,
    LegacyImportRecord, LockData, LockRecord, MetadataSnapshot, PlannedRunData, ReconciliationData,
    RunRecord, StepData, StepRecord,
};

const METADATA_TABLE_NAMES: [&str; 8] = [
    "schema_migration_meta",
    "schema_migration_imports",
    "schema_migration_import_events",
    "schema_migration_adoptions",
    "schema_migrations",
    "schema_migration_runs",
    "schema_migration_steps",
    "schema_migration_locks",
];

const MYSQL_METADATA_TABLE_DDL: [&str; 8] = [
    "CREATE TABLE schema_migration_meta (\
        singleton TINYINT UNSIGNED NOT NULL PRIMARY KEY CHECK (singleton = 1),\
        metadata_format_version INT NOT NULL,\
        installation_id CHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        initialized_at DATETIME(6) NOT NULL,\
        upgraded_at DATETIME(6) NULL,\
        CHECK (metadata_format_version = 5)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_imports (\
        import_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,\
        namespace VARCHAR(128) NOT NULL,\
        origin_tool VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        source_identity VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        snapshot_digest VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        plan_digest VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        imported_at DATETIME(6) NOT NULL,\
        event_count BIGINT NOT NULL,\
        current_migration_count BIGINT NOT NULL,\
        UNIQUE KEY schema_migration_import_namespace_id_uq (namespace, import_id),\
        CHECK (CHAR_LENGTH(namespace) BETWEEN 1 AND 128),\
        CHECK (origin_tool IN ('knex', 'goose', 'sqlx')),\
        CHECK (event_count >= 0),\
        CHECK (current_migration_count >= 0)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_import_events (\
        import_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        event_index BIGINT NOT NULL,\
        event_id LONGTEXT NOT NULL,\
        original_version LONGTEXT NOT NULL,\
        state VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        filename LONGTEXT NULL,\
        batch_id LONGTEXT NULL,\
        applied_at LONGTEXT NULL,\
        checksum_algorithm VARCHAR(32) NULL,\
        checksum_value LONGTEXT NULL,\
        execution_time_ms BIGINT NULL,\
        PRIMARY KEY (import_id, event_index),\
        CHECK (event_index >= 0),\
        CHECK (state IN ('applied', 'reverted', 'failed', 'sentinel')),\
        CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
        CONSTRAINT schema_migration_import_events_import_fk FOREIGN KEY (import_id)\
            REFERENCES schema_migration_imports (import_id)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_adoptions (\
        adoption_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,\
        namespace VARCHAR(128) NOT NULL,\
        import_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        legacy_migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        native_migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        native_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        native_checksum VARCHAR(128) NOT NULL,\
        evidence_ref VARCHAR(128) NOT NULL,\
        approved_by VARCHAR(128) NOT NULL,\
        reason TEXT NOT NULL,\
        adopted_at DATETIME(6) NOT NULL,\
        UNIQUE KEY schema_migration_adoptions_namespace_legacy_uq (namespace, legacy_migration_key),\
        CHECK (CHAR_LENGTH(namespace) BETWEEN 1 AND 128),\
        CHECK (CHAR_LENGTH(legacy_migration_key) = 64),\
        CHECK (CHAR_LENGTH(native_migration_key) = 64),\
        CHECK (CHAR_LENGTH(native_version) = 20),\
        CONSTRAINT schema_migration_adoptions_import_fk FOREIGN KEY (import_id)\
            REFERENCES schema_migration_imports (import_id)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migrations (\
        namespace VARCHAR(128) NOT NULL,\
        migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        kind VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        native_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        repeatable_name VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        original_version LONGTEXT NULL,\
        filename LONGTEXT NULL,\
        source_path LONGTEXT NULL,\
        description LONGTEXT NULL,\
        authoring_type VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        content_checksum VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        origin_tool VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        origin_database_identity VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        origin_identity LONGTEXT NULL,\
        legacy_import_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        batch_id LONGTEXT NULL,\
        checksum_algorithm VARCHAR(32) NULL,\
        checksum_value LONGTEXT NULL,\
        execution_time_ms BIGINT NULL,\
        verification_status VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        apply_order BIGINT NOT NULL,\
        applied_at DATETIME(6) NULL,\
        origin_applied_at LONGTEXT NULL,\
        observed_at DATETIME(6) NULL,\
        PRIMARY KEY (namespace, migration_key),\
        UNIQUE KEY schema_migrations_namespace_version_uq (namespace, kind, native_version),\
        UNIQUE KEY schema_migrations_namespace_repeatable_uq (namespace, kind, repeatable_name),\
        CHECK (CHAR_LENGTH(namespace) BETWEEN 1 AND 128),\
        CHECK (CHAR_LENGTH(migration_key) = 64),\
        CHECK (kind IN ('versioned', 'repeatable', 'legacy')),\
        CHECK (native_version IS NULL OR CHAR_LENGTH(native_version) = 20),\
        CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),\
        CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),\
        CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),\
        CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND repeatable_name IS NULL AND original_version IS NULL \
                AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
               (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL AND original_version IS NULL \
                AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
               (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL \
                AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL \
                AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL \
                AND verification_status IS NOT NULL AND observed_at IS NOT NULL)),\
        CONSTRAINT schema_migrations_legacy_import_fk FOREIGN KEY (legacy_import_id)\
            REFERENCES schema_migration_imports (import_id)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_runs (\
        run_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,\
        batch_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        namespace VARCHAR(128) NOT NULL,\
        migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        migration_version VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        direction VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        state VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        attempt BIGINT NOT NULL,\
        created_at DATETIME(6) NOT NULL,\
        started_at DATETIME(6) NULL,\
        ended_at DATETIME(6) NULL,\
        duration_ms BIGINT NULL,\
        checksum_at_execution VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        plan_hash VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        error_code VARCHAR(128) NULL,\
        error_summary_redacted TEXT NULL,\
        reconciliation_result VARCHAR(32) NULL,\
        reconciliation_plan LONGTEXT NULL,\
        reconciliation_evidence_ref VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        reconciliation_reason TEXT NULL,\
        reconciled_by VARCHAR(128) NULL,\
        reconciled_from_state VARCHAR(24) NULL,\
        related_run_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        actor_kind VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        KEY schema_migration_runs_namespace_created_idx (namespace, created_at, run_id),\
        CHECK (direction IN ('up', 'down', 'repeat', 'reconcile')),\
        CHECK (state IN ('planned', 'started', 'succeeded', 'failed', 'indeterminate', 'reconciled')),\
        CHECK (attempt >= 1),\
        CHECK (duration_ms IS NULL OR duration_ms >= 0),\
        CHECK (reconciliation_result IS NULL OR reconciliation_result IN ('operation_applied', 'operation_not_applied')),\
        CHECK (actor_kind IN ('human', 'ci', 'embedded', 'agent_proposal')),\
        CONSTRAINT schema_migration_runs_related_fk FOREIGN KEY (related_run_id)\
            REFERENCES schema_migration_runs (run_id)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_steps (\
        run_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        step_index BIGINT NOT NULL,\
        step_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        direction VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        statement_hash VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        statement_kind VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        transaction_class VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        state VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        started_at DATETIME(6) NOT NULL,\
        acknowledged_at DATETIME(6) NULL,\
        error_code VARCHAR(128) NULL,\
        evidence TEXT NULL,\
        attempt_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        PRIMARY KEY (run_id, step_index),\
        CHECK (step_index >= 0),\
        CHECK (direction IN ('up', 'down')),\
        CHECK (transaction_class IN ('transactional', 'non_transactional')),\
        CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
        CONSTRAINT schema_migration_steps_run_fk FOREIGN KEY (run_id)\
            REFERENCES schema_migration_runs (run_id)\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
    "CREATE TABLE schema_migration_locks (\
        scope VARCHAR(128) NOT NULL PRIMARY KEY,\
        owner_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        session_id_redacted VARCHAR(128) NULL,\
        lock_strategy VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        acquired_at DATETIME(6) NOT NULL,\
        last_observed_at DATETIME(6) NOT NULL,\
        run_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,\
        lock_epoch BIGINT NOT NULL,\
        state VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
        CHECK (lock_epoch >= 1),\
        CHECK (state IN ('observed_active', 'released', 'lost', 'unknown'))\
    ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
];

macro_rules! params {
    ($($value:expr),* $(,)?) => {
        Params::Positional(vec![$(Value::from(&$value)),*])
    };
}

pub(crate) struct MySqlAdapter;

impl DatabaseAdapter for MySqlAdapter {
    fn connect<'a>(
        &'a self,
        config: &'a LoadedConfig,
        intent: SessionIntent,
    ) -> BoxFuture<'a, Result<Box<dyn DatabaseSession>>> {
        Box::pin(async move {
            let variable_name =
                config.config.target.url_env.as_deref().ok_or_else(|| {
                    DbmError::Configuration("MySQL target has no url_env".to_owned())
                })?;
            let connection_string = std::env::var(variable_name).map_err(|_| {
                DbmError::Configuration(format!(
                    "MySQL connection variable {variable_name} is not set or not valid Unicode"
                ))
            })?;
            let opts = prepare_connection_options(&connection_string, config).await?;
            let session = MySqlSession::connect_from_options(opts, intent).await?;
            Ok(Box::new(session) as Box<dyn DatabaseSession>)
        })
    }
}

fn mysql_target_identity(options: &Opts) -> String {
    let endpoint = match options.socket() {
        Some(socket) => format!("unix:{socket}"),
        None => format!("tcp:{}:{}", options.ip_or_hostname(), options.tcp_port()),
    };
    let database = options.db_name().unwrap_or("<default>");
    format!("mysql://{endpoint}/{database}")
}

struct MySqlSession {
    connection: Option<Conn>,
    info: DatabaseInfo,
    intent: SessionIntent,
    transaction_open: bool,
    held_lock: Option<String>,
    closed: bool,
}

impl MySqlSession {
    async fn connect_from_options(opts: Opts, intent: SessionIntent) -> Result<Self> {
        let target_identity = mysql_target_identity(&opts);
        let mut connection = Conn::new(opts).await.map_err(|_| {
            driver_error("connection failed; credentials and connection details are redacted")
        })?;
        let session_id = connection.id().to_string();
        let server_row = connection
            .query_first::<(String, String, Option<String>, u64), _>(
                "SELECT VERSION(), @@version_comment, DATABASE(), CONNECTION_ID()",
            )
            .await
            .map_err(|_| driver_error("could not identify the connected database server"))?
            .ok_or_else(|| driver_error("database server returned no identity row"))?;
        let (version, comment, database, server_connection_id) = server_row;
        if session_id != server_connection_id.to_string() {
            let _ = connection.disconnect().await;
            return Err(DbmError::ReconciliationRequired(
                "MySQL connection identity changed during session initialization".to_owned(),
            ));
        }
        let database = database.filter(|name| !name.is_empty()).ok_or_else(|| {
            DbmError::Configuration(
                "MySQL connection URL must select a database explicitly".to_owned(),
            )
        })?;
        let vendor = detect_vendor(&version, &comment)?;
        ensure_supported_server_version(vendor, &version)?;
        let server_version = format!("{} {version}", vendor.label());
        Ok(Self {
            connection: Some(connection),
            info: DatabaseInfo {
                dialect: "mysql",
                server_version,
                database: Some(database),
                schema: None,
                target_identity: Some(target_identity),
                session_id: Some(session_id),
            },
            intent,
            transaction_open: false,
            held_lock: None,
            closed: false,
        })
    }

    fn connection(&mut self) -> Result<&mut Conn> {
        if self.closed {
            return Err(driver_error("session is closed"));
        }
        self.connection
            .as_mut()
            .ok_or_else(|| driver_error("session connection is unavailable"))
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.intent != SessionIntent::ReadWrite {
            return Err(DbmError::Safety(
                "a read-only MySQL session cannot execute writes".to_owned(),
            ));
        }
        if self.closed {
            return Err(driver_error("session is closed"));
        }
        Ok(())
    }

    fn ensure_metadata_transaction(&self) -> Result<()> {
        self.ensure_writable()?;
        if !self.transaction_open {
            return Err(DbmError::Safety(
                "MySQL metadata writes require an open managed transaction".to_owned(),
            ));
        }
        Ok(())
    }

    async fn verify_session_inner(&mut self) -> Result<()> {
        if self.closed {
            return Err(driver_error("session is closed"));
        }
        let expected_id = self
            .info
            .session_id
            .as_deref()
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or_else(|| driver_error("MySQL session has no valid connection identity"))?;
        let connection = self.connection()?;
        if connection.is_disconnected() || connection.id() != expected_id {
            return Err(DbmError::ReconciliationRequired(
                "MySQL pinned connection was lost; session-bound lock ownership is unknown"
                    .to_owned(),
            ));
        }
        let current_id = connection
            .query_first::<u64, _>("SELECT CONNECTION_ID()")
            .await
            .map_err(|_| {
                DbmError::ReconciliationRequired(
                    "MySQL pinned session could not be verified".to_owned(),
                )
            })?
            .and_then(|id| u32::try_from(id).ok());
        if current_id != Some(expected_id) {
            return Err(DbmError::ReconciliationRequired(
                "MySQL connection identity changed; session-bound lock ownership is unknown"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn execute_drop(&mut self, sql: &str, params: Params, operation: &str) -> Result<u64> {
        self.verify_session_inner().await?;
        let state_may_be_uncertain = self.transaction_open || self.held_lock.is_some();
        let connection = self.connection()?;
        connection
            .exec_drop(sql, params)
            .await
            .map_err(|error| query_error(error, operation, state_may_be_uncertain))?;
        let changed = connection.affected_rows();
        self.verify_session_inner().await?;
        Ok(changed)
    }

    async fn query_rows<T>(&mut self, sql: &str, params: Params, operation: &str) -> Result<Vec<T>>
    where
        T: FromRow + Send + 'static,
    {
        self.verify_session_inner().await?;
        let rows = self
            .connection()?
            .exec(sql, params)
            .await
            .map_err(|_| driver_error(operation))?;
        self.verify_session_inner().await?;
        Ok(rows)
    }

    async fn query_first<T>(
        &mut self,
        sql: &str,
        params: Params,
        operation: &str,
    ) -> Result<Option<T>>
    where
        T: FromRow + Send + 'static,
    {
        self.verify_session_inner().await?;
        let row = self
            .connection()?
            .exec_first(sql, params)
            .await
            .map_err(|_| driver_error(operation))?;
        self.verify_session_inner().await?;
        Ok(row)
    }

    async fn table_counts(&mut self) -> Result<(u64, u64)> {
        let query = format!(
            "SELECT COUNT(*), COALESCE(SUM(table_type = 'BASE TABLE'), 0) \
             FROM information_schema.tables WHERE table_schema = DATABASE() \
             AND table_name IN ({})",
            METADATA_TABLE_NAMES
                .iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.query_first::<(u64, u64)>(
            &query,
            Params::Empty,
            "could not inspect MySQL metadata tables",
        )
        .await?
        .ok_or_else(|| driver_error("MySQL returned no metadata inspection result"))
    }

    async fn read_snapshot_contents(&mut self, namespace: &str) -> Result<MetadataSnapshot> {
        let (relation_count, table_count) = self.table_counts().await?;
        if relation_count == 0 {
            return Ok(MetadataSnapshot::default());
        }
        if !matches!((relation_count, table_count), (5, 5) | (8, 8)) {
            return Err(DbmError::Diverged(format!(
                "MySQL migration metadata has {table_count} base tables among {relation_count} reserved objects (expected 5 or 8 tables); refusing automatic repair"
            )));
        }
        let metadata = self
            .query_first::<(i64, String)>(
                "SELECT metadata_format_version, installation_id \
                 FROM schema_migration_meta WHERE singleton = 1",
                Params::Empty,
                "could not read MySQL metadata identity",
            )
            .await?
            .ok_or_else(|| {
                DbmError::Diverged(
                    "MySQL metadata tables exist without a metadata identity row".to_owned(),
                )
            })?;
        let (format_version, installation_id) = metadata;
        if !matches!(
            format_version,
            crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION
                | crate::storage::PREVIOUS_METADATA_FORMAT_VERSION
                | crate::storage::METADATA_FORMAT_VERSION
        ) || installation_id.is_empty()
        {
            return Err(DbmError::Diverged(format!(
                "MySQL metadata format {format_version} is unsupported or has an invalid installation id"
            )));
        }

        let applied_query = if format_version >= crate::storage::METADATA_FORMAT_VERSION {
            "SELECT migration_key, kind, COALESCE(repeatable_name, native_version), \
                    filename, source_path, description, content_checksum, apply_order, \
                    DATE_FORMAT(applied_at, '%Y-%m-%dT%H:%i:%s.%fZ') \
             FROM schema_migrations WHERE namespace = ? AND kind IN ('versioned', 'repeatable') \
             ORDER BY apply_order, kind, native_version, repeatable_name"
        } else {
            "SELECT migration_key, 'versioned', native_version, filename, source_path, description, \
                    content_checksum, apply_order, \
                    DATE_FORMAT(applied_at, '%Y-%m-%dT%H:%i:%s.%fZ') \
             FROM schema_migrations WHERE namespace = ? AND kind = 'versioned' \
             ORDER BY apply_order, native_version"
        };
        let applied_rows: Vec<Row> = self
            .query_rows(
                applied_query,
                params![namespace],
                "could not read MySQL applied migrations",
            )
            .await?;
        let mut applied = Vec::with_capacity(applied_rows.len());
        for row in &applied_rows {
            applied.push(AppliedMigration {
                migration_key: row_value(row, 0, "migration key")?,
                kind: row_value(row, 1, "migration kind")?,
                version: row_value(row, 2, "migration version")?,
                filename: row_value(row, 3, "migration filename")?,
                source_path: row_value(row, 4, "migration source path")?,
                description: row_value(row, 5, "migration description")?,
                checksum: row_value(row, 6, "migration checksum")?,
                apply_order: row_value(row, 7, "migration apply order")?,
                applied_at: row_value(row, 8, "migration timestamp")?,
            });
        }

        let (legacy_applied, legacy_imports, legacy_import_events) = if format_version
            >= crate::storage::PREVIOUS_METADATA_FORMAT_VERSION
        {
            let legacy_rows: Vec<Row> = self
                .query_rows(
                    "SELECT migration_key, legacy_import_id, origin_tool, \
                                origin_database_identity, origin_identity, original_version, \
                                filename, description, batch_id, origin_applied_at, \
                                checksum_algorithm, checksum_value, execution_time_ms, \
                                verification_status, apply_order \
                         FROM schema_migrations WHERE namespace = ? AND kind = 'legacy' \
                         ORDER BY apply_order, migration_key",
                    params![namespace],
                    "could not read MySQL imported migrations",
                )
                .await?;
            let mut legacy_applied = Vec::with_capacity(legacy_rows.len());
            for row in &legacy_rows {
                legacy_applied.push(LegacyAppliedMigration {
                    migration_key: row_value(row, 0, "legacy migration key")?,
                    import_id: row_value(row, 1, "legacy import id")?,
                    origin_tool: row_value(row, 2, "legacy origin tool")?,
                    source_identity: row_value(row, 3, "legacy source identity")?,
                    origin_identity: row_value(row, 4, "legacy origin identity")?,
                    original_version: row_value(row, 5, "legacy original version")?,
                    filename: row_value(row, 6, "legacy filename")?,
                    description: row_value(row, 7, "legacy description")?,
                    batch_id: row_value(row, 8, "legacy batch id")?,
                    applied_at: row_value(row, 9, "legacy applied timestamp")?,
                    checksum_algorithm: row_value(row, 10, "legacy checksum algorithm")?,
                    checksum_value: row_value(row, 11, "legacy checksum value")?,
                    execution_time_ms: row_value(row, 12, "legacy execution time")?,
                    verification_status: row_value(row, 13, "legacy verification status")?,
                    apply_order: row_value(row, 14, "legacy apply order")?,
                });
            }

            let import_rows: Vec<Row> = self
                .query_rows(
                    "SELECT import_id, namespace, origin_tool, source_identity, \
                                snapshot_digest, plan_digest, \
                                DATE_FORMAT(imported_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                                event_count, current_migration_count \
                         FROM schema_migration_imports WHERE namespace = ? \
                         ORDER BY imported_at, import_id",
                    params![namespace],
                    "could not read MySQL legacy import records",
                )
                .await?;
            let mut legacy_imports = Vec::with_capacity(import_rows.len());
            for row in &import_rows {
                legacy_imports.push(LegacyImportRecord {
                    import_id: row_value(row, 0, "legacy import id")?,
                    namespace: row_value(row, 1, "legacy import namespace")?,
                    origin_tool: row_value(row, 2, "legacy origin tool")?,
                    source_identity: row_value(row, 3, "legacy source identity")?,
                    snapshot_digest: row_value(row, 4, "legacy snapshot digest")?,
                    plan_digest: row_value(row, 5, "legacy plan digest")?,
                    imported_at: row_value(row, 6, "legacy imported timestamp")?,
                    event_count: row_value(row, 7, "legacy event count")?,
                    current_migration_count: row_value(row, 8, "legacy current migration count")?,
                });
            }

            let event_rows: Vec<Row> = self
                .query_rows(
                    "SELECT events.import_id, events.event_index, events.event_id, \
                                events.original_version, events.state, events.filename, \
                                events.batch_id, events.applied_at, events.checksum_algorithm, \
                                events.checksum_value, events.execution_time_ms \
                         FROM schema_migration_import_events AS events \
                         INNER JOIN schema_migration_imports AS imports USING (import_id) \
                         WHERE imports.namespace = ? \
                         ORDER BY events.import_id, events.event_index",
                    params![namespace],
                    "could not read MySQL legacy import events",
                )
                .await?;
            let mut legacy_import_events = Vec::with_capacity(event_rows.len());
            for row in &event_rows {
                legacy_import_events.push(LegacyImportEventRecord {
                    import_id: row_value(row, 0, "legacy event import id")?,
                    event_index: row_value(row, 1, "legacy event index")?,
                    event_id: row_value(row, 2, "legacy event id")?,
                    original_version: row_value(row, 3, "legacy event version")?,
                    state: row_value(row, 4, "legacy event state")?,
                    filename: row_value(row, 5, "legacy event filename")?,
                    batch_id: row_value(row, 6, "legacy event batch id")?,
                    applied_at: row_value(row, 7, "legacy event timestamp")?,
                    checksum_algorithm: row_value(row, 8, "legacy event checksum algorithm")?,
                    checksum_value: row_value(row, 9, "legacy event checksum value")?,
                    execution_time_ms: row_value(row, 10, "legacy event execution time")?,
                });
            }
            (legacy_applied, legacy_imports, legacy_import_events)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

        let run_rows: Vec<Row> = self
            .query_rows(
                "SELECT run_id, batch_id, namespace, migration_version, direction, state, attempt, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        DATE_FORMAT(started_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s.%fZ'), duration_ms, \
                        checksum_at_execution, plan_hash, error_code, error_summary_redacted, \
                        reconciliation_result, reconciliation_plan, reconciliation_evidence_ref, \
                        reconciliation_reason, reconciled_by, reconciled_from_state, related_run_id \
                 FROM schema_migration_runs WHERE namespace = ? \
                 ORDER BY created_at, run_id",
                params![namespace],
                "could not read MySQL migration history",
            )
            .await?;
        let mut runs = Vec::with_capacity(run_rows.len());
        for row in &run_rows {
            runs.push(RunRecord {
                run_id: row_value(row, 0, "run id")?,
                batch_id: row_value(row, 1, "batch id")?,
                namespace: row_value(row, 2, "run namespace")?,
                migration_id: row_value(row, 3, "run migration id")?,
                direction: row_value(row, 4, "run direction")?,
                state: row_value(row, 5, "run state")?,
                attempt: row_value(row, 6, "run attempt")?,
                created_at: row_value(row, 7, "run creation time")?,
                started_at: row_value(row, 8, "run start time")?,
                ended_at: row_value(row, 9, "run end time")?,
                duration_ms: row_value(row, 10, "run duration")?,
                checksum_at_execution: row_value(row, 11, "run checksum")?,
                plan_hash: row_value(row, 12, "run plan hash")?,
                error_code: row_value(row, 13, "run error code")?,
                error_summary_redacted: row_value(row, 14, "run error summary")?,
                reconciliation_result: row_value(row, 15, "reconciliation result")?,
                reconciliation_plan: row_value(row, 16, "reconciliation plan")?,
                reconciliation_evidence_ref: row_value(
                    row,
                    17,
                    "reconciliation evidence reference",
                )?,
                reconciliation_reason: row_value(row, 18, "reconciliation reason")?,
                reconciled_by: row_value(row, 19, "reconciliation actor")?,
                reconciled_from_state: row_value(row, 20, "reconciled run state")?,
                related_run_id: row_value(row, 21, "related run id")?,
            });
        }

        let step_rows: Vec<Row> = self
            .query_rows(
                "SELECT steps.run_id, steps.step_index, steps.step_key, steps.direction, \
                        steps.statement_hash, steps.statement_kind, steps.transaction_class, \
                        steps.state, DATE_FORMAT(steps.started_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        DATE_FORMAT(steps.acknowledged_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        steps.error_code, steps.evidence \
                 FROM schema_migration_steps AS steps \
                 INNER JOIN schema_migration_runs AS runs USING (run_id) \
                 WHERE runs.namespace = ? \
                 ORDER BY runs.created_at, runs.run_id, steps.step_index",
                params![namespace],
                "could not read MySQL migration journal",
            )
            .await?;
        let mut steps = Vec::with_capacity(step_rows.len());
        for row in &step_rows {
            steps.push(StepRecord {
                run_id: row_value(row, 0, "step run id")?,
                step_index: row_value(row, 1, "step index")?,
                step_key: row_value(row, 2, "step key")?,
                direction: row_value(row, 3, "step direction")?,
                statement_hash: row_value(row, 4, "step hash")?,
                statement_kind: row_value(row, 5, "step kind")?,
                transaction_class: row_value(row, 6, "step transaction class")?,
                state: row_value(row, 7, "step state")?,
                started_at: row_value(row, 8, "step start time")?,
                acknowledged_at: row_value(row, 9, "step acknowledgement time")?,
                error_code: row_value(row, 10, "step error code")?,
                evidence: row_value(row, 11, "step evidence")?,
            });
        }

        let lock_rows: Vec<Row> = self
            .query_rows(
                "SELECT scope, owner_id, lock_strategy, \
                        DATE_FORMAT(acquired_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        DATE_FORMAT(last_observed_at, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                        run_id, lock_epoch, state \
                 FROM schema_migration_locks WHERE scope = 'database'",
                Params::Empty,
                "could not read MySQL lock metadata",
            )
            .await?;
        if lock_rows.len() > 1 {
            return Err(DbmError::Diverged(
                "MySQL metadata contains duplicate database lock rows".to_owned(),
            ));
        }
        let lock = match lock_rows.first() {
            Some(row) => Some(LockRecord {
                scope: row_value(row, 0, "lock scope")?,
                owner_id: row_value(row, 1, "lock owner")?,
                lock_strategy: row_value(row, 2, "lock strategy")?,
                acquired_at: row_value(row, 3, "lock acquisition time")?,
                last_observed_at: row_value(row, 4, "lock observation time")?,
                run_id: row_value(row, 5, "lock run id")?,
                lock_epoch: row_value(row, 6, "lock epoch")?,
                state: row_value(row, 7, "lock state")?,
            }),
            None => None,
        };

        let snapshot = MetadataSnapshot {
            applied,
            legacy_applied,
            legacy_imports,
            legacy_import_events,
            runs,
            steps,
            lock,
            initialized: true,
            metadata_format_version: Some(format_version),
        };
        let expected_count = if format_version >= crate::storage::PREVIOUS_METADATA_FORMAT_VERSION {
            8
        } else {
            5
        };
        if relation_count != expected_count || table_count != expected_count {
            return Err(DbmError::Diverged(format!(
                "MySQL metadata format {format_version} does not match its reserved table set"
            )));
        }
        Ok(snapshot)
    }

    async fn metadata_check_constraint(
        &mut self,
        table: &str,
        expected_clause: &'static str,
    ) -> Result<String> {
        let rows = self
            .query_rows::<(String, String)>(
                "SELECT table_constraints.CONSTRAINT_NAME, check_constraints.CHECK_CLAUSE \
                 FROM information_schema.TABLE_CONSTRAINTS AS table_constraints \
                 INNER JOIN information_schema.CHECK_CONSTRAINTS AS check_constraints \
                    ON check_constraints.CONSTRAINT_SCHEMA = table_constraints.CONSTRAINT_SCHEMA \
                   AND check_constraints.CONSTRAINT_NAME = table_constraints.CONSTRAINT_NAME \
                 WHERE table_constraints.CONSTRAINT_SCHEMA = DATABASE() \
                   AND table_constraints.TABLE_NAME = ? \
                   AND table_constraints.CONSTRAINT_TYPE = 'CHECK'",
                params![table],
                "could not inspect MySQL metadata check constraints",
            )
            .await?;
        let mut matches = Vec::new();
        for (name, clause) in rows {
            if normalized_check_clause(&clause) == expected_clause {
                matches.push(name);
            }
        }
        if matches.len() != 1 {
            return Err(DbmError::Diverged(format!(
                "MySQL metadata upgrade expected exactly one {expected_clause} check on {table}; found {}",
                matches.len()
            )));
        }
        let name = matches.remove(0);
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(DbmError::Diverged(
                "MySQL metadata check has an unsupported identifier".to_owned(),
            ));
        }
        Ok(name)
    }

    async fn execute_metadata_upgrade_ddl(&mut self, sql: &str, operation: &str) -> Result<()> {
        self.verify_session_inner().await?;
        match self.connection()?.query_drop(sql).await {
            Ok(()) => self.verify_session_inner().await,
            Err(error) => {
                let redacted_reason = match error {
                    mysql_async::Error::Server(server_error) => format!(
                        "server error {}, SQLSTATE {}",
                        server_error.code, server_error.state
                    ),
                    _ => "the pinned connection outcome is uncertain".to_owned(),
                };
                Err(DbmError::ReconciliationRequired(format!(
                    "MySQL/MariaDB {operation} failed ({redacted_reason}) after implicit DDL commit; metadata may be partially upgraded, inspect reserved tables and do not retry automatically"
                )))
            }
        }
    }

    async fn read_snapshot_consistently(&mut self, namespace: &str) -> Result<MetadataSnapshot> {
        let owns_transaction = !self.transaction_open;
        if owns_transaction {
            self.verify_session_inner().await?;
            self.connection()?
                .query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
                .await
                .map_err(|_| driver_error("could not start a MySQL read snapshot"))?;
            self.transaction_open = true;
        }
        let result = self.read_snapshot_contents(namespace).await;
        if !owns_transaction {
            return result;
        }
        match result {
            Ok(snapshot) => {
                self.connection()?.query_drop("COMMIT").await.map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "MySQL read snapshot could not be closed cleanly".to_owned(),
                    )
                })?;
                self.transaction_open = false;
                Ok(snapshot)
            }
            Err(read_error) => match self.connection()?.query_drop("ROLLBACK").await {
                Ok(()) => {
                    self.transaction_open = false;
                    Err(read_error)
                }
                Err(_) => Err(DbmError::ReconciliationRequired(
                    "MySQL read snapshot failed and its transaction could not be closed".to_owned(),
                )),
            },
        }
    }

    async fn legacy_table_exists(&mut self, table: &str) -> Result<bool> {
        let count = self
            .query_first::<u64>(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name = ?",
                params![table],
                "could not inspect the MySQL legacy history table",
            )
            .await?
            .ok_or_else(|| driver_error("MySQL returned no legacy table inspection result"))?;
        Ok(count == 1)
    }

    async fn require_legacy_table(&mut self, table: &str, tool: &str) -> Result<()> {
        if self.legacy_table_exists(table).await? {
            return Ok(());
        }
        Err(DbmError::Configuration(format!(
            "legacy {tool} table {table} was not found in the selected MySQL/MariaDB database"
        )))
    }

    async fn inspect_legacy_history_inner(
        &mut self,
        tool: LegacyTool,
        source_identity: &str,
    ) -> Result<LegacyImportSnapshot> {
        self.verify_session_inner().await?;
        let snapshot = match tool {
            LegacyTool::Knex => {
                self.require_legacy_table("knex_migrations", "Knex").await?;
                let rows = self
                    .query_rows::<(i64, String, i64, Option<String>)>(
                        "SELECT CAST(id AS SIGNED), name, CAST(batch AS SIGNED), \
                                CAST(migration_time AS CHAR) \
                         FROM knex_migrations ORDER BY id",
                        Params::Empty,
                        "could not read MySQL/MariaDB Knex migration history",
                    )
                    .await?;
                let mut migrations = Vec::with_capacity(rows.len());
                for (id, name, batch, migration_time) in rows {
                    migrations.push(KnexRow {
                        id,
                        name,
                        batch,
                        migration_time,
                    });
                }

                let lock = if self.legacy_table_exists("knex_migrations_lock").await? {
                    let rows = self
                        .query_rows::<(String, Option<String>)>(
                            "SELECT CAST(is_locked AS CHAR), CAST(`index` AS CHAR) \
                             FROM knex_migrations_lock ORDER BY `index`",
                            Params::Empty,
                            "could not read MySQL/MariaDB Knex lock state",
                        )
                        .await?;
                    let mut locks = Vec::with_capacity(rows.len());
                    for (locked_text, index_text) in rows {
                        locks.push(KnexLock {
                            is_locked: parse_legacy_bool(&locked_text, "Knex", "is_locked")?,
                            index: index_text
                                .map(|value| {
                                    value
                                        .parse::<i64>()
                                        .map_err(|_| legacy_row_error("Knex", "lock index"))
                                })
                                .transpose()?,
                        });
                    }
                    if locks.len() > 1 {
                        return Err(DbmError::Safety(
                            "Knex migration lock table contains multiple rows; refusing cutover inspection"
                                .to_owned(),
                        ));
                    }
                    locks.into_iter().next()
                } else {
                    None
                };
                inspect_knex(source_identity, migrations, lock)?
            }
            LegacyTool::Goose => {
                self.require_legacy_table("goose_db_version", "Goose")
                    .await?;
                let rows = self
                    .query_rows::<(i64, i64, String, Option<String>)>(
                        "SELECT CAST(id AS SIGNED), CAST(version_id AS SIGNED), \
                                CAST(is_applied AS CHAR), CAST(tstamp AS CHAR) \
                         FROM goose_db_version ORDER BY id",
                        Params::Empty,
                        "could not read MySQL/MariaDB Goose migration history",
                    )
                    .await?;
                let mut events = Vec::with_capacity(rows.len());
                for (id, version_id, applied_text, timestamp) in rows {
                    events.push(GooseRow {
                        id,
                        version_id,
                        is_applied: parse_legacy_bool(&applied_text, "Goose", "is_applied")?,
                        timestamp,
                    });
                }
                inspect_goose(source_identity, events)?
            }
            LegacyTool::Sqlx => {
                self.require_legacy_table("_sqlx_migrations", "SQLx")
                    .await?;
                let rows = self
                    .query_rows::<(i64, String, Option<String>, String, Vec<u8>, i64)>(
                        "SELECT CAST(version AS SIGNED), description, \
                                CAST(installed_on AS CHAR), CAST(success AS CHAR), checksum, \
                                CAST(execution_time AS SIGNED) \
                         FROM _sqlx_migrations ORDER BY version",
                        Params::Empty,
                        "could not read MySQL/MariaDB SQLx migration history",
                    )
                    .await?;
                let mut migrations = Vec::with_capacity(rows.len());
                for (
                    version,
                    description,
                    installed_on,
                    success_text,
                    checksum,
                    execution_time_ms,
                ) in rows
                {
                    migrations.push(SqlxRow {
                        version,
                        description,
                        installed_on,
                        success: parse_legacy_bool(&success_text, "SQLx", "success")?,
                        checksum,
                        execution_time_ms,
                    });
                }
                inspect_sqlx(source_identity, migrations)?
            }
        };
        self.verify_session_inner().await?;
        Ok(snapshot)
    }

    async fn persist_legacy_import_inner(&mut self, import: LegacyImportWrite<'_>) -> Result<()> {
        self.ensure_metadata_transaction()?;
        self.verify_session_inner().await?;
        let source = import.source;
        let event_count = i64::try_from(source.events.len()).map_err(|_| {
            DbmError::Execution("legacy event count exceeds metadata storage limits".to_owned())
        })?;
        let current_migration_count =
            i64::try_from(source.current_migrations.len()).map_err(|_| {
                DbmError::Execution(
                    "legacy migration count exceeds metadata storage limits".to_owned(),
                )
            })?;
        self.execute_drop(
            "INSERT INTO schema_migration_imports \
             (import_id, namespace, origin_tool, source_identity, snapshot_digest, plan_digest, \
              imported_at, event_count, current_migration_count) \
             VALUES (?, ?, ?, ?, ?, ?, STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), ?, ?)",
            params![
                import.import_id,
                import.namespace,
                crate::legacy::tool_name(import.tool),
                source.source_identity,
                source.digest,
                import.plan_digest,
                import.imported_at,
                event_count,
                current_migration_count,
            ],
            "could not record MySQL/MariaDB legacy import",
        )
        .await?;

        for (event_index, event) in source.events.iter().enumerate() {
            let event_index = i64::try_from(event_index).map_err(|_| {
                DbmError::Execution("legacy event index exceeds storage limits".to_owned())
            })?;
            let checksum_algorithm = event
                .checksum
                .as_ref()
                .map(|checksum| checksum.algorithm.as_str());
            let checksum_value = event
                .checksum
                .as_ref()
                .map(|checksum| checksum.value.as_str());
            self.execute_drop(
                "INSERT INTO schema_migration_import_events \
                 (import_id, event_index, event_id, original_version, state, filename, batch_id, \
                  applied_at, checksum_algorithm, checksum_value, execution_time_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    import.import_id,
                    event_index,
                    event.event_id,
                    event.original_version,
                    crate::legacy::event_state_name(event.state),
                    event.filename,
                    event.batch_id,
                    event.applied_at,
                    checksum_algorithm,
                    checksum_value,
                    event.execution_time_ms,
                ],
                "could not record a MySQL/MariaDB legacy import event",
            )
            .await?;
        }

        for (position, migration) in source.current_migrations.iter().enumerate() {
            let migration_key = legacy_migration_key(
                import.namespace,
                crate::legacy::tool_name(import.tool),
                &source.source_identity,
                &migration.original_identity,
            );
            let apply_order = i64::try_from(position + 1).map_err(|_| {
                DbmError::Execution("legacy apply order exceeds metadata storage limits".to_owned())
            })?;
            let checksum_algorithm = migration
                .checksum
                .as_ref()
                .map(|checksum| checksum.algorithm.as_str());
            let checksum_value = migration
                .checksum
                .as_ref()
                .map(|checksum| checksum.value.as_str());
            let verification_status =
                crate::legacy::verification_status_name(migration.verification_status);
            self.execute_drop(
                "INSERT INTO schema_migrations \
                 (namespace, migration_key, kind, native_version, original_version, filename, \
                  source_path, description, authoring_type, content_checksum, origin_tool, \
                  origin_database_identity, origin_identity, legacy_import_id, batch_id, \
                  checksum_algorithm, checksum_value, execution_time_ms, verification_status, \
                  apply_order, applied_at, origin_applied_at, observed_at) \
                 VALUES (?, ?, 'legacy', NULL, ?, ?, NULL, ?, 'legacy_unknown', NULL, \
                         ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, \
                         STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'))",
                params![
                    import.namespace,
                    migration_key,
                    migration.original_version,
                    migration.filename,
                    migration.filename,
                    crate::legacy::tool_name(import.tool),
                    source.source_identity,
                    migration.original_identity,
                    import.import_id,
                    migration.batch_id,
                    checksum_algorithm,
                    checksum_value,
                    migration.execution_time_ms,
                    verification_status,
                    apply_order,
                    migration.applied_at,
                    import.imported_at,
                ],
                "could not record an imported MySQL/MariaDB migration",
            )
            .await?;
        }
        self.verify_session_inner().await
    }

    async fn persist_legacy_adoption_inner(
        &mut self,
        adoption: LegacyAdoptionWrite<'_>,
    ) -> Result<()> {
        self.ensure_metadata_transaction()?;
        self.verify_session_inner().await?;
        let changed = self
            .execute_drop(
                "UPDATE schema_migrations SET verification_status = 'adopted' \
                 WHERE namespace = ? AND migration_key = ? AND kind = 'legacy' \
                   AND legacy_import_id = ? AND verification_status <> 'adopted'",
                params![
                    adoption.namespace,
                    adoption.legacy_migration_key,
                    adoption.import_id
                ],
                "could not update MySQL/MariaDB legacy adoption state",
            )
            .await?;
        if changed != 1 {
            return Err(DbmError::Diverged(
                "legacy adoption state changed while the operation was in progress".to_owned(),
            ));
        }
        self.execute_drop(
            "INSERT INTO schema_migration_adoptions \
             (adoption_id, namespace, import_id, legacy_migration_key, native_migration_key, \
              native_version, native_checksum, evidence_ref, approved_by, reason, adopted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'))",
            params![
                adoption.adoption_id,
                adoption.namespace,
                adoption.import_id,
                adoption.legacy_migration_key,
                adoption.native_migration_key,
                adoption.native_version,
                adoption.native_checksum,
                adoption.evidence_ref,
                adoption.actor,
                adoption.reason,
                adoption.adopted_at,
            ],
            "could not record MySQL/MariaDB legacy adoption",
        )
        .await?;
        self.verify_session_inner().await
    }

    async fn initialize_metadata_in_transaction(&mut self, initialized_at: &str) -> Result<()> {
        self.ensure_writable()?;
        if self.held_lock.is_none() {
            return Err(DbmError::Safety(
                "MySQL metadata initialization requires the session GET_LOCK".to_owned(),
            ));
        }
        if !self.transaction_open {
            return Err(DbmError::Safety(
                "MySQL metadata initialization requires the managed transaction boundary"
                    .to_owned(),
            ));
        }
        let (relation_count, table_count) = self.table_counts().await?;
        if matches!((relation_count, table_count), (5, 5) | (8, 8)) {
            let snapshot = self
                .read_snapshot_contents("__dbm_metadata_probe__")
                .await?;
            if relation_count != 8
                || !snapshot.initialized
                || snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
            {
                return Err(DbmError::Diverged(
                    "existing MySQL metadata requires an explicit metadata upgrade before migration writes"
                        .to_owned(),
                ));
            }
            return Ok(());
        }
        if relation_count != 0 {
            return Err(DbmError::Diverged(format!(
                "MySQL migration metadata is partial ({relation_count} of 8 reserved objects); refusing automatic repair"
            )));
        }

        self.connection()?.query_drop("COMMIT").await.map_err(|_| {
            DbmError::ReconciliationRequired(
                "could not close the empty transaction before MySQL metadata bootstrap".to_owned(),
            )
        })?;
        self.transaction_open = false;
        for ddl in MYSQL_METADATA_TABLE_DDL {
            if self.connection()?.query_drop(ddl).await.is_err() {
                return Err(DbmError::ReconciliationRequired(
                    "MySQL metadata bootstrap may be partial; inspect reserved tables before any retry"
                        .to_owned(),
                ));
            }
        }
        self.connection()?
            .query_drop("START TRANSACTION")
            .await
            .map_err(|_| {
                DbmError::ReconciliationRequired(
                    "MySQL metadata tables were created but the identity transaction could not begin"
                        .to_owned(),
                )
            })?;
        self.transaction_open = true;
        let installation_id = uuid::Uuid::new_v4().to_string();
        self.execute_drop(
            "INSERT INTO schema_migration_meta \
             (singleton, metadata_format_version, installation_id, initialized_at) \
             VALUES (1, ?, ?, STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'))",
            params![
                crate::storage::METADATA_FORMAT_VERSION,
                installation_id,
                initialized_at
            ],
            "could not initialize MySQL metadata identity",
        )
        .await?;
        Ok(())
    }

    async fn upgrade_metadata_v4_to_v5_locked(&mut self) -> Result<()> {
        let kind_check = self
            .metadata_check_constraint("schema_migrations", "kindinversionedlegacy")
            .await?;
        let shape_check = self
            .metadata_check_constraint(
                "schema_migrations",
                "kindversionedandnativeversionisnotnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindlegacyandnativeversionisnullandoriginalversionisnotnullandlegacyimportidisnotnullandorigintoolisnotnullandorigindatabaseidentityisnotnullandoriginidentityisnotnullandverificationstatusisnotnullandobservedatisnotnull",
            )
            .await?;
        let direction_check = self
            .metadata_check_constraint("schema_migration_runs", "directioninupdownreconcile")
            .await?;
        let staging_table_count = self
            .query_first::<u64>(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name IN (\
                     'schema_migration_meta_v5_stage', 'schema_migration_meta_v4_backup')",
                Params::Empty,
                "could not inspect MySQL/MariaDB v5 metadata staging tables",
            )
            .await?
            .ok_or_else(|| driver_error("MySQL returned no v5 staging table count"))?;
        if staging_table_count != 0 {
            return Err(DbmError::Diverged(
                "MySQL/MariaDB metadata v5 staging tables already exist; inspect them before any mutation"
                    .to_owned(),
            ));
        }

        let migration_ddl = format!(
            "ALTER TABLE schema_migrations \
               DROP CONSTRAINT `{kind_check}`, DROP CONSTRAINT `{shape_check}`, \
               ADD COLUMN repeatable_name VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL AFTER native_version, \
               ADD UNIQUE KEY schema_migrations_namespace_repeatable_uq (namespace, kind, repeatable_name), \
               ADD CONSTRAINT schema_migrations_kind_check CHECK (kind IN ('versioned', 'repeatable', 'legacy')), \
               ADD CONSTRAINT schema_migrations_repeatable_name_check CHECK (repeatable_name IS NULL OR CHAR_LENGTH(repeatable_name) BETWEEN 1 AND 128), \
               ADD CONSTRAINT schema_migrations_legacy_shape_check CHECK (\
                   (kind = 'versioned' AND native_version IS NOT NULL AND repeatable_name IS NULL AND original_version IS NULL \
                    AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                   (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL AND original_version IS NULL \
                    AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                   (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL \
                    AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL AND origin_database_identity IS NOT NULL \
                    AND origin_identity IS NOT NULL AND verification_status IS NOT NULL AND observed_at IS NOT NULL))"
        );
        self.execute_metadata_upgrade_ddl(
            &migration_ddl,
            "adding repeatable state to schema_migrations",
        )
        .await?;
        let runs_ddl = format!(
            "ALTER TABLE schema_migration_runs \
               MODIFY COLUMN migration_version VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
               DROP CONSTRAINT `{direction_check}`, \
               ADD CONSTRAINT schema_migration_runs_direction_check CHECK (direction IN ('up', 'down', 'repeat', 'reconcile'))"
        );
        self.execute_metadata_upgrade_ddl(&runs_ddl, "adding repeatable run direction")
            .await?;

        self.execute_metadata_upgrade_ddl(
            "CREATE TABLE schema_migration_meta_v5_stage (\
                singleton TINYINT UNSIGNED NOT NULL PRIMARY KEY CHECK (singleton = 1),\
                metadata_format_version INT NOT NULL,\
                installation_id CHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                initialized_at DATETIME(6) NOT NULL,\
                upgraded_at DATETIME(6) NULL,\
                CONSTRAINT schema_migration_meta_v5_format_check CHECK (metadata_format_version = 5)\
             ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
            "creating the staged v5 metadata identity",
        )
        .await?;
        let staged_identity = self
            .execute_drop(
                "INSERT INTO schema_migration_meta_v5_stage \
                 (singleton, metadata_format_version, installation_id, initialized_at, upgraded_at) \
                 SELECT singleton, 5, installation_id, initialized_at, CURRENT_TIMESTAMP(6) \
                 FROM schema_migration_meta WHERE singleton = 1 AND metadata_format_version = 4",
                Params::Empty,
                "could not stage MySQL/MariaDB metadata format 5 identity",
            )
            .await?;
        if staged_identity != 1 {
            return Err(DbmError::Diverged(
                "MySQL/MariaDB metadata format changed during explicit v5 upgrade".to_owned(),
            ));
        }
        self.execute_metadata_upgrade_ddl(
            "RENAME TABLE schema_migration_meta TO schema_migration_meta_v4_backup, \
                           schema_migration_meta_v5_stage TO schema_migration_meta",
            "atomically swapping the metadata format identity",
        )
        .await?;
        self.execute_metadata_upgrade_ddl(
            "DROP TABLE schema_migration_meta_v4_backup",
            "removing the replaced v4 metadata identity",
        )
        .await?;
        let (relation_count, table_count) = self.table_counts().await?;
        if (relation_count, table_count) != (8, 8) {
            return Err(DbmError::ReconciliationRequired(
                "MySQL/MariaDB metadata identity was upgraded but the v5 table set could not be confirmed"
                    .to_owned(),
            ));
        }
        self.verify_session_inner().await
    }
}

impl DatabaseSession for MySqlSession {
    fn info(&self) -> &DatabaseInfo {
        &self.info
    }

    fn read_snapshot<'a>(
        &'a mut self,
        namespace: &'a str,
    ) -> BoxFuture<'a, Result<MetadataSnapshot>> {
        Box::pin(async move { self.read_snapshot_consistently(namespace).await })
    }

    fn inspect_legacy_history<'a>(
        &'a mut self,
        tool: LegacyTool,
        source_identity: &'a str,
    ) -> BoxFuture<'a, Result<LegacyImportSnapshot>> {
        Box::pin(async move {
            self.inspect_legacy_history_inner(tool, source_identity)
                .await
        })
    }

    fn persist_legacy_import<'a>(
        &'a mut self,
        import: LegacyImportWrite<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.persist_legacy_import_inner(import).await })
    }

    fn persist_legacy_adoption<'a>(
        &'a mut self,
        adoption: LegacyAdoptionWrite<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.persist_legacy_adoption_inner(adoption).await })
    }

    fn read_legacy_adoption_id<'a>(
        &'a mut self,
        namespace: &'a str,
        legacy_migration_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            self.query_first::<String>(
                "SELECT adoption_id FROM schema_migration_adoptions \
                 WHERE namespace = ? AND legacy_migration_key = ?",
                params![namespace, legacy_migration_key],
                "could not read MySQL/MariaDB legacy adoption",
            )
            .await
        })
    }

    fn initialize_metadata<'a>(&'a mut self, initialized_at: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.initialize_metadata_in_transaction(initialized_at)
                .await
        })
    }

    fn upgrade_metadata_to_current<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if self.held_lock.is_none() || self.transaction_open {
                return Err(DbmError::Safety(
                    "MySQL/MariaDB metadata upgrade requires the pinned named lock and no open transaction because DDL commits implicitly"
                        .to_owned(),
                ));
            }
            self.verify_session_inner().await?;

            let (relation_count, table_count) = self.table_counts().await?;
            let metadata = self
                .query_first::<(i64, String)>(
                    "SELECT metadata_format_version, installation_id \
                     FROM schema_migration_meta WHERE singleton = 1",
                    Params::Empty,
                    "could not read MySQL metadata before upgrade",
                )
                .await?
                .ok_or_else(|| {
                    DbmError::Diverged(
                        "MySQL/MariaDB metadata identity row is missing during upgrade".to_owned(),
                    )
                })?;
            if metadata.1.is_empty() {
                return Err(DbmError::Diverged(format!(
                    "explicit MySQL/MariaDB metadata upgrade found an invalid installation id for format {}",
                    metadata.0
                )));
            }
            if metadata.0 == crate::storage::PREVIOUS_METADATA_FORMAT_VERSION {
                if (relation_count, table_count) != (8, 8) {
                    return Err(DbmError::Diverged(format!(
                        "MySQL/MariaDB format 4 upgrade requires exactly eight intact metadata tables; found {table_count} tables among {relation_count} reserved objects"
                    )));
                }
                self.upgrade_metadata_v4_to_v5_locked().await?;
                return Ok(());
            }
            if metadata.0 != crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION
                || (relation_count, table_count) != (5, 5)
            {
                return Err(DbmError::Diverged(format!(
                    "explicit MySQL/MariaDB upgrade supports intact formats 3 and 4; found format {} with {table_count} tables among {relation_count} reserved objects",
                    metadata.0
                )));
            }

            let kind_check = self
                .metadata_check_constraint("schema_migrations", "kindversioned")
                .await?;
            let authoring_check = self
                .metadata_check_constraint("schema_migrations", "authoringtypesql")
                .await?;
            let staging_table_count = self
                .query_first::<u64>(
                    "SELECT COUNT(*) FROM information_schema.tables \
                     WHERE table_schema = DATABASE() AND table_name IN (\
                         'schema_migration_meta_v4_stage', \
                         'schema_migration_meta_v3_backup')",
                    Params::Empty,
                    "could not inspect MySQL metadata upgrade staging tables",
                )
                .await?
                .ok_or_else(|| driver_error("MySQL returned no staging table count"))?;
            if staging_table_count != 0 {
                return Err(DbmError::Diverged(
                    "MySQL/MariaDB metadata upgrade staging tables already exist; inspect them before any mutation"
                        .to_owned(),
                ));
            }

            for ddl in &MYSQL_METADATA_TABLE_DDL[1..4] {
                self.execute_metadata_upgrade_ddl(ddl, "creating v4 legacy import tables")
                    .await?;
            }

            let migration_table_upgrade = format!(
                "ALTER TABLE schema_migrations \
                   DROP CONSTRAINT `{kind_check}`, \
                   DROP CONSTRAINT `{authoring_check}`, \
                   MODIFY COLUMN kind VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
                   MODIFY COLUMN native_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   MODIFY COLUMN filename LONGTEXT NULL, \
                   MODIFY COLUMN source_path LONGTEXT NULL, \
                   MODIFY COLUMN description LONGTEXT NULL, \
                   MODIFY COLUMN content_checksum VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   MODIFY COLUMN applied_at DATETIME(6) NULL, \
                   ADD COLUMN original_version LONGTEXT NULL AFTER native_version, \
                   ADD COLUMN origin_tool VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   ADD COLUMN origin_database_identity VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   ADD COLUMN origin_identity LONGTEXT NULL, \
                   ADD COLUMN legacy_import_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   ADD COLUMN batch_id LONGTEXT NULL, \
                   ADD COLUMN checksum_algorithm VARCHAR(32) NULL, \
                   ADD COLUMN checksum_value LONGTEXT NULL, \
                   ADD COLUMN execution_time_ms BIGINT NULL, \
                   ADD COLUMN verification_status VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NULL, \
                   ADD COLUMN origin_applied_at LONGTEXT NULL, \
                   ADD COLUMN observed_at DATETIME(6) NULL, \
                   ADD CONSTRAINT schema_migrations_kind_check \
                     CHECK (kind IN ('versioned', 'legacy')), \
                   ADD CONSTRAINT schema_migrations_authoring_type_check \
                     CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')), \
                   ADD CONSTRAINT schema_migrations_execution_time_check \
                     CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0), \
                   ADD CONSTRAINT schema_migrations_verification_status_check \
                     CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')), \
                   ADD CONSTRAINT schema_migrations_legacy_shape_check \
                     CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND original_version IS NULL \
                             AND legacy_import_id IS NULL AND origin_tool IS NULL) OR \
                            (kind = 'legacy' AND native_version IS NULL AND original_version IS NOT NULL \
                             AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL \
                             AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL \
                             AND verification_status IS NOT NULL AND observed_at IS NOT NULL)), \
                   ADD CONSTRAINT schema_migrations_legacy_import_fk FOREIGN KEY (legacy_import_id) \
                     REFERENCES schema_migration_imports (import_id)"
            );
            self.execute_metadata_upgrade_ddl(
                &migration_table_upgrade,
                "altering the current migration metadata table",
            )
            .await?;

            self.execute_metadata_upgrade_ddl(
                "CREATE TABLE schema_migration_meta_v4_stage (\
                    singleton TINYINT UNSIGNED NOT NULL PRIMARY KEY CHECK (singleton = 1),\
                    metadata_format_version INT NOT NULL,\
                    installation_id CHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    initialized_at DATETIME(6) NOT NULL,\
                    upgraded_at DATETIME(6) NULL,\
                    CONSTRAINT schema_migration_meta_v4_format_check CHECK (metadata_format_version = 4)\
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
                "creating the staged v4 metadata identity",
            )
            .await?;
            let staged_identity = self
                .execute_drop(
                    "INSERT INTO schema_migration_meta_v4_stage \
                     (singleton, metadata_format_version, installation_id, initialized_at, upgraded_at) \
                     SELECT singleton, 4, installation_id, initialized_at, CURRENT_TIMESTAMP(6) \
                     FROM schema_migration_meta \
                     WHERE singleton = 1 AND metadata_format_version = 3",
                    Params::Empty,
                    "could not stage MySQL/MariaDB metadata format 4 identity",
                )
                .await?;
            if staged_identity != 1 {
                return Err(DbmError::Diverged(
                    "MySQL/MariaDB metadata identity changed during explicit upgrade".to_owned(),
                ));
            }
            self.execute_metadata_upgrade_ddl(
                "RENAME TABLE schema_migration_meta TO schema_migration_meta_v3_backup, \
                               schema_migration_meta_v4_stage TO schema_migration_meta",
                "atomically swapping the metadata format identity",
            )
            .await?;
            self.execute_metadata_upgrade_ddl(
                "DROP TABLE schema_migration_meta_v3_backup",
                "removing the replaced v3 metadata identity",
            )
            .await?;
            let (final_relations, final_tables) = self.table_counts().await?;
            if (final_relations, final_tables) != (8, 8) {
                return Err(DbmError::ReconciliationRequired(
                    "MySQL/MariaDB metadata version was updated but the final v4 table set could not be confirmed"
                        .to_owned(),
                ));
            }
            self.verify_session_inner().await?;
            self.upgrade_metadata_v4_to_v5_locked().await
        })
    }

    fn insert_planned_run<'a>(
        &'a mut self,
        migration: &'a Migration,
        run: PlannedRunData<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session_inner().await?;
            let key = migration.migration_key();
            let attempt = self
                .query_first::<i64>(
                    "SELECT COALESCE(MAX(attempt), 0) + 1 \
                     FROM schema_migration_runs WHERE namespace = ? AND migration_key = ? AND direction = ?",
                    params![&migration.identity.namespace, &key, run.direction],
                    "could not read the next MySQL migration attempt",
                )
                .await?
                .ok_or_else(|| driver_error("MySQL returned no migration attempt value"))?;
            self.execute_drop(
                "INSERT INTO schema_migration_runs \
                 (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                  state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
                 VALUES (?, ?, ?, ?, ?, ?, 'planned', ?, STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), ?, ?, 'human')",
                params![
                    run.run_id,
                    run.batch_id,
                    &migration.identity.namespace,
                    &key,
                    &migration.identity.version,
                    run.direction,
                    attempt,
                    run.created_at,
                    &migration.checksum,
                    run.plan_hash
                ],
                "could not record a planned MySQL migration run",
            )
            .await?;
            Ok(())
        })
    }

    fn mark_run_started<'a>(
        &'a mut self,
        run_id: &'a str,
        started_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_runs SET state = 'started', \
                     started_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ') \
                     WHERE run_id = ? AND state = 'planned'",
                    params![started_at, run_id],
                    "could not mark the MySQL migration run started",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "MySQL run cannot transition from its current state to started".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn insert_step<'a>(&'a mut self, step: StepData<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.execute_drop(
                "INSERT INTO schema_migration_steps \
                 (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                  transaction_class, state, started_at, acknowledged_at, error_code, evidence, attempt_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, \
                         STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                         STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), ?, ?, ?)",
                params![
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
                    step.run_id
                ],
                "could not record a MySQL migration journal step",
            )
            .await?;
            Ok(())
        })
    }

    fn acknowledge_step<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        acknowledged_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_steps SET state = 'acknowledged', \
                     acknowledged_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ') \
                     WHERE run_id = ? AND step_index = ? AND state = 'intent_recorded'",
                    params![acknowledged_at, run_id, step_index],
                    "could not acknowledge a MySQL journal step",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "MySQL migration journal step cannot be acknowledged".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn mark_step_indeterminate<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        error_code: &'a str,
        evidence: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_steps SET state = 'indeterminate', \
                     error_code = ?, evidence = ? \
                     WHERE run_id = ? AND step_index = ? AND state = 'intent_recorded'",
                    params![error_code, evidence, run_id, step_index],
                    "could not mark a MySQL journal step indeterminate",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "MySQL migration journal step cannot be marked indeterminate".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn reconcile_run<'a>(
        &'a mut self,
        reconciliation: ReconciliationData<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_runs SET reconciled_from_state = state, state = 'reconciled', \
                     reconciliation_result = ?, reconciliation_plan = ?, reconciliation_evidence_ref = ?, \
                     reconciliation_reason = ?, reconciled_by = ? \
                     WHERE run_id = ? AND state <> 'reconciled'",
                    params![
                        reconciliation.result,
                        reconciliation.repair_plan,
                        reconciliation.evidence_ref,
                        reconciliation.reason,
                        reconciliation.actor,
                        reconciliation.run_id
                    ],
                    "could not reconcile the MySQL migration run",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "MySQL run is missing or already reconciled".to_owned(),
                ));
            }
            let inserted = self
                .execute_drop(
                    "INSERT INTO schema_migration_runs \
                     (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                      state, attempt, created_at, started_at, ended_at, duration_ms, \
                      checksum_at_execution, plan_hash, reconciliation_result, reconciliation_plan, \
                      reconciliation_evidence_ref, reconciliation_reason, reconciled_by, related_run_id, actor_kind) \
                     SELECT ?, batch_id, namespace, migration_key, migration_version, \
                            'reconcile', 'succeeded', 1, \
                            STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                            STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                            STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), 0, \
                            checksum_at_execution, plan_hash, ?, ?, ?, ?, ?, run_id, 'human' \
                     FROM schema_migration_runs WHERE run_id = ?",
                    params![
                        reconciliation.reconcile_run_id,
                        reconciliation.now,
                        reconciliation.now,
                        reconciliation.now,
                        reconciliation.result,
                        reconciliation.repair_plan,
                        reconciliation.evidence_ref,
                        reconciliation.reason,
                        reconciliation.actor,
                        reconciliation.run_id
                    ],
                    "could not record MySQL reconciliation audit",
                )
                .await?;
            if inserted != 1 {
                return Err(DbmError::Diverged(
                    "MySQL reconciliation audit row could not be inserted".to_owned(),
                ));
            }
            self.execute_drop(
                "UPDATE schema_migration_steps SET state = 'reconciled', \
                 evidence = 'step state was reconciled by an approved operator decision' \
                 WHERE run_id = ? AND state IN ('intent_recorded', 'indeterminate')",
                params![reconciliation.run_id],
                "could not reconcile MySQL journal steps",
            )
            .await?;
            Ok(())
        })
    }

    fn release_reconciled_lock<'a>(
        &'a mut self,
        run_id: &'a str,
        released_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_locks SET state = 'released', run_id = NULL, \
                     last_observed_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ') \
                     WHERE scope = 'database' AND run_id = ? AND state = 'observed_active'",
                    params![released_at, run_id],
                    "could not release reconciled MySQL lock metadata",
                )
                .await?;
            if changed != 1 {
                let active = self
                    .query_first::<i64>(
                        "SELECT EXISTS(SELECT 1 FROM schema_migration_locks \
                         WHERE scope = 'database' AND state = 'observed_active')",
                        Params::Empty,
                        "could not inspect active MySQL lock metadata",
                    )
                    .await?
                    .unwrap_or(0);
                if active != 0 {
                    return Err(DbmError::Lock(
                        "active MySQL lock metadata is not associated with the selected run"
                            .to_owned(),
                    ));
                }
            }
            Ok(())
        })
    }

    fn insert_applied<'a>(
        &'a mut self,
        migration: &'a Migration,
        applied_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            let key = migration.migration_key();
            let apply_order = self
                .query_first::<i64>(
                    "SELECT COALESCE(MAX(apply_order), 0) + 1 FROM schema_migrations WHERE namespace = ?",
                    params![&migration.identity.namespace],
                    "could not read the next MySQL apply order",
                )
                .await?
                .ok_or_else(|| driver_error("MySQL returned no apply order"))?;
            match migration.kind {
                MigrationKind::Versioned => {
                    self.execute_drop(
                        "INSERT INTO schema_migrations \
                         (namespace, migration_key, kind, native_version, repeatable_name, filename, source_path, description, \
                          authoring_type, content_checksum, apply_order, applied_at) \
                         VALUES (?, ?, 'versioned', ?, NULL, ?, ?, ?, 'sql', ?, ?, \
                                 STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'))",
                        params![
                            &migration.identity.namespace,
                            &key,
                            &migration.identity.version,
                            &migration.filename,
                            &migration.source_path,
                            &migration.description,
                            &migration.checksum,
                            apply_order,
                            applied_at
                        ],
                        "could not record an applied MySQL migration",
                    )
                    .await?;
                }
                MigrationKind::Repeatable => {
                    self.execute_drop(
                        "INSERT INTO schema_migrations \
                         (namespace, migration_key, kind, native_version, repeatable_name, filename, source_path, description, \
                          authoring_type, content_checksum, apply_order, applied_at) \
                         VALUES (?, ?, 'repeatable', NULL, ?, ?, ?, ?, 'sql', ?, ?, \
                                 STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ')) \
                         ON DUPLICATE KEY UPDATE repeatable_name = VALUES(repeatable_name), \
                           filename = VALUES(filename), source_path = VALUES(source_path), \
                           description = VALUES(description), content_checksum = VALUES(content_checksum), \
                           apply_order = VALUES(apply_order), applied_at = VALUES(applied_at)",
                        params![
                            &migration.identity.namespace,
                            &key,
                            &migration.identity.version,
                            &migration.filename,
                            &migration.source_path,
                            &migration.description,
                            &migration.checksum,
                            apply_order,
                            applied_at
                        ],
                        "could not update applied MySQL repeatable state",
                    )
                    .await?;
                }
            }
            Ok(())
        })
    }

    fn delete_applied<'a>(&'a mut self, migration: &'a Migration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            if migration.kind == MigrationKind::Repeatable {
                return Err(DbmError::Safety(
                    "repeatable migrations do not support automatic rollback".to_owned(),
                ));
            }
            let key = migration.migration_key();
            let changed = self
                .execute_drop(
                    "DELETE FROM schema_migrations WHERE namespace = ? AND migration_key = ? AND native_version = ?",
                    params![
                        &migration.identity.namespace,
                        &key,
                        &migration.identity.version
                    ],
                    "could not remove rolled-back MySQL migration state",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "applied MySQL migration state changed during rollback".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn finish_run<'a>(&'a mut self, run: FinishedRunData<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            if !matches!(run.state, "succeeded" | "failed" | "indeterminate") {
                return Err(DbmError::Execution(format!(
                    "internal error: invalid terminal run state {}",
                    run.state
                )));
            }
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_runs SET state = ?, \
                     ended_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), duration_ms = ?, \
                     error_code = ?, error_summary_redacted = ? \
                     WHERE run_id = ? AND state = 'started'",
                    params![
                        run.state,
                        run.ended_at,
                        run.duration_ms,
                        run.error_code,
                        run.error_summary_redacted,
                        run.run_id
                    ],
                    "could not finish a MySQL migration run",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "MySQL run cannot transition from its current state to a terminal state"
                        .to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn record_lock<'a>(&'a mut self, lock: LockData<'a>) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            if self.held_lock.is_none() {
                return Err(DbmError::Safety(
                    "MySQL lock metadata requires the session GET_LOCK".to_owned(),
                ));
            }
            self.verify_session_inner().await?;
            let current = self
                .query_first::<(String, i64)>(
                    "SELECT state, lock_epoch FROM schema_migration_locks \
                     WHERE scope = 'database' FOR UPDATE",
                    Params::Empty,
                    "could not inspect MySQL lock metadata",
                )
                .await?;
            let current_epoch = match current {
                Some((state, epoch)) if state == "released" => Some(epoch),
                Some(_) => {
                    return Err(DbmError::Lock(
                        "MySQL migration lock metadata is unresolved; inspect with doctor and reconcile explicitly"
                            .to_owned(),
                    ));
                }
                None => None,
            };
            let next_epoch = match current_epoch {
                Some(epoch) => epoch.checked_add(1).ok_or_else(|| {
                    DbmError::Diverged("MySQL lock epoch is exhausted".to_owned())
                })?,
                None => 1,
            };
            let session_label = self
                .info
                .session_id
                .as_deref()
                .map(|id| format!("connection:{id}"));
            match current_epoch.is_some() {
                true => {
                    self.execute_drop(
                        "UPDATE schema_migration_locks SET owner_id = ?, session_id_redacted = ?, \
                         lock_strategy = 'mysql_get_lock_session', \
                         acquired_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                         last_observed_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                         run_id = ?, lock_epoch = ?, state = 'observed_active' \
                         WHERE scope = 'database'",
                        params![
                            lock.owner_id,
                            session_label,
                            lock.now,
                            lock.now,
                            lock.run_id,
                            next_epoch
                        ],
                        "could not update MySQL lock metadata",
                    )
                    .await?;
                }
                false => {
                    self.execute_drop(
                        "INSERT INTO schema_migration_locks \
                         (scope, owner_id, session_id_redacted, lock_strategy, acquired_at, \
                          last_observed_at, run_id, lock_epoch, state) \
                         VALUES ('database', ?, ?, 'mysql_get_lock_session', \
                                 STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), \
                                 STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ'), ?, ?, 'observed_active')",
                        params![
                            lock.owner_id,
                            session_label,
                            lock.now,
                            lock.now,
                            lock.run_id,
                            next_epoch
                        ],
                        "could not insert MySQL lock metadata",
                    )
                    .await?;
                }
            }
            Ok(next_epoch)
        })
    }

    fn release_recorded_lock<'a>(
        &'a mut self,
        owner_id: &'a str,
        released_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            if self.held_lock.is_none() {
                return Err(DbmError::Safety(
                    "MySQL lock metadata requires the session GET_LOCK".to_owned(),
                ));
            }
            let changed = self
                .execute_drop(
                    "UPDATE schema_migration_locks SET state = 'released', run_id = NULL, \
                     last_observed_at = STR_TO_DATE(?, '%Y-%m-%dT%H:%i:%s.%fZ') \
                     WHERE scope = 'database' AND owner_id = ? AND state = 'observed_active'",
                    params![released_at, owner_id],
                    "could not release MySQL lock metadata",
                )
                .await?;
            if changed != 1 {
                return Err(DbmError::Lock(
                    "MySQL lock metadata could not confirm release for this owner".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn verify_session<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.verify_session_inner().await })
    }

    fn acquire_lock<'a>(&'a mut self, timeout: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if self.held_lock.is_some() {
                return Err(DbmError::Lock(
                    "MySQL session already owns a named lock".to_owned(),
                ));
            }
            let database = self.info.database.as_deref().ok_or_else(|| {
                driver_error("session has no selected database for named locking")
            })?;
            let lock_name = mysql_lock_name(database);
            let seconds = i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX);
            self.verify_session_inner().await?;
            let result = self
                .query_first::<(Option<i64>,)>(
                    "SELECT GET_LOCK(?, ?)",
                    params![lock_name.clone(), seconds],
                    "MySQL named-lock acquisition lost its pinned session",
                )
                .await?
                .ok_or_else(|| driver_error("MySQL returned no named-lock result"))?
                .0;
            match result {
                Some(1) => {
                    self.held_lock = Some(lock_name);
                    self.verify_session_inner().await
                }
                Some(0) => Err(DbmError::Lock(
                    "timed out waiting for the MySQL session named lock".to_owned(),
                )),
                Some(_) | None => Err(DbmError::Lock(
                    "MySQL did not confirm acquisition of the session named lock".to_owned(),
                )),
            }
        })
    }

    fn release_lock<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let lock_name = self.held_lock.clone().ok_or_else(|| {
                DbmError::Lock("MySQL session does not own a named lock".to_owned())
            })?;
            self.verify_session_inner().await?;
            let result = self
                .query_first::<(Option<i64>,)>(
                    "SELECT RELEASE_LOCK(?)",
                    params![lock_name],
                    "MySQL named-lock release could not be confirmed",
                )
                .await?
                .ok_or_else(|| driver_error("MySQL returned no named-lock release result"))?
                .0;
            if result != Some(1) {
                return Err(DbmError::Lock(
                    "MySQL did not confirm release of the session named lock".to_owned(),
                ));
            }
            self.held_lock = None;
            Ok(())
        })
    }

    fn begin<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if self.transaction_open {
                return Err(DbmError::Safety(
                    "MySQL session already has an open transaction".to_owned(),
                ));
            }
            self.verify_session_inner().await?;
            self.connection()?
                .query_drop("START TRANSACTION")
                .await
                .map_err(|_| driver_error("could not start a MySQL metadata transaction"))?;
            self.transaction_open = true;
            Ok(())
        })
    }

    fn execute_statement<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            crate::migration::validate_mysql_migration_statement(sql)?;
            self.execute_drop(sql, Params::Empty, "MySQL statement failed")
                .await?;
            Ok(())
        })
    }

    fn execute_nontransactional_statement<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if self.held_lock.is_none() || self.transaction_open {
                return Err(DbmError::Safety(
                    "MySQL non-transactional statements require the session lock and no open transaction"
                        .to_owned(),
                ));
            }
            crate::migration::validate_mysql_migration_statement(sql)?;
            let statements = crate::migration::split_sql_statements_for_dialect(
                sql,
                crate::migration::SqlDialect::Mysql,
            );
            if statements.len() != 1 {
                return Err(DbmError::Safety(
                    "MySQL execution accepts exactly one parsed statement per journal step"
                        .to_owned(),
                ));
            }
            self.verify_session_inner().await?;
            match self.connection()?.query_drop(sql).await {
                Ok(()) => {}
                Err(error) if error.is_fatal() => {
                    return Err(DbmError::ReconciliationRequired(
                        "MySQL statement response was not confirmed because the pinned session was lost; inspect database effects before reconciliation"
                            .to_owned(),
                    ));
                }
                Err(_) => {
                    return Err(DbmError::Execution(
                        "MySQL rejected the non-transactional statement; inspect its effects and reconcile explicitly"
                            .to_owned(),
                    ));
                }
            }
            self.verify_session_inner().await
        })
    }

    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if !self.transaction_open {
                return Err(DbmError::Safety(
                    "MySQL session has no open transaction to commit".to_owned(),
                ));
            }
            self.verify_session_inner().await?;
            self.connection()?.query_drop("COMMIT").await.map_err(|_| {
                DbmError::ReconciliationRequired(
                    "MySQL transaction commit response could not be confirmed".to_owned(),
                )
            })?;
            self.transaction_open = false;
            Ok(())
        })
    }

    fn rollback<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if !self.transaction_open {
                return Ok(());
            }
            self.connection()?
                .query_drop("ROLLBACK")
                .await
                .map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "MySQL rollback response could not be confirmed".to_owned(),
                    )
                })?;
            self.transaction_open = false;
            Ok(())
        })
    }

    fn close<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Ok(());
            }
            let mut close_error = None;
            if self.transaction_open
                && let Err(error) = self.rollback().await
            {
                close_error = Some(error);
            }
            if self.held_lock.is_some()
                && let Err(error) = self.release_lock().await
                && close_error.is_none()
            {
                close_error = Some(error);
            }
            self.closed = true;
            if let Some(connection) = self.connection.take()
                && connection.disconnect().await.is_err()
                && close_error.is_none()
            {
                close_error = Some(driver_error("could not close the MySQL session cleanly"));
            }
            match close_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DatabaseVendor {
    MySql,
    MariaDb,
}

impl DatabaseVendor {
    fn label(self) -> &'static str {
        match self {
            Self::MySql => "MySQL",
            Self::MariaDb => "MariaDB",
        }
    }
}

async fn prepare_connection_options(
    connection_string: &str,
    config: &LoadedConfig,
) -> Result<Opts> {
    let opts = Opts::from_url(connection_string).map_err(|_| {
        DbmError::Configuration(
            "MySQL connection settings are invalid; the connection value is redacted".to_owned(),
        )
    })?;
    if opts.db_name().is_none_or(str::is_empty) {
        return Err(DbmError::Configuration(
            "MySQL connection URL must select a database explicitly".to_owned(),
        ));
    }
    if opts.enable_cleartext_plugin() {
        return Err(DbmError::Safety(
            "MySQL cleartext-password authentication is not supported".to_owned(),
        ));
    }
    if opts.local_infile_handler().is_some() {
        return Err(DbmError::Safety(
            "MySQL LOCAL INFILE handlers are not supported for migration connections".to_owned(),
        ));
    }

    let configured_ca = match config.config.target.tls_ca_cert_env.as_deref() {
        Some(variable_name) => {
            let path = std::env::var(variable_name).map_err(|_| {
                DbmError::Configuration(format!(
                    "MySQL CA certificate path variable {variable_name} is not set or not valid Unicode"
                ))
            })?;
            if path.trim().is_empty() || !std::path::Path::new(&path).is_file() {
                return Err(DbmError::Configuration(
                    "configured MySQL CA certificate file is missing or invalid".to_owned(),
                ));
            }
            Some(PathBuf::from(path))
        }
        None => None,
    };

    let original_ssl_opts = opts.ssl_opts().cloned();
    let secure_transport_requested = original_ssl_opts.is_some() || configured_ca.is_some();
    if let Some(ssl_opts) = original_ssl_opts.as_ref()
        && (ssl_opts.skip_domain_validation() || ssl_opts.accept_invalid_certs())
    {
        return Err(DbmError::Safety(
            "MySQL TLS certificate and hostname validation cannot be disabled".to_owned(),
        ));
    }
    if configured_ca.is_none()
        && original_ssl_opts
            .as_ref()
            .is_some_and(SslOpts::disable_built_in_roots)
    {
        return Err(DbmError::Safety(
            "MySQL TLS disables built-in roots but no explicit CA file is configured".to_owned(),
        ));
    }

    let insecure_local_opt_in = config.config.target.allow_insecure_transport
        && config.config.target.environment != Environment::Production
        && is_local_mysql_host(&opts);
    if !secure_transport_requested && !insecure_local_opt_in {
        let mut ssl_opts = SslOpts::default();
        if let Some(ca_path) = configured_ca.as_ref() {
            ssl_opts = ssl_opts
                .with_root_certs(vec![ca_path.clone().into()])
                .with_disable_built_in_roots(true);
        }
        let secured = OptsBuilder::from_opts(opts)
            .prefer_socket(Some(false))
            .ssl_opts(Some(ssl_opts));
        return Ok(secured.into());
    }

    let mut builder = OptsBuilder::from_opts(opts).prefer_socket(Some(false));
    if secure_transport_requested {
        let mut ssl_opts = original_ssl_opts.unwrap_or_default();
        if let Some(ca_path) = configured_ca {
            ssl_opts = ssl_opts
                .with_root_certs(vec![ca_path.into()])
                .with_disable_built_in_roots(true);
        }
        builder = builder.ssl_opts(Some(ssl_opts));
    }
    Ok(builder.into())
}

fn is_local_mysql_host(opts: &Opts) -> bool {
    let hostname = opts.ip_or_hostname();
    if hostname.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if IpAddr::from_str(hostname).is_ok_and(|address| address.is_loopback()) {
        return true;
    }
    opts.resolved_ips()
        .as_ref()
        .is_some_and(|addresses| !addresses.is_empty() && addresses.iter().all(IpAddr::is_loopback))
}

fn detect_vendor(version: &str, comment: &str) -> Result<DatabaseVendor> {
    if version.to_ascii_lowercase().contains("mariadb")
        || comment.to_ascii_lowercase().contains("mariadb")
    {
        Ok(DatabaseVendor::MariaDb)
    } else if !version.is_empty() {
        Ok(DatabaseVendor::MySql)
    } else {
        Err(driver_error("database server returned an empty version"))
    }
}

fn ensure_supported_server_version(vendor: DatabaseVendor, version: &str) -> Result<()> {
    let normalized = if vendor == DatabaseVendor::MariaDb && version.starts_with("5.5.5-") {
        version.trim_start_matches("5.5.5-")
    } else {
        version
    };
    let mut numbers = normalized
        .split(|character: char| !character.is_ascii_digit())
        .filter(|component| !component.is_empty())
        .filter_map(|component| component.parse::<u32>().ok());
    let major = numbers.next();
    let minor = numbers.next();
    let supported = matches!(
        (vendor, major, minor),
        (DatabaseVendor::MySql, Some(8), Some(4))
            | (DatabaseVendor::MariaDb, Some(10), Some(11))
            | (DatabaseVendor::MariaDb, Some(11), Some(4))
            | (DatabaseVendor::MariaDb, Some(11), Some(8))
    );
    if !supported {
        return Err(DbmError::Unsupported(format!(
            "{} server version {version} is outside the initial F5 support matrix",
            vendor.label()
        )));
    }
    Ok(())
}

fn mysql_lock_name(database: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dbm-mysql-named-lock-v1\0");
    digest.update((database.len() as u64).to_be_bytes());
    digest.update(database.as_bytes());
    let digest = digest.finalize();
    let mut name = String::with_capacity(64);
    name.push_str("dbm:");
    for byte in digest.iter().take(30) {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn row_value<T>(row: &Row, index: usize, label: &str) -> Result<T>
where
    T: FromValue,
{
    row.get(index).ok_or_else(|| metadata_row_error(label))
}

fn normalized_check_clause(clause: &str) -> String {
    let lower = clause.to_ascii_lowercase();
    let without_charset_introducers = lower
        .replace("_utf8mb4", "")
        .replace("_utf8", "")
        .replace("_ascii", "")
        .replace("_latin1", "");
    without_charset_introducers
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(char::from)
        .collect()
}

fn query_error(
    error: mysql_async::Error,
    operation: &str,
    state_may_be_uncertain: bool,
) -> DbmError {
    match error {
        mysql_async::Error::Server(server_error) => DbmError::Driver {
            driver: "MySQL/MariaDB",
            message: format!(
                "{operation} (server error {}, SQLSTATE {}); server details are redacted",
                server_error.code, server_error.state
            ),
        },
        error if state_may_be_uncertain && error.is_fatal() => DbmError::ReconciliationRequired(
            format!("MySQL session was lost while {operation}; server outcome is unknown"),
        ),
        _ => driver_error(operation),
    }
}

fn driver_error(operation: &str) -> DbmError {
    DbmError::Driver {
        driver: "MySQL/MariaDB",
        message: format!("{operation}; server details are redacted"),
    }
}

fn metadata_row_error(label: &str) -> DbmError {
    DbmError::Diverged(format!("MySQL returned invalid metadata {label}"))
}

fn legacy_row_error(tool: &str, field: &str) -> DbmError {
    DbmError::InvalidMigration {
        path: format!("legacy:{tool}:{field}"),
        message: "legacy history contains a value with an unsupported type".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Conn, DatabaseVendor, MySqlAdapter, MySqlSession, Opts, OptsBuilder,
        ensure_supported_server_version, is_local_mysql_host, mysql_lock_name,
        normalized_check_clause, prepare_connection_options,
    };
    use crate::config::{Config, Environment, ExecutionConfig, LoadedConfig, TargetConfig};
    use crate::database::{DatabaseAdapter, DatabaseSession, SessionIntent};
    use crate::error::DbmError;
    use crate::legacy::{LegacyImportWrite, LegacyTool};

    fn test_config(
        environment: Environment,
        allow_insecure_transport: bool,
        tls_ca_cert_env: Option<&str>,
    ) -> LoadedConfig {
        LoadedConfig {
            config: Config {
                version: 1,
                namespace: "default".to_owned(),
                source: vec!["migrations".into()],
                target: TargetConfig {
                    driver: "mysql".to_owned(),
                    database: None,
                    url_env: Some("DBM_TEST_MYSQL_URL".to_owned()),
                    tls_ca_cert_env: tls_ca_cert_env.map(str::to_owned),
                    allow_insecure_transport,
                    environment,
                },
                execution: ExecutionConfig::default(),
                ai: crate::config::AiConfig::default(),
            },
            source_dirs: Vec::new(),
            database_path: None,
        }
    }

    async fn check_constraint_name(
        connection: &mut Conn,
        table_name: &str,
        expected_clause: &str,
    ) -> String {
        use mysql_async::prelude::Queryable;

        assert!(
            table_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        let query = format!(
            "SELECT table_constraints.CONSTRAINT_NAME, check_constraints.CHECK_CLAUSE \
             FROM information_schema.TABLE_CONSTRAINTS AS table_constraints \
             INNER JOIN information_schema.CHECK_CONSTRAINTS AS check_constraints \
                ON check_constraints.CONSTRAINT_SCHEMA = table_constraints.CONSTRAINT_SCHEMA \
               AND check_constraints.CONSTRAINT_NAME = table_constraints.CONSTRAINT_NAME \
             WHERE table_constraints.CONSTRAINT_SCHEMA = DATABASE() \
               AND table_constraints.TABLE_NAME = '{table_name}' \
               AND table_constraints.CONSTRAINT_TYPE = 'CHECK'"
        );
        let rows: Vec<(String, String)> = connection
            .query(query)
            .await
            .expect("inspect metadata check constraints");
        let mut matches = Vec::new();
        for (name, clause) in rows {
            if normalized_check_clause(&clause) == expected_clause {
                matches.push(name);
            }
        }
        assert_eq!(matches.len(), 1, "expected one {expected_clause} check");
        let name = matches.remove(0);
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        name
    }

    #[test]
    fn mysql_named_lock_is_deterministic_and_within_server_limit() {
        let first = mysql_lock_name("app_database");
        assert_eq!(first, mysql_lock_name("app_database"));
        assert_ne!(first, mysql_lock_name("other_database"));
        assert!(first.is_ascii());
        assert!(first.len() <= 64);
    }

    #[test]
    fn supports_only_the_declared_mysql_and_mariadb_lines() {
        assert!(ensure_supported_server_version(DatabaseVendor::MySql, "8.4.5").is_ok());
        assert!(
            ensure_supported_server_version(DatabaseVendor::MariaDb, "10.11.14-MariaDB").is_ok()
        );
        assert!(
            ensure_supported_server_version(DatabaseVendor::MariaDb, "5.5.5-11.4.13-MariaDB")
                .is_ok()
        );
        assert!(ensure_supported_server_version(DatabaseVendor::MariaDb, "11.8.2-MariaDB").is_ok());
        assert!(ensure_supported_server_version(DatabaseVendor::MySql, "8.0.42").is_err());
        assert!(
            ensure_supported_server_version(DatabaseVendor::MariaDb, "10.6.23-MariaDB").is_err()
        );
    }

    #[test]
    fn normalizes_supported_metadata_check_clauses_without_matching_looser_rules() {
        assert_eq!(
            normalized_check_clause("(`kind` = _utf8mb4'versioned')"),
            "kindversioned"
        );
        assert_eq!(
            normalized_check_clause("(`authoring_type` = 'sql')"),
            "authoringtypesql"
        );
        assert_eq!(
            normalized_check_clause("(`metadata_format_version` = 3)"),
            "metadataformatversion3"
        );
        assert_ne!(
            normalized_check_clause("kind = 'versioned' OR 1 = 1"),
            "kindversioned"
        );
    }

    #[tokio::test]
    async fn requires_tls_except_for_explicit_local_nonproduction_opt_in() {
        let default_config = test_config(Environment::Test, false, None);
        let default_options =
            prepare_connection_options("mysql://root@127.0.0.1/test_db", &default_config)
                .await
                .expect("prepare secure options");
        assert!(default_options.ssl_opts().is_some());

        let local_config = test_config(Environment::Test, true, None);
        let local_options =
            prepare_connection_options("mysql://root@127.0.0.1/test_db", &local_config)
                .await
                .expect("prepare explicit local plaintext options");
        assert!(local_options.ssl_opts().is_none());

        let remote_options =
            prepare_connection_options("mysql://root@db.example.invalid/test_db", &local_config)
                .await
                .expect("remote opt-in still uses TLS");
        assert!(remote_options.ssl_opts().is_some());
    }

    #[tokio::test]
    async fn rejects_mysql_urls_that_disable_certificate_validation() {
        let config = test_config(Environment::Test, false, None);
        let result = prepare_connection_options(
            "mysql://root@127.0.0.1/test_db?require_ssl=true&verify_identity=false",
            &config,
        )
        .await;

        assert!(matches!(result, Err(DbmError::Safety(_))));
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL/MariaDB database"]
    async fn local_mysql_metadata_v3_and_v4_upgrades_preserve_native_rows_and_run_journal() {
        use mysql_async::prelude::Queryable;

        let config = test_config(Environment::Test, true, None);
        let connection_string = std::env::var("DBM_TEST_MYSQL_URL")
            .expect("DBM_TEST_MYSQL_URL must identify a disposable local database");
        let options = prepare_connection_options(&connection_string, &config)
            .await
            .expect("prepare local MySQL/MariaDB integration connection");
        assert!(
            is_local_mysql_host(&options),
            "integration target must be loopback"
        );
        let mut connection = mysql_async::Conn::new(options)
            .await
            .expect("connect to disposable local MySQL/MariaDB database");
        let reserved_tables: u64 = connection
            .query_first(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name IN (\
                    'schema_migration_meta', 'schema_migrations', \
                    'schema_migration_runs', 'schema_migration_steps', \
                    'schema_migration_locks', 'schema_migration_imports', \
                    'schema_migration_import_events', 'schema_migration_adoptions')",
            )
            .await
            .expect("inspect test target metadata tables")
            .expect("metadata table count row");
        assert_eq!(
            reserved_tables, 0,
            "test target must start without DBM metadata"
        );

        let adapter = MySqlAdapter;
        let mut session = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect DBM MySQL/MariaDB adapter");
        assert!(
            !session
                .read_snapshot("default")
                .await
                .expect("read empty test target")
                .initialized
        );
        session
            .acquire_lock(std::time::Duration::from_secs(2))
            .await
            .expect("acquire MySQL/MariaDB named lock");
        session.begin().await.expect("begin metadata bootstrap");
        session
            .initialize_metadata("2026-10-24T12:00:00.000000Z")
            .await
            .expect("bootstrap format 4 metadata");
        session.commit().await.expect("commit metadata bootstrap");
        session
            .release_lock()
            .await
            .expect("release bootstrap session lock");
        session.close().await.expect("close bootstrap session");

        connection
            .query_drop(
                "INSERT INTO schema_migrations \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES ('default', REPEAT('a', 64), 'versioned', \
                         '20261024120000000000', '20261024120000000000_probe.sql', \
                         'migrations/versioned/20261024120000000000_probe.sql', \
                         'probe', 'sql', 'sha256:fixture', 1, CURRENT_TIMESTAMP(6))",
            )
            .await
            .expect("seed native migration row");
        connection
            .query_drop(
                "INSERT INTO schema_migration_runs \
                 (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                  state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
                 VALUES ('upgrade-run', 'upgrade-batch', 'default', REPEAT('a', 64), \
                         '20261024120000000000', 'up', 'succeeded', 1, CURRENT_TIMESTAMP(6), \
                         'sha256:fixture', 'sha256:plan', 'human')",
            )
            .await
            .expect("seed migration run");
        connection
            .query_drop(
                "INSERT INTO schema_migration_steps \
                 (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                  transaction_class, state, started_at, attempt_id) \
                 VALUES ('upgrade-run', 0, REPEAT('b', 64), 'up', 'sha256:step', 'CREATE', \
                         'transactional', 'acknowledged', CURRENT_TIMESTAMP(6), 'upgrade-attempt')",
            )
            .await
            .expect("seed migration journal step");

        connection
            .query_drop("DROP TABLE schema_migration_adoptions")
            .await
            .expect("drop v4 adoption table in isolated fixture");
        let direction_constraint = check_constraint_name(
            &mut connection,
            "schema_migration_runs",
            "directioninupdownrepeatreconcile",
        )
        .await;
        connection
            .query_drop(
                format!(
                    "ALTER TABLE schema_migration_runs \
                        DROP CONSTRAINT `{direction_constraint}`, \
                        MODIFY COLUMN migration_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
                        ADD CONSTRAINT schema_migration_runs_direction_check \
                            CHECK (direction IN ('up', 'down', 'reconcile'))"
                ),
            )
            .await
            .expect("restore the v3 run direction and version contract");
        connection
            .query_drop("DROP TABLE schema_migration_import_events")
            .await
            .expect("drop v4 event table in isolated fixture");
        connection
            .query_drop(
                "CREATE TABLE schema_migrations_v3 (\
                    namespace VARCHAR(128) NOT NULL,\
                    migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    kind VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    native_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    filename LONGTEXT NOT NULL,\
                    source_path LONGTEXT NOT NULL,\
                    description LONGTEXT NOT NULL,\
                    authoring_type VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    content_checksum VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    apply_order BIGINT NOT NULL,\
                    applied_at DATETIME(6) NOT NULL,\
                    PRIMARY KEY (namespace, migration_key),\
                    UNIQUE KEY schema_migrations_namespace_version_uq (namespace, kind, native_version),\
                    CONSTRAINT schema_migrations_v3_kind_check CHECK (kind = 'versioned'),\
                    CONSTRAINT schema_migrations_v3_authoring_check CHECK (authoring_type = 'sql')\
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
            )
            .await
            .expect("create v3 migration table fixture");
        connection
            .query_drop(
                "INSERT INTO schema_migrations_v3 \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 SELECT namespace, migration_key, kind, native_version, filename, source_path, \
                        description, authoring_type, content_checksum, apply_order, applied_at \
                 FROM schema_migrations WHERE kind = 'versioned'",
            )
            .await
            .expect("copy native state into v3 fixture");
        connection
            .query_drop("DROP TABLE schema_migrations")
            .await
            .expect("replace v4 migration table with v3 fixture");
        connection
            .query_drop("RENAME TABLE schema_migrations_v3 TO schema_migrations")
            .await
            .expect("install v3 migration table fixture");
        connection
            .query_drop("DROP TABLE schema_migration_imports")
            .await
            .expect("drop v4 import table in isolated fixture");
        connection
            .query_drop(
                "CREATE TABLE schema_migration_meta_v3 (\
                    singleton TINYINT UNSIGNED NOT NULL PRIMARY KEY CHECK (singleton = 1),\
                    metadata_format_version INT NOT NULL,\
                    installation_id CHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    initialized_at DATETIME(6) NOT NULL,\
                    upgraded_at DATETIME(6) NULL,\
                    CONSTRAINT schema_migration_meta_v3_format_check CHECK (metadata_format_version = 3)\
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
            )
            .await
            .expect("create v3 metadata identity fixture");
        connection
            .query_drop(
                "INSERT INTO schema_migration_meta_v3 \
                 (singleton, metadata_format_version, installation_id, initialized_at, upgraded_at) \
                 SELECT singleton, 3, installation_id, initialized_at, upgraded_at \
                 FROM schema_migration_meta",
            )
            .await
            .expect("copy metadata identity into v3 fixture");
        connection
            .query_drop("DROP TABLE schema_migration_meta")
            .await
            .expect("replace v4 metadata identity with v3 fixture");
        connection
            .query_drop("RENAME TABLE schema_migration_meta_v3 TO schema_migration_meta")
            .await
            .expect("install v3 metadata identity fixture");

        let mut before_session = adapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect to inspect v3 metadata");
        let before = before_session
            .read_snapshot("default")
            .await
            .expect("read v3 metadata fixture");
        assert_eq!(before.metadata_format_version, Some(3));
        assert_eq!(before.applied.len(), 1);
        assert_eq!(before.runs.len(), 1);
        assert_eq!(before.steps.len(), 1);
        before_session
            .close()
            .await
            .expect("close pre-upgrade inspection session");

        let preview = crate::engine::upgrade_metadata_async(&config, false)
            .await
            .expect("preview explicit MySQL/MariaDB metadata upgrade");
        assert_eq!(preview.status, "requires_approval");
        assert!(preview.read_only);
        assert_eq!(preview.from_version, Some(3));

        let report = crate::engine::upgrade_metadata_async(&config, true)
            .await
            .expect("upgrade MySQL/MariaDB metadata through the engine");
        assert_eq!(report.status, "upgraded");
        assert_eq!(report.runs_preserved, 1);
        let mut session = adapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect to inspect upgraded metadata");
        let after = session
            .read_snapshot("default")
            .await
            .expect("read upgraded metadata");
        assert_eq!(
            after.metadata_format_version,
            Some(crate::storage::METADATA_FORMAT_VERSION)
        );
        assert_eq!(after.applied.len(), 1);
        assert_eq!(after.runs.len(), 1);
        assert_eq!(after.steps.len(), 1);
        assert_eq!(after.applied[0].version, "20261024120000000000");
        assert_eq!(after.runs[0].run_id, "upgrade-run");
        session.close().await.expect("close post-upgrade session");

        let kind_check = check_constraint_name(
            &mut connection,
            "schema_migrations",
            "kindinversionedrepeatablelegacy",
        )
        .await;
        let shape_check = check_constraint_name(
            &mut connection,
            "schema_migrations",
            "kindversionedandnativeversionisnotnullandrepeatablenameisnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindrepeatableandnativeversionisnullandrepeatablenameisnotnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindlegacyandnativeversionisnullandrepeatablenameisnullandoriginalversionisnotnullandlegacyimportidisnotnullandorigintoolisnotnullandorigindatabaseidentityisnotnullandoriginidentityisnotnullandverificationstatusisnotnullandobservedatisnotnull",
        )
        .await;
        let repeatable_name_check = check_constraint_name(
            &mut connection,
            "schema_migrations",
            "repeatablenameisnullorcharlengthrepeatablenamebetween1and128",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migrations \
                    DROP CONSTRAINT `{kind_check}`, \
                    DROP CONSTRAINT `{shape_check}`, \
                    DROP CONSTRAINT `{repeatable_name_check}`, \
                    DROP INDEX schema_migrations_namespace_repeatable_uq, \
                    DROP COLUMN repeatable_name, \
                    ADD CONSTRAINT schema_migrations_kind_check \
                        CHECK (kind IN ('versioned', 'legacy')), \
                    ADD CONSTRAINT schema_migrations_legacy_shape_check CHECK (\
                        (kind = 'versioned' AND native_version IS NOT NULL \
                         AND original_version IS NULL AND legacy_import_id IS NULL \
                         AND origin_tool IS NULL) OR \
                        (kind = 'legacy' AND native_version IS NULL \
                         AND original_version IS NOT NULL AND legacy_import_id IS NOT NULL \
                         AND origin_tool IS NOT NULL AND origin_database_identity IS NOT NULL \
                         AND origin_identity IS NOT NULL AND verification_status IS NOT NULL \
                         AND observed_at IS NOT NULL))"
            ))
            .await
            .expect("restore the v4 migration table contract");

        let direction_check = check_constraint_name(
            &mut connection,
            "schema_migration_runs",
            "directioninupdownrepeatreconcile",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migration_runs \
                    DROP CONSTRAINT `{direction_check}`, \
                    MODIFY COLUMN migration_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
                    ADD CONSTRAINT schema_migration_runs_direction_check \
                        CHECK (direction IN ('up', 'down', 'reconcile'))"
            ))
            .await
            .expect("restore the v4 run journal contract");

        let metadata_format_check = check_constraint_name(
            &mut connection,
            "schema_migration_meta",
            "metadataformatversion5",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migration_meta DROP CONSTRAINT `{metadata_format_check}`"
            ))
            .await
            .expect("remove the v5-only metadata format check");
        connection
            .query_drop(
                "UPDATE schema_migration_meta SET metadata_format_version = 4 WHERE singleton = 1",
            )
            .await
            .expect("set the v4 metadata identity");
        connection
            .query_drop(
                "ALTER TABLE schema_migration_meta \
                    ADD CONSTRAINT schema_migration_meta_format_check \
                    CHECK (metadata_format_version = 4)",
            )
            .await
            .expect("restore the v4 metadata format check");

        let preview_v4 = crate::engine::upgrade_metadata_async(&config, false)
            .await
            .expect("preview explicit v4-to-v5 metadata upgrade");
        assert_eq!(preview_v4.status, "requires_approval");
        assert!(preview_v4.read_only);
        assert_eq!(preview_v4.from_version, Some(4));
        let report_v4 = crate::engine::upgrade_metadata_async(&config, true)
            .await
            .expect("upgrade format 4 metadata through the engine");
        assert_eq!(report_v4.status, "upgraded");
        assert_eq!(report_v4.from_version, Some(4));
        assert_eq!(report_v4.runs_preserved, 1);

        let mut session_v4 = adapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect to inspect direct v4 upgrade");
        let after_v4 = session_v4
            .read_snapshot("default")
            .await
            .expect("read direct v4 upgrade result");
        assert_eq!(
            after_v4.metadata_format_version,
            Some(crate::storage::METADATA_FORMAT_VERSION)
        );
        assert_eq!(after_v4.applied.len(), 1);
        assert_eq!(after_v4.runs.len(), 1);
        assert_eq!(after_v4.steps.len(), 1);
        assert_eq!(after_v4.applied[0].version, "20261024120000000000");
        assert_eq!(after_v4.runs[0].run_id, "upgrade-run");
        session_v4
            .close()
            .await
            .expect("close direct v4 inspection");

        for cleanup in [
            "DROP TABLE IF EXISTS schema_migration_steps",
            "DROP TABLE IF EXISTS schema_migration_locks",
            "DROP TABLE IF EXISTS schema_migrations",
            "DROP TABLE IF EXISTS schema_migration_adoptions",
            "DROP TABLE IF EXISTS schema_migration_import_events",
            "DROP TABLE IF EXISTS schema_migration_imports",
            "DROP TABLE IF EXISTS schema_migration_runs",
            "DROP TABLE IF EXISTS schema_migration_meta",
        ] {
            connection
                .query_drop(cleanup)
                .await
                .expect("remove only the DBM tables created by this isolated test");
        }
        connection
            .disconnect()
            .await
            .expect("close disposable MySQL/MariaDB connection");
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL or MariaDB database"]
    async fn local_mysql_legacy_import_adapters_preserve_knex_goose_and_sqlx_history() {
        use mysql_async::prelude::Queryable;

        let config = test_config(Environment::Test, true, None);
        let connection_string = std::env::var("DBM_TEST_MYSQL_URL")
            .expect("DBM_TEST_MYSQL_URL must identify a disposable local database");
        let options = prepare_connection_options(&connection_string, &config)
            .await
            .expect("prepare local MySQL/MariaDB integration connection");
        assert!(
            is_local_mysql_host(&options),
            "integration target must be loopback"
        );
        let database = format!("dbm_legacy_{}", uuid::Uuid::new_v4().simple());
        let mut setup_connection = Conn::new(options.clone())
            .await
            .expect("connect to disposable local MySQL/MariaDB database");
        setup_connection
            .query_drop(format!(
                "CREATE DATABASE `{database}` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
            ))
            .await
            .expect("create isolated legacy import database");

        let result = async {
            let isolated_options =
                Opts::from(OptsBuilder::from_opts(options).db_name(Some(database.as_str())));
            let mut session =
                MySqlSession::connect_from_options(isolated_options, SessionIntent::ReadWrite)
                    .await?;

            let fixture_statements = [
                "CREATE TABLE knex_migrations (\
                     id BIGINT PRIMARY KEY, name LONGTEXT NOT NULL, batch BIGINT NOT NULL, \
                     migration_time DATETIME(6) NULL\
                 ) ENGINE=InnoDB",
                "CREATE TABLE knex_migrations_lock (\
                     `index` BIGINT PRIMARY KEY, is_locked TINYINT NOT NULL\
                 ) ENGINE=InnoDB",
                "INSERT INTO knex_migrations VALUES \
                     (1, '001_create_legacy_users.sql', 3, '2026-10-01 00:00:00.000000'),\
                     (2, '002_create_legacy_orders.sql', 4, '2026-10-02 00:00:00.000000')",
                "INSERT INTO knex_migrations_lock VALUES (1, 0)",
                "CREATE TABLE goose_db_version (\
                     id BIGINT PRIMARY KEY, version_id BIGINT NOT NULL, \
                     is_applied BOOLEAN NOT NULL, tstamp DATETIME(6) NULL\
                 ) ENGINE=InnoDB",
                "INSERT INTO goose_db_version VALUES \
                     (1, 0, TRUE, '2026-10-01 00:00:00.000000'),\
                     (2, 100, TRUE, '2026-10-01 00:01:00.000000'),\
                     (3, 200, TRUE, '2026-10-01 00:02:00.000000'),\
                     (4, 100, FALSE, '2026-10-01 00:03:00.000000')",
                "CREATE TABLE _sqlx_migrations (\
                     version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
                     installed_on DATETIME(6) NULL, success BOOLEAN NOT NULL, \
                     checksum BINARY(48) NOT NULL, execution_time BIGINT NOT NULL\
                 ) ENGINE=InnoDB",
                "INSERT INTO _sqlx_migrations VALUES \
                     (42, 'create_accounts', '2026-10-03 00:00:00.000000', TRUE, \
                      UNHEX(REPEAT('ab', 48)), 19)",
            ];
            for statement in fixture_statements {
                session
                    .connection()?
                    .query_drop(statement)
                    .await
                    .map_err(|_| super::driver_error("could not create legacy fixture tables"))?;
            }

            session
                .acquire_lock(std::time::Duration::from_secs(2))
                .await?;
            session.begin().await?;
            session
                .initialize_metadata("2026-10-25T12:00:00.000000Z")
                .await?;

            let fixtures = [
                (LegacyTool::Knex, 'a', "knex-import"),
                (LegacyTool::Goose, 'b', "goose-import"),
                (LegacyTool::Sqlx, 'c', "sqlx-import"),
            ];
            for (tool, digest_prefix, import_id) in fixtures {
                let source_identity = format!("sha256:{}", digest_prefix.to_string().repeat(64));
                let source = session
                    .inspect_legacy_history(tool, &source_identity)
                    .await?;
                if !source.blockers.is_empty() {
                    return Err(DbmError::Safety(
                        "clean MySQL/MariaDB import fixture unexpectedly contains blockers"
                            .to_owned(),
                    ));
                }
                session
                    .persist_legacy_import(LegacyImportWrite {
                        namespace: "default",
                        import_id,
                        tool,
                        plan_digest: &source.digest,
                        source: &source,
                        imported_at: "2026-10-25T12:00:00.000000Z",
                    })
                    .await?;
            }
            session.commit().await?;
            session.release_lock().await?;
            let snapshot = session.read_snapshot("default").await?;
            session.begin().await?;
            session
                .execute_statement(
                    "INSERT INTO _sqlx_migrations VALUES \
                     (43, 'create_invoices', NULL, FALSE, UNHEX(REPEAT('cd', 48)), 0)",
                )
                .await?;
            session.commit().await?;
            let failed_sqlx = session
                .inspect_legacy_history(LegacyTool::Sqlx, &format!("sha256:{}", "c".repeat(64)))
                .await?;
            assert!(
                failed_sqlx
                    .blockers
                    .iter()
                    .any(|blocker| blocker.code == "SQLX_MIGRATION_FAILED")
            );
            session.begin().await?;
            session
                .execute_statement(
                    "UPDATE knex_migrations_lock SET is_locked = 1 WHERE `index` = 1",
                )
                .await?;
            session.commit().await?;
            let active_knex_lock = session
                .inspect_legacy_history(LegacyTool::Knex, &format!("sha256:{}", "a".repeat(64)))
                .await?;
            if !active_knex_lock
                .blockers
                .iter()
                .any(|blocker| blocker.code == "LEGACY_LOCK_ACTIVE")
            {
                return Err(DbmError::Safety(
                    "active MySQL/MariaDB Knex lock did not block import inspection".to_owned(),
                ));
            }
            session.close().await?;
            Ok::<_, DbmError>(snapshot)
        }
        .await;

        let cleanup = setup_connection
            .query_drop(format!("DROP DATABASE `{database}`"))
            .await;
        assert!(cleanup.is_ok(), "isolated database cleanup must succeed");
        let snapshot = result.expect("MySQL/MariaDB legacy import adapter conformance");
        assert_eq!(snapshot.metadata_format_version, Some(4));
        assert_eq!(snapshot.legacy_imports.len(), 3);
        assert_eq!(snapshot.legacy_import_events.len(), 7);
        assert_eq!(snapshot.legacy_applied.len(), 4);
        let knex_migrations: Vec<_> = snapshot
            .legacy_applied
            .iter()
            .filter(|migration| migration.origin_tool == "knex")
            .collect();
        assert_eq!(knex_migrations.len(), 2);
        assert_eq!(
            knex_migrations[0].filename.as_deref(),
            Some("001_create_legacy_users.sql")
        );
        assert_eq!(knex_migrations[0].batch_id.as_deref(), Some("3"));
        assert_eq!(snapshot.legacy_import_events[0].state, "sentinel");
        let sqlx_migration = snapshot
            .legacy_applied
            .iter()
            .find(|migration| migration.origin_tool == "sqlx")
            .expect("SQLx current migration");
        let expected_checksum = "ab".repeat(48);
        assert_eq!(sqlx_migration.checksum_algorithm.as_deref(), Some("sha384"));
        assert_eq!(
            sqlx_migration.checksum_value.as_deref(),
            Some(expected_checksum.as_str())
        );
        assert!(
            snapshot
                .legacy_applied
                .iter()
                .all(|migration| migration.verification_status != "adopted")
        );
    }
}
