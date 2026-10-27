use std::io::Read;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{Client, Config as PostgresConfig, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::async_runtime::run_blocking;
use crate::config::{Environment, LoadedConfig};
use crate::error::{DbmError, Result};
use crate::identity::legacy_migration_key;
use crate::legacy::{
    GooseRow, KnexLock, KnexRow, LegacyAdoptionWrite, LegacyImportSnapshot, LegacyImportWrite,
    LegacyTool, SqlxRow, inspect_goose, inspect_knex, inspect_sqlx, parse_legacy_bool,
};
use crate::migration::{
    MigrationKind, PostgresNonTransactionalIndexOperation,
    parse_postgres_nontransactional_index_operation,
};
use crate::storage::{
    AppliedMigration, FinishedRunData, LegacyAppliedMigration, LegacyImportEventRecord,
    LegacyImportRecord, LockData, LockRecord, MetadataSnapshot, PlannedRunData, ReconciliationData,
    RunRecord, StepData, StepRecord,
};

use super::{BoxFuture, DatabaseAdapter, DatabaseInfo, DatabaseSession, SessionIntent};

const METADATA_TABLE_COUNT_QUERY: &str = "SELECT count(*)::bigint, \
    count(*) FILTER (WHERE tables.relkind IN ('r', 'p'))::bigint \
    FROM pg_catalog.pg_class AS tables \
    INNER JOIN pg_catalog.pg_namespace AS schemas \
        ON schemas.oid = tables.relnamespace \
    WHERE schemas.nspname = current_schema() \
      AND tables.relname IN (\
          'schema_migration_meta', 'schema_migrations', \
          'schema_migration_runs', 'schema_migration_steps', \
          'schema_migration_locks', 'schema_migration_imports', \
          'schema_migration_import_events', 'schema_migration_adoptions'\
      )";

const POSTGRES_METADATA_SCHEMA_DDL: &str = r#"
CREATE TABLE schema_migration_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    metadata_format_version INTEGER NOT NULL,
    installation_id TEXT NOT NULL,
    initialized_at TIMESTAMPTZ NOT NULL,
    upgraded_at TIMESTAMPTZ
);
CREATE TABLE schema_migration_imports (
    import_id TEXT PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),
    origin_tool TEXT NOT NULL CHECK (origin_tool IN ('knex', 'goose', 'sqlx')),
    source_identity TEXT NOT NULL,
    snapshot_digest TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL,
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    current_migration_count BIGINT NOT NULL CHECK (current_migration_count >= 0),
    UNIQUE (namespace, import_id)
);
CREATE TABLE schema_migration_import_events (
    import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),
    event_index BIGINT NOT NULL CHECK (event_index >= 0),
    event_id TEXT NOT NULL,
    original_version TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('applied', 'reverted', 'failed', 'sentinel')),
    filename TEXT,
    batch_id TEXT,
    applied_at TEXT,
    checksum_algorithm TEXT,
    checksum_value TEXT,
    execution_time_ms BIGINT CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),
    PRIMARY KEY (import_id, event_index)
);
CREATE TABLE schema_migration_adoptions (
    adoption_id TEXT PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),
    import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),
    legacy_migration_key CHAR(64) NOT NULL CHECK (length(legacy_migration_key) = 64),
    native_migration_key CHAR(64) NOT NULL CHECK (length(native_migration_key) = 64),
    native_version VARCHAR(20) NOT NULL CHECK (length(native_version) = 20),
    native_checksum TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    approved_by TEXT NOT NULL,
    reason TEXT NOT NULL,
    adopted_at TIMESTAMPTZ NOT NULL,
    UNIQUE (namespace, legacy_migration_key)
);
CREATE TABLE schema_migrations (
    namespace VARCHAR(128) NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),
    migration_key CHAR(64) NOT NULL CHECK (length(migration_key) = 64),
    kind TEXT NOT NULL CHECK (kind IN ('versioned', 'repeatable', 'legacy')),
    native_version VARCHAR(20) CHECK (native_version IS NULL OR length(native_version) = 20),
    repeatable_name VARCHAR(128),
    original_version TEXT,
    filename TEXT,
    source_path TEXT,
    description TEXT,
    authoring_type TEXT NOT NULL CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),
    content_checksum TEXT,
    origin_tool TEXT,
    origin_database_identity TEXT,
    origin_identity TEXT,
    legacy_import_id TEXT REFERENCES schema_migration_imports(import_id),
    batch_id TEXT,
    checksum_algorithm TEXT,
    checksum_value TEXT,
    execution_time_ms BIGINT CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),
    verification_status TEXT CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),
    apply_order BIGINT NOT NULL,
    applied_at TIMESTAMPTZ,
    origin_applied_at TEXT,
    observed_at TIMESTAMPTZ,
    PRIMARY KEY (namespace, migration_key),
    UNIQUE (namespace, kind, native_version),
    UNIQUE (namespace, kind, repeatable_name),
    CHECK (repeatable_name IS NULL OR length(repeatable_name) BETWEEN 1 AND 128),
    CHECK ((kind = 'versioned' AND native_version IS NOT NULL AND original_version IS NULL
             AND repeatable_name IS NULL AND legacy_import_id IS NULL AND origin_tool IS NULL) OR
           (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL AND original_version IS NULL
             AND legacy_import_id IS NULL AND origin_tool IS NULL) OR
           (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL
             AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL
             AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL
             AND verification_status IS NOT NULL AND observed_at IS NOT NULL))
);
CREATE TABLE schema_migration_runs (
    run_id TEXT PRIMARY KEY,
    batch_id TEXT NOT NULL,
    namespace VARCHAR(128) NOT NULL,
    migration_key CHAR(64) NOT NULL,
    migration_version VARCHAR(128) NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('up', 'down', 'repeat', 'reconcile')),
    state TEXT NOT NULL CHECK (state IN ('planned', 'started', 'succeeded', 'failed', 'indeterminate', 'reconciled')),
    attempt BIGINT NOT NULL CHECK (attempt >= 1),
    created_at TIMESTAMPTZ NOT NULL,
    started_at TIMESTAMPTZ,
    ended_at TIMESTAMPTZ,
    duration_ms BIGINT CHECK (duration_ms IS NULL OR duration_ms >= 0),
    checksum_at_execution TEXT NOT NULL,
    plan_hash TEXT NOT NULL,
    error_code TEXT,
    error_summary_redacted TEXT,
    reconciliation_result TEXT CHECK (reconciliation_result IS NULL OR reconciliation_result IN ('operation_applied', 'operation_not_applied')),
    reconciliation_plan TEXT,
    reconciliation_evidence_ref TEXT,
    reconciliation_reason TEXT,
    reconciled_by TEXT,
    reconciled_from_state TEXT,
    related_run_id TEXT REFERENCES schema_migration_runs(run_id),
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ci', 'embedded', 'agent_proposal'))
);
CREATE INDEX schema_migration_runs_namespace_created_idx ON
    schema_migration_runs(namespace, created_at, run_id);
CREATE TABLE schema_migration_steps (
    run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),
    step_index BIGINT NOT NULL CHECK (step_index >= 0),
    step_key TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),
    statement_hash TEXT NOT NULL,
    statement_kind TEXT NOT NULL,
    transaction_class TEXT NOT NULL CHECK (transaction_class IN ('transactional', 'non_transactional')),
    state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),
    started_at TIMESTAMPTZ NOT NULL,
    acknowledged_at TIMESTAMPTZ,
    error_code TEXT,
    evidence TEXT,
    attempt_id TEXT NOT NULL,
    PRIMARY KEY (run_id, step_index)
);
CREATE TABLE schema_migration_locks (
    scope TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    session_id_redacted TEXT,
    lock_strategy TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL,
    last_observed_at TIMESTAMPTZ NOT NULL,
    run_id TEXT,
    lock_epoch BIGINT NOT NULL CHECK (lock_epoch >= 1),
    state TEXT NOT NULL CHECK (state IN ('observed_active', 'released', 'lost', 'unknown'))
);
"#;

pub(crate) struct PostgresAdapter;

pub(crate) fn advisory_lock_key(info: &DatabaseInfo) -> Result<i64> {
    let database = info.database.as_deref().ok_or_else(|| DbmError::Driver {
        driver: "PostgreSQL",
        message: "session has no database identity for advisory locking".to_owned(),
    })?;
    let schema = info.schema.as_deref().ok_or_else(|| DbmError::Driver {
        driver: "PostgreSQL",
        message: "session has no schema identity for advisory locking".to_owned(),
    })?;

    let mut hasher = Sha256::new();
    hasher.update(b"dbm-postgresql-advisory-lock-v1\0");
    for value in [database, schema] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize();
    let key_bytes = digest[..8].try_into().map_err(|_| {
        DbmError::Execution("SHA-256 digest returned fewer than 8 bytes".to_owned())
    })?;
    Ok(i64::from_be_bytes(key_bytes))
}

impl DatabaseAdapter for PostgresAdapter {
    fn connect<'a>(
        &'a self,
        config: &'a LoadedConfig,
        intent: SessionIntent,
    ) -> BoxFuture<'a, Result<Box<dyn DatabaseSession>>> {
        Box::pin(async move {
            let variable_name = config.config.target.url_env.as_deref().ok_or_else(|| {
                DbmError::Configuration("PostgreSQL target has no url_env".to_owned())
            })?;
            let connection_string = std::env::var(variable_name).map_err(|error| {
                DbmError::Configuration(match error {
                    std::env::VarError::NotPresent => {
                        format!("PostgreSQL connection variable {variable_name} is not set")
                    }
                    std::env::VarError::NotUnicode(_) => format!(
                        "PostgreSQL connection variable {variable_name} is not valid Unicode"
                    ),
                })
            })?;
            let (mut postgres_config, use_tls) = prepare_connection_config(
                &connection_string,
                config.config.target.allow_insecure_transport,
                config.config.target.environment,
            )?;
            if postgres_config.get_connect_timeout().is_none() {
                postgres_config.connect_timeout(Duration::from_secs(10));
            }
            let target_identity = postgres_target_identity(&postgres_config);

            let (client, connection_task) = if use_tls {
                let tls =
                    make_tls_connector(config.config.target.tls_ca_cert_env.as_deref()).await?;
                let connected = postgres_config.connect(tls).await.map_err(|_| {
                    driver_error(
                        "connection failed; verify reachability, credentials, and the TLS certificate",
                    )
                })?;
                let (client, connection) = connected;
                (client, tokio::spawn(connection))
            } else {
                let (client, connection) = postgres_config.connect(NoTls).await.map_err(|_| {
                    driver_error(
                        "local plaintext connection failed; connection details are redacted",
                    )
                })?;
                (client, tokio::spawn(connection))
            };
            let mut session = PostgresSession {
                client: Some(client),
                connection_task: Some(connection_task),
                info: DatabaseInfo {
                    dialect: "postgres",
                    server_version: String::new(),
                    database: None,
                    schema: None,
                    target_identity: Some(target_identity),
                    session_id: None,
                },
                intent,
                transaction_open: false,
                held_lock: None,
                closed: false,
            };
            if let Err(error) = session.load_session_info().await {
                let _ = session.close().await;
                return Err(error);
            }
            Ok(Box::new(session) as Box<dyn DatabaseSession>)
        })
    }
}

struct PostgresSession {
    client: Option<Client>,
    connection_task: Option<JoinHandle<std::result::Result<(), tokio_postgres::Error>>>,
    info: DatabaseInfo,
    intent: SessionIntent,
    transaction_open: bool,
    held_lock: Option<i64>,
    closed: bool,
}

const POSTGRES_METADATA_V3_TO_V4_DDL: &str = r#"
CREATE TABLE schema_migration_imports (
    import_id TEXT PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),
    origin_tool TEXT NOT NULL CHECK (origin_tool IN ('knex', 'goose', 'sqlx')),
    source_identity TEXT NOT NULL,
    snapshot_digest TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL,
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    current_migration_count BIGINT NOT NULL CHECK (current_migration_count >= 0),
    UNIQUE (namespace, import_id)
);
CREATE TABLE schema_migration_import_events (
    import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),
    event_index BIGINT NOT NULL CHECK (event_index >= 0),
    event_id TEXT NOT NULL,
    original_version TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('applied', 'reverted', 'failed', 'sentinel')),
    filename TEXT,
    batch_id TEXT,
    applied_at TEXT,
    checksum_algorithm TEXT,
    checksum_value TEXT,
    execution_time_ms BIGINT CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),
    PRIMARY KEY (import_id, event_index)
);
CREATE TABLE schema_migration_adoptions (
    adoption_id TEXT PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL CHECK (length(namespace) BETWEEN 1 AND 128),
    import_id TEXT NOT NULL REFERENCES schema_migration_imports(import_id),
    legacy_migration_key CHAR(64) NOT NULL CHECK (length(legacy_migration_key) = 64),
    native_migration_key CHAR(64) NOT NULL CHECK (length(native_migration_key) = 64),
    native_version VARCHAR(20) NOT NULL CHECK (length(native_version) = 20),
    native_checksum TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    approved_by TEXT NOT NULL,
    reason TEXT NOT NULL,
    adopted_at TIMESTAMPTZ NOT NULL,
    UNIQUE (namespace, legacy_migration_key)
);
ALTER TABLE schema_migrations
    ALTER COLUMN native_version DROP NOT NULL,
    ALTER COLUMN filename DROP NOT NULL,
    ALTER COLUMN source_path DROP NOT NULL,
    ALTER COLUMN description DROP NOT NULL,
    ALTER COLUMN content_checksum DROP NOT NULL,
    ALTER COLUMN applied_at DROP NOT NULL;
ALTER TABLE schema_migrations
    ADD COLUMN original_version TEXT,
    ADD COLUMN origin_tool TEXT,
    ADD COLUMN origin_database_identity TEXT,
    ADD COLUMN origin_identity TEXT,
    ADD COLUMN legacy_import_id TEXT REFERENCES schema_migration_imports(import_id),
    ADD COLUMN batch_id TEXT,
    ADD COLUMN checksum_algorithm TEXT,
    ADD COLUMN checksum_value TEXT,
    ADD COLUMN execution_time_ms BIGINT CHECK (execution_time_ms IS NULL OR execution_time_ms >= 0),
    ADD COLUMN verification_status TEXT CHECK (verification_status IS NULL OR verification_status IN ('verified', 'partial', 'unverifiable', 'adopted')),
    ADD COLUMN origin_applied_at TEXT,
    ADD COLUMN observed_at TIMESTAMPTZ;
ALTER TABLE schema_migrations
    DROP CONSTRAINT IF EXISTS schema_migrations_kind_check,
    DROP CONSTRAINT IF EXISTS schema_migrations_authoring_type_check,
    ADD CONSTRAINT schema_migrations_kind_check CHECK (kind IN ('versioned', 'legacy')),
    ADD CONSTRAINT schema_migrations_authoring_type_check CHECK (authoring_type IN ('sql', 'rust', 'dsl', 'legacy_unknown')),
    ADD CONSTRAINT schema_migrations_legacy_shape_check CHECK (
        (kind = 'versioned' AND native_version IS NOT NULL AND original_version IS NULL
         AND legacy_import_id IS NULL AND origin_tool IS NULL)
        OR
        (kind = 'legacy' AND native_version IS NULL AND original_version IS NOT NULL
         AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL
         AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL
         AND verification_status IS NOT NULL AND observed_at IS NOT NULL)
    );
"#;

const POSTGRES_METADATA_V4_TO_V5_DDL: &str = r#"
ALTER TABLE schema_migrations
    ADD COLUMN repeatable_name VARCHAR(128),
    ADD CONSTRAINT schema_migrations_repeatable_name_check
        CHECK (repeatable_name IS NULL OR length(repeatable_name) BETWEEN 1 AND 128);
ALTER TABLE schema_migrations
    DROP CONSTRAINT schema_migrations_kind_check,
    DROP CONSTRAINT schema_migrations_legacy_shape_check,
    ADD CONSTRAINT schema_migrations_kind_check CHECK (kind IN ('versioned', 'repeatable', 'legacy')),
    ADD CONSTRAINT schema_migrations_legacy_shape_check CHECK (
        (kind = 'versioned' AND native_version IS NOT NULL AND repeatable_name IS NULL AND original_version IS NULL
         AND legacy_import_id IS NULL AND origin_tool IS NULL)
        OR
        (kind = 'repeatable' AND native_version IS NULL AND repeatable_name IS NOT NULL AND original_version IS NULL
         AND legacy_import_id IS NULL AND origin_tool IS NULL)
        OR
        (kind = 'legacy' AND native_version IS NULL AND repeatable_name IS NULL AND original_version IS NOT NULL
         AND legacy_import_id IS NOT NULL AND origin_tool IS NOT NULL
         AND origin_database_identity IS NOT NULL AND origin_identity IS NOT NULL
         AND verification_status IS NOT NULL AND observed_at IS NOT NULL)
    );
ALTER TABLE schema_migrations
    ADD CONSTRAINT schema_migrations_namespace_repeatable_uq UNIQUE (namespace, kind, repeatable_name);
ALTER TABLE schema_migration_runs
    ALTER COLUMN migration_version TYPE VARCHAR(128),
    DROP CONSTRAINT schema_migration_runs_direction_check,
    ADD CONSTRAINT schema_migration_runs_direction_check
        CHECK (direction IN ('up', 'down', 'repeat', 'reconcile'));
"#;

impl PostgresSession {
    async fn read_initialized_snapshot(&self, namespace: &str) -> Result<MetadataSnapshot> {
        let client = self.client()?;
        let metadata_row = client
            .query_opt(
                "SELECT metadata_format_version::bigint, installation_id \
                 FROM schema_migration_meta WHERE singleton = 1",
                &[],
            )
            .await
            .map_err(|_| driver_error("could not read PostgreSQL metadata identity"))?
            .ok_or_else(|| {
                DbmError::Diverged(
                    "PostgreSQL metadata tables exist without a metadata identity row".to_owned(),
                )
            })?;
        let format_version: i64 = metadata_row
            .try_get(0)
            .map_err(|_| metadata_row_error("metadata format version"))?;
        let installation_id: String = metadata_row
            .try_get(1)
            .map_err(|_| metadata_row_error("metadata installation id"))?;
        if !matches!(
            format_version,
            crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION
                | crate::storage::PREVIOUS_METADATA_FORMAT_VERSION
                | crate::storage::METADATA_FORMAT_VERSION
        ) || installation_id.is_empty()
        {
            return Err(DbmError::Diverged(format!(
                "PostgreSQL metadata format {format_version} is unsupported or has an invalid installation id"
            )));
        }

        let applied_query = if format_version >= crate::storage::METADATA_FORMAT_VERSION {
            "SELECT migration_key, kind, COALESCE(repeatable_name, native_version)::text, \
                    filename, source_path, description, content_checksum, apply_order::bigint, applied_at::text \
             FROM schema_migrations WHERE namespace = $1 AND kind IN ('versioned', 'repeatable') \
             ORDER BY apply_order ASC, kind ASC, native_version ASC, repeatable_name ASC"
        } else {
            "SELECT migration_key, 'versioned'::text, native_version::text, filename, source_path, \
                    description, content_checksum, apply_order::bigint, applied_at::text \
             FROM schema_migrations WHERE namespace = $1 AND kind = 'versioned' \
             ORDER BY apply_order ASC, native_version ASC"
        };
        let applied_rows = client
            .query(applied_query, &[&namespace])
            .await
            .map_err(|_| driver_error("could not read PostgreSQL applied migrations"))?;
        let mut applied = Vec::with_capacity(applied_rows.len());
        for row in applied_rows {
            applied.push(AppliedMigration {
                migration_key: row
                    .try_get(0)
                    .map_err(|_| metadata_row_error("migration key"))?,
                kind: row
                    .try_get(1)
                    .map_err(|_| metadata_row_error("migration kind"))?,
                version: row
                    .try_get(2)
                    .map_err(|_| metadata_row_error("migration version"))?,
                filename: row
                    .try_get(3)
                    .map_err(|_| metadata_row_error("migration filename"))?,
                source_path: row
                    .try_get(4)
                    .map_err(|_| metadata_row_error("migration source path"))?,
                description: row
                    .try_get(5)
                    .map_err(|_| metadata_row_error("migration description"))?,
                checksum: row
                    .try_get(6)
                    .map_err(|_| metadata_row_error("migration checksum"))?,
                apply_order: row
                    .try_get(7)
                    .map_err(|_| metadata_row_error("migration apply order"))?,
                applied_at: row
                    .try_get(8)
                    .map_err(|_| metadata_row_error("migration timestamp"))?,
            });
        }

        let (legacy_applied, legacy_imports, legacy_import_events) = if format_version
            >= crate::storage::PREVIOUS_METADATA_FORMAT_VERSION
        {
            let legacy_rows = client
                .query(
                    "SELECT migration_key, legacy_import_id, origin_tool, \
                                origin_database_identity, origin_identity, original_version, \
                                filename, description, batch_id, origin_applied_at, \
                                checksum_algorithm, checksum_value, execution_time_ms::bigint, \
                                verification_status, apply_order::bigint \
                         FROM schema_migrations WHERE namespace = $1 AND kind = 'legacy' \
                         ORDER BY apply_order ASC, migration_key ASC",
                    &[&namespace],
                )
                .await
                .map_err(|_| driver_error("could not read PostgreSQL imported migrations"))?;
            let mut legacy_applied = Vec::with_capacity(legacy_rows.len());
            for row in legacy_rows {
                legacy_applied.push(LegacyAppliedMigration {
                    migration_key: row
                        .try_get(0)
                        .map_err(|_| metadata_row_error("legacy migration key"))?,
                    import_id: row
                        .try_get(1)
                        .map_err(|_| metadata_row_error("legacy import id"))?,
                    origin_tool: row
                        .try_get(2)
                        .map_err(|_| metadata_row_error("legacy origin tool"))?,
                    source_identity: row
                        .try_get(3)
                        .map_err(|_| metadata_row_error("legacy source identity"))?,
                    origin_identity: row
                        .try_get(4)
                        .map_err(|_| metadata_row_error("legacy origin identity"))?,
                    original_version: row
                        .try_get(5)
                        .map_err(|_| metadata_row_error("legacy original version"))?,
                    filename: row
                        .try_get(6)
                        .map_err(|_| metadata_row_error("legacy filename"))?,
                    description: row
                        .try_get(7)
                        .map_err(|_| metadata_row_error("legacy description"))?,
                    batch_id: row
                        .try_get(8)
                        .map_err(|_| metadata_row_error("legacy batch id"))?,
                    applied_at: row
                        .try_get(9)
                        .map_err(|_| metadata_row_error("legacy applied timestamp"))?,
                    checksum_algorithm: row
                        .try_get(10)
                        .map_err(|_| metadata_row_error("legacy checksum algorithm"))?,
                    checksum_value: row
                        .try_get(11)
                        .map_err(|_| metadata_row_error("legacy checksum value"))?,
                    execution_time_ms: row
                        .try_get(12)
                        .map_err(|_| metadata_row_error("legacy execution time"))?,
                    verification_status: row
                        .try_get(13)
                        .map_err(|_| metadata_row_error("legacy verification status"))?,
                    apply_order: row
                        .try_get(14)
                        .map_err(|_| metadata_row_error("legacy apply order"))?,
                });
            }

            let import_rows = client
                    .query(
                        "SELECT import_id, namespace, origin_tool, source_identity, \
                                snapshot_digest, plan_digest, imported_at::text, event_count::bigint, \
                                current_migration_count::bigint \
                         FROM schema_migration_imports WHERE namespace = $1 \
                         ORDER BY imported_at ASC, import_id ASC",
                        &[&namespace],
                    )
                    .await
                    .map_err(|_| driver_error("could not read PostgreSQL import records"))?;
            let mut legacy_imports = Vec::with_capacity(import_rows.len());
            for row in import_rows {
                legacy_imports.push(LegacyImportRecord {
                    import_id: row
                        .try_get(0)
                        .map_err(|_| metadata_row_error("legacy import id"))?,
                    namespace: row
                        .try_get(1)
                        .map_err(|_| metadata_row_error("legacy import namespace"))?,
                    origin_tool: row
                        .try_get(2)
                        .map_err(|_| metadata_row_error("legacy origin tool"))?,
                    source_identity: row
                        .try_get(3)
                        .map_err(|_| metadata_row_error("legacy source identity"))?,
                    snapshot_digest: row
                        .try_get(4)
                        .map_err(|_| metadata_row_error("legacy snapshot digest"))?,
                    plan_digest: row
                        .try_get(5)
                        .map_err(|_| metadata_row_error("legacy plan digest"))?,
                    imported_at: row
                        .try_get(6)
                        .map_err(|_| metadata_row_error("legacy import timestamp"))?,
                    event_count: row
                        .try_get(7)
                        .map_err(|_| metadata_row_error("legacy event count"))?,
                    current_migration_count: row
                        .try_get(8)
                        .map_err(|_| metadata_row_error("legacy current migration count"))?,
                });
            }

            let event_rows = client
                .query(
                    "SELECT events.import_id, events.event_index::bigint, events.event_id, \
                                events.original_version, events.state, events.filename, \
                                events.batch_id, events.applied_at, events.checksum_algorithm, \
                                events.checksum_value, events.execution_time_ms::bigint \
                         FROM schema_migration_import_events AS events \
                         INNER JOIN schema_migration_imports AS imports USING (import_id) \
                         WHERE imports.namespace = $1 \
                         ORDER BY events.import_id ASC, events.event_index ASC",
                    &[&namespace],
                )
                .await
                .map_err(|_| driver_error("could not read PostgreSQL import events"))?;
            let mut legacy_import_events = Vec::with_capacity(event_rows.len());
            for row in event_rows {
                legacy_import_events.push(LegacyImportEventRecord {
                    import_id: row
                        .try_get(0)
                        .map_err(|_| metadata_row_error("legacy event import id"))?,
                    event_index: row
                        .try_get(1)
                        .map_err(|_| metadata_row_error("legacy event index"))?,
                    event_id: row
                        .try_get(2)
                        .map_err(|_| metadata_row_error("legacy event id"))?,
                    original_version: row
                        .try_get(3)
                        .map_err(|_| metadata_row_error("legacy event version"))?,
                    state: row
                        .try_get(4)
                        .map_err(|_| metadata_row_error("legacy event state"))?,
                    filename: row
                        .try_get(5)
                        .map_err(|_| metadata_row_error("legacy event filename"))?,
                    batch_id: row
                        .try_get(6)
                        .map_err(|_| metadata_row_error("legacy event batch id"))?,
                    applied_at: row
                        .try_get(7)
                        .map_err(|_| metadata_row_error("legacy event timestamp"))?,
                    checksum_algorithm: row
                        .try_get(8)
                        .map_err(|_| metadata_row_error("legacy event checksum algorithm"))?,
                    checksum_value: row
                        .try_get(9)
                        .map_err(|_| metadata_row_error("legacy event checksum value"))?,
                    execution_time_ms: row
                        .try_get(10)
                        .map_err(|_| metadata_row_error("legacy event execution time"))?,
                });
            }
            (legacy_applied, legacy_imports, legacy_import_events)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

        let run_rows = client
            .query(
                "SELECT run_id, batch_id, namespace, migration_version, direction, state, attempt::bigint, \
                        created_at::text, started_at::text, ended_at::text, duration_ms::bigint, checksum_at_execution, \
                        plan_hash, error_code, error_summary_redacted, reconciliation_result, \
                        reconciliation_plan, reconciliation_evidence_ref, reconciliation_reason, \
                        reconciled_by, reconciled_from_state, related_run_id \
                 FROM schema_migration_runs \
                 WHERE namespace = $1 \
                 ORDER BY created_at ASC, run_id ASC",
                &[&namespace],
            )
            .await
            .map_err(|_| driver_error("could not read PostgreSQL migration history"))?;
        let mut runs = Vec::with_capacity(run_rows.len());
        for row in run_rows {
            runs.push(RunRecord {
                run_id: row.try_get(0).map_err(|_| metadata_row_error("run id"))?,
                batch_id: row.try_get(1).map_err(|_| metadata_row_error("batch id"))?,
                namespace: row
                    .try_get(2)
                    .map_err(|_| metadata_row_error("run namespace"))?,
                migration_id: row
                    .try_get(3)
                    .map_err(|_| metadata_row_error("run migration id"))?,
                direction: row
                    .try_get(4)
                    .map_err(|_| metadata_row_error("run direction"))?,
                state: row
                    .try_get(5)
                    .map_err(|_| metadata_row_error("run state"))?,
                attempt: row
                    .try_get(6)
                    .map_err(|_| metadata_row_error("run attempt"))?,
                created_at: row
                    .try_get(7)
                    .map_err(|_| metadata_row_error("run creation time"))?,
                started_at: row
                    .try_get(8)
                    .map_err(|_| metadata_row_error("run start time"))?,
                ended_at: row
                    .try_get(9)
                    .map_err(|_| metadata_row_error("run end time"))?,
                duration_ms: row
                    .try_get(10)
                    .map_err(|_| metadata_row_error("run duration"))?,
                checksum_at_execution: row
                    .try_get(11)
                    .map_err(|_| metadata_row_error("run checksum"))?,
                plan_hash: row
                    .try_get(12)
                    .map_err(|_| metadata_row_error("run plan hash"))?,
                error_code: row
                    .try_get(13)
                    .map_err(|_| metadata_row_error("run error code"))?,
                error_summary_redacted: row
                    .try_get(14)
                    .map_err(|_| metadata_row_error("run error summary"))?,
                reconciliation_result: row
                    .try_get(15)
                    .map_err(|_| metadata_row_error("reconciliation result"))?,
                reconciliation_plan: row
                    .try_get(16)
                    .map_err(|_| metadata_row_error("reconciliation plan"))?,
                reconciliation_evidence_ref: row
                    .try_get(17)
                    .map_err(|_| metadata_row_error("reconciliation evidence reference"))?,
                reconciliation_reason: row
                    .try_get(18)
                    .map_err(|_| metadata_row_error("reconciliation reason"))?,
                reconciled_by: row
                    .try_get(19)
                    .map_err(|_| metadata_row_error("reconciliation actor"))?,
                reconciled_from_state: row
                    .try_get(20)
                    .map_err(|_| metadata_row_error("reconciled run state"))?,
                related_run_id: row
                    .try_get(21)
                    .map_err(|_| metadata_row_error("related run id"))?,
            });
        }

        let step_rows = client
            .query(
                "SELECT steps.run_id, steps.step_index::bigint, steps.step_key, steps.direction, \
                        steps.statement_hash, steps.statement_kind, steps.transaction_class, \
                        steps.state, steps.started_at::text, steps.acknowledged_at::text, steps.error_code, steps.evidence \
                 FROM schema_migration_steps AS steps \
                 INNER JOIN schema_migration_runs AS runs USING (run_id) \
                 WHERE runs.namespace = $1 \
                 ORDER BY runs.created_at ASC, runs.run_id ASC, steps.step_index ASC",
                &[&namespace],
            )
            .await
            .map_err(|_| driver_error("could not read PostgreSQL migration journal"))?;
        let mut steps = Vec::with_capacity(step_rows.len());
        for row in step_rows {
            steps.push(StepRecord {
                run_id: row
                    .try_get(0)
                    .map_err(|_| metadata_row_error("step run id"))?,
                step_index: row
                    .try_get(1)
                    .map_err(|_| metadata_row_error("step index"))?,
                step_key: row.try_get(2).map_err(|_| metadata_row_error("step key"))?,
                direction: row
                    .try_get(3)
                    .map_err(|_| metadata_row_error("step direction"))?,
                statement_hash: row
                    .try_get(4)
                    .map_err(|_| metadata_row_error("step hash"))?,
                statement_kind: row
                    .try_get(5)
                    .map_err(|_| metadata_row_error("step kind"))?,
                transaction_class: row
                    .try_get(6)
                    .map_err(|_| metadata_row_error("step transaction class"))?,
                state: row
                    .try_get(7)
                    .map_err(|_| metadata_row_error("step state"))?,
                started_at: row
                    .try_get(8)
                    .map_err(|_| metadata_row_error("step start time"))?,
                acknowledged_at: row
                    .try_get(9)
                    .map_err(|_| metadata_row_error("step acknowledgement time"))?,
                error_code: row
                    .try_get(10)
                    .map_err(|_| metadata_row_error("step error code"))?,
                evidence: row
                    .try_get(11)
                    .map_err(|_| metadata_row_error("step evidence"))?,
            });
        }

        let lock_row = client
            .query_opt(
                "SELECT scope, owner_id, lock_strategy, acquired_at::text, last_observed_at::text, \
                        run_id, lock_epoch::bigint, state \
                 FROM schema_migration_locks WHERE scope = 'database'",
                &[],
            )
            .await
            .map_err(|_| driver_error("could not read PostgreSQL migration lock metadata"))?;
        let lock = match lock_row {
            Some(row) => Some(LockRecord {
                scope: row
                    .try_get(0)
                    .map_err(|_| metadata_row_error("lock scope"))?,
                owner_id: row
                    .try_get(1)
                    .map_err(|_| metadata_row_error("lock owner"))?,
                lock_strategy: row
                    .try_get(2)
                    .map_err(|_| metadata_row_error("lock strategy"))?,
                acquired_at: row
                    .try_get(3)
                    .map_err(|_| metadata_row_error("lock acquisition time"))?,
                last_observed_at: row
                    .try_get(4)
                    .map_err(|_| metadata_row_error("lock observation time"))?,
                run_id: row
                    .try_get(5)
                    .map_err(|_| metadata_row_error("lock run id"))?,
                lock_epoch: row
                    .try_get(6)
                    .map_err(|_| metadata_row_error("lock epoch"))?,
                state: row
                    .try_get(7)
                    .map_err(|_| metadata_row_error("lock state"))?,
            }),
            None => None,
        };

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

    async fn read_snapshot_contents(&self, namespace: &str) -> Result<MetadataSnapshot> {
        let row = self
            .client()?
            .query_one(METADATA_TABLE_COUNT_QUERY, &[])
            .await
            .map_err(|_| driver_error("could not inspect PostgreSQL migration metadata"))?;
        let relation_count: i64 = row
            .try_get(0)
            .map_err(|_| driver_error("PostgreSQL returned invalid metadata inspection results"))?;
        let table_count: i64 = row
            .try_get(1)
            .map_err(|_| driver_error("PostgreSQL returned invalid metadata inspection results"))?;
        if relation_count == 0 {
            return Ok(MetadataSnapshot::default());
        }
        if !matches!((relation_count, table_count), (5, 5) | (8, 8)) {
            return Err(DbmError::Diverged(format!(
                "PostgreSQL migration metadata has {table_count} base tables among {relation_count} reserved objects (expected 5 or 8 tables); refusing to plan from an unknown state"
            )));
        }
        let snapshot = self.read_initialized_snapshot(namespace).await?;
        let expected_count = if snapshot
            .metadata_format_version
            .is_some_and(|version| version >= crate::storage::PREVIOUS_METADATA_FORMAT_VERSION)
        {
            8
        } else {
            5
        };
        if relation_count != expected_count || table_count != expected_count {
            return Err(DbmError::Diverged(format!(
                "PostgreSQL metadata format {:?} does not match its reserved table set",
                snapshot.metadata_format_version
            )));
        }
        Ok(snapshot)
    }

    async fn read_snapshot_consistently(&mut self, namespace: &str) -> Result<MetadataSnapshot> {
        let owns_transaction = !self.transaction_open;
        if owns_transaction {
            self.verify_session().await?;
            self.client()?
                .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .await
                .map_err(|_| driver_error("could not start a PostgreSQL read snapshot"))?;
            self.transaction_open = true;
        }

        let result = match self.verify_session().await {
            Ok(()) => self.read_snapshot_contents(namespace).await,
            Err(error) => Err(error),
        };
        if !owns_transaction {
            return result;
        }

        let result = match result {
            Ok(snapshot) => self.verify_session().await.map(|()| snapshot),
            Err(error) => Err(error),
        };
        match result {
            Ok(snapshot) => {
                self.client()?.batch_execute("COMMIT").await.map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "PostgreSQL read snapshot could not be closed cleanly".to_owned(),
                    )
                })?;
                self.transaction_open = false;
                Ok(snapshot)
            }
            Err(read_error) => match self.client()?.batch_execute("ROLLBACK").await {
                Ok(()) => {
                    self.transaction_open = false;
                    Err(read_error)
                }
                Err(_) => Err(DbmError::ReconciliationRequired(
                    "PostgreSQL read snapshot failed and its transaction could not be closed"
                        .to_owned(),
                )),
            },
        }
    }

    async fn legacy_table_exists(&self, table: &str) -> Result<bool> {
        let row = self
            .client()?
            .query_one("SELECT to_regclass($1)::text", &[&table])
            .await
            .map_err(|_| driver_error("could not inspect the PostgreSQL legacy history table"))?;
        let relation: Option<String> = row
            .try_get(0)
            .map_err(|_| driver_error("PostgreSQL returned invalid legacy table metadata"))?;
        Ok(relation.is_some())
    }

    async fn require_legacy_table(&self, table: &str, tool: &str) -> Result<()> {
        if self.legacy_table_exists(table).await? {
            return Ok(());
        }
        Err(DbmError::Configuration(format!(
            "legacy {tool} table {table} was not found in the selected PostgreSQL schema"
        )))
    }

    async fn inspect_legacy_history_inner(
        &mut self,
        tool: LegacyTool,
        source_identity: &str,
    ) -> Result<LegacyImportSnapshot> {
        self.verify_session().await?;
        let snapshot = match tool {
            LegacyTool::Knex => {
                self.require_legacy_table("knex_migrations", "Knex").await?;
                let rows = self
                    .client()?
                    .query(
                        "SELECT id::bigint, name::text, batch::bigint, migration_time::text \
                         FROM knex_migrations ORDER BY id",
                        &[],
                    )
                    .await
                    .map_err(|_| {
                        driver_error("could not read PostgreSQL Knex migration history")
                    })?;
                let mut migrations = Vec::with_capacity(rows.len());
                for row in rows {
                    migrations.push(KnexRow {
                        id: row.try_get(0).map_err(|_| legacy_row_error("Knex", "id"))?,
                        name: row
                            .try_get(1)
                            .map_err(|_| legacy_row_error("Knex", "name"))?,
                        batch: row
                            .try_get(2)
                            .map_err(|_| legacy_row_error("Knex", "batch"))?,
                        migration_time: row
                            .try_get(3)
                            .map_err(|_| legacy_row_error("Knex", "migration_time"))?,
                    });
                }

                let lock = if self.legacy_table_exists("knex_migrations_lock").await? {
                    let rows = self
                        .client()?
                        .query(
                            "SELECT is_locked::text, \"index\"::text \
                             FROM knex_migrations_lock ORDER BY \"index\"",
                            &[],
                        )
                        .await
                        .map_err(|_| driver_error("could not read PostgreSQL Knex lock state"))?;
                    let mut locks = Vec::with_capacity(rows.len());
                    for row in rows {
                        let locked_text: String = row
                            .try_get(0)
                            .map_err(|_| legacy_row_error("Knex", "is_locked"))?;
                        let index_text: Option<String> = row
                            .try_get(1)
                            .map_err(|_| legacy_row_error("Knex", "lock index"))?;
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
                    .client()?
                    .query(
                        "SELECT id::bigint, version_id::bigint, is_applied::text, tstamp::text \
                         FROM goose_db_version ORDER BY id",
                        &[],
                    )
                    .await
                    .map_err(|_| {
                        driver_error("could not read PostgreSQL Goose migration history")
                    })?;
                let mut events = Vec::with_capacity(rows.len());
                for row in rows {
                    let applied_text: String = row
                        .try_get(2)
                        .map_err(|_| legacy_row_error("Goose", "is_applied"))?;
                    events.push(GooseRow {
                        id: row
                            .try_get(0)
                            .map_err(|_| legacy_row_error("Goose", "id"))?,
                        version_id: row
                            .try_get(1)
                            .map_err(|_| legacy_row_error("Goose", "version_id"))?,
                        is_applied: parse_legacy_bool(&applied_text, "Goose", "is_applied")?,
                        timestamp: row
                            .try_get(3)
                            .map_err(|_| legacy_row_error("Goose", "tstamp"))?,
                    });
                }
                inspect_goose(source_identity, events)?
            }
            LegacyTool::Sqlx => {
                self.require_legacy_table("_sqlx_migrations", "SQLx")
                    .await?;
                let rows = self
                    .client()?
                    .query(
                        "SELECT version::bigint, description::text, installed_on::text, \
                                success::text, checksum, execution_time::bigint \
                         FROM _sqlx_migrations ORDER BY version",
                        &[],
                    )
                    .await
                    .map_err(|_| {
                        driver_error("could not read PostgreSQL SQLx migration history")
                    })?;
                let mut migrations = Vec::with_capacity(rows.len());
                for row in rows {
                    let success_text: String = row
                        .try_get(3)
                        .map_err(|_| legacy_row_error("SQLx", "success"))?;
                    migrations.push(SqlxRow {
                        version: row
                            .try_get(0)
                            .map_err(|_| legacy_row_error("SQLx", "version"))?,
                        description: row
                            .try_get(1)
                            .map_err(|_| legacy_row_error("SQLx", "description"))?,
                        installed_on: row
                            .try_get(2)
                            .map_err(|_| legacy_row_error("SQLx", "installed_on"))?,
                        success: parse_legacy_bool(&success_text, "SQLx", "success")?,
                        checksum: row
                            .try_get(4)
                            .map_err(|_| legacy_row_error("SQLx", "checksum"))?,
                        execution_time_ms: row
                            .try_get(5)
                            .map_err(|_| legacy_row_error("SQLx", "execution_time"))?,
                    });
                }
                inspect_sqlx(source_identity, migrations)?
            }
        };
        self.verify_session().await?;
        Ok(snapshot)
    }

    async fn persist_legacy_import_inner(&mut self, import: LegacyImportWrite<'_>) -> Result<()> {
        self.ensure_metadata_transaction()?;
        self.verify_session().await?;
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
        self.client()?
            .execute(
                "INSERT INTO schema_migration_imports \
                 (import_id, namespace, origin_tool, source_identity, snapshot_digest, plan_digest, \
                  imported_at, event_count, current_migration_count) \
                 VALUES ($1, $2, $3, $4, $5, $6, ($7::text)::timestamptz, $8, $9)",
                &[
                    &import.import_id,
                    &import.namespace,
                    &crate::legacy::tool_name(import.tool),
                    &source.source_identity,
                    &source.digest,
                    &import.plan_digest,
                    &import.imported_at,
                    &event_count,
                    &current_migration_count,
                ],
            )
            .await
            .map_err(|error| Self::classify_statement_error(error, "recording legacy import"))?;

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
            self.client()?
                .execute(
                    "INSERT INTO schema_migration_import_events \
                     (import_id, event_index, event_id, original_version, state, filename, batch_id, \
                      applied_at, checksum_algorithm, checksum_value, execution_time_ms) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    &[
                        &import.import_id,
                        &event_index,
                        &event.event_id,
                        &event.original_version,
                        &crate::legacy::event_state_name(event.state),
                        &event.filename,
                        &event.batch_id,
                        &event.applied_at,
                        &checksum_algorithm,
                        &checksum_value,
                        &event.execution_time_ms,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "recording a legacy import event")
                })?;
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
            self.client()?
                .execute(
                    "INSERT INTO schema_migrations \
                     (namespace, migration_key, kind, native_version, original_version, filename, \
                      source_path, description, authoring_type, content_checksum, origin_tool, \
                      origin_database_identity, origin_identity, legacy_import_id, batch_id, \
                      checksum_algorithm, checksum_value, execution_time_ms, verification_status, \
                      apply_order, applied_at, origin_applied_at, observed_at) \
                     VALUES ($1, $2, 'legacy', NULL, $3, $4, NULL, $5, 'legacy_unknown', NULL, \
                             $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, NULL, $16, \
                             ($17::text)::timestamptz)",
                    &[
                        &import.namespace,
                        &migration_key,
                        &migration.original_version,
                        &migration.filename,
                        &migration.filename,
                        &crate::legacy::tool_name(import.tool),
                        &source.source_identity,
                        &migration.original_identity,
                        &import.import_id,
                        &migration.batch_id,
                        &checksum_algorithm,
                        &checksum_value,
                        &migration.execution_time_ms,
                        &verification_status,
                        &apply_order,
                        &migration.applied_at,
                        &import.imported_at,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "recording an imported migration")
                })?;
        }
        self.verify_session().await
    }

    async fn persist_legacy_adoption_inner(
        &mut self,
        adoption: LegacyAdoptionWrite<'_>,
    ) -> Result<()> {
        self.ensure_metadata_transaction()?;
        self.verify_session().await?;
        let changed = self
            .client()?
            .execute(
                "UPDATE schema_migrations SET verification_status = 'adopted' \
                 WHERE namespace = $1 AND migration_key = $2 AND kind = 'legacy' \
                   AND legacy_import_id = $3 AND verification_status <> 'adopted'",
                &[
                    &adoption.namespace,
                    &adoption.legacy_migration_key,
                    &adoption.import_id,
                ],
            )
            .await
            .map_err(|error| {
                Self::classify_statement_error(error, "updating legacy adoption state")
            })?;
        if changed != 1 {
            return Err(DbmError::Diverged(
                "legacy adoption state changed while the operation was in progress".to_owned(),
            ));
        }
        self.client()?
            .execute(
                "INSERT INTO schema_migration_adoptions \
                 (adoption_id, namespace, import_id, legacy_migration_key, native_migration_key, \
                  native_version, native_checksum, evidence_ref, approved_by, reason, adopted_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, ($11::text)::timestamptz)",
                &[
                    &adoption.adoption_id,
                    &adoption.namespace,
                    &adoption.import_id,
                    &adoption.legacy_migration_key,
                    &adoption.native_migration_key,
                    &adoption.native_version,
                    &adoption.native_checksum,
                    &adoption.evidence_ref,
                    &adoption.actor,
                    &adoption.reason,
                    &adoption.adopted_at,
                ],
            )
            .await
            .map_err(|error| Self::classify_statement_error(error, "recording legacy adoption"))?;
        self.verify_session().await
    }

    async fn initialize_metadata_in_transaction(&mut self, initialized_at: &str) -> Result<()> {
        self.ensure_writable()?;
        self.verify_session().await?;
        if self.held_lock.is_none() {
            return Err(DbmError::Safety(
                "PostgreSQL metadata initialization requires the session advisory lock".to_owned(),
            ));
        }
        if !self.transaction_open {
            return Err(DbmError::Safety(
                "PostgreSQL metadata initialization requires an open transaction".to_owned(),
            ));
        }

        let row = self
            .client()?
            .query_one(METADATA_TABLE_COUNT_QUERY, &[])
            .await
            .map_err(|_| driver_error("could not inspect PostgreSQL metadata before bootstrap"))?;
        let relation_count: i64 = row
            .try_get(0)
            .map_err(|_| driver_error("PostgreSQL returned invalid metadata inspection results"))?;
        let table_count: i64 = row
            .try_get(1)
            .map_err(|_| driver_error("PostgreSQL returned invalid metadata inspection results"))?;
        if matches!((relation_count, table_count), (5, 5) | (8, 8)) {
            let snapshot = self
                .read_initialized_snapshot("__dbm_metadata_probe__")
                .await?;
            if relation_count != 8
                || snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
            {
                return Err(DbmError::Safety(
                    "existing PostgreSQL metadata requires an explicit metadata upgrade before migration writes"
                        .to_owned(),
                ));
            }
            return Ok(());
        }
        if relation_count != 0 || table_count != 0 {
            return Err(DbmError::Diverged(format!(
                "PostgreSQL migration metadata has {table_count} base tables among {relation_count} reserved objects; refusing automatic repair"
            )));
        }

        self.client()?
            .batch_execute(POSTGRES_METADATA_SCHEMA_DDL)
            .await
            .map_err(|error| {
                Self::classify_statement_error(error, "creating PostgreSQL metadata")
            })?;
        let metadata_version =
            i32::try_from(crate::storage::METADATA_FORMAT_VERSION).map_err(|_| {
                DbmError::Diverged("metadata format version exceeds PostgreSQL INTEGER".to_owned())
            })?;
        let installation_id = uuid::Uuid::new_v4().simple().to_string();
        self.client()?
            .execute(
                "INSERT INTO schema_migration_meta \
                 (singleton, metadata_format_version, installation_id, initialized_at) \
                 VALUES (1, $1, $2, ($3::text)::timestamptz)",
                &[&metadata_version, &installation_id, &initialized_at],
            )
            .await
            .map_err(|error| {
                Self::classify_statement_error(error, "recording PostgreSQL metadata identity")
            })?;
        Ok(())
    }

    async fn load_session_info(&mut self) -> Result<()> {
        let row = self
            .client()?
            .query_one(
                "SELECT current_setting('server_version'), current_database(), \
                        current_schema(), pg_backend_pid()",
                &[],
            )
            .await
            .map_err(|_| driver_error("could not read PostgreSQL session information"))?;
        let pid: i32 = row
            .try_get(3)
            .map_err(|_| driver_error("PostgreSQL returned invalid session information"))?;
        self.info.server_version = row
            .try_get(0)
            .map_err(|_| driver_error("PostgreSQL returned an invalid server version"))?;
        self.info.database = row
            .try_get(1)
            .map_err(|_| driver_error("PostgreSQL returned an invalid database name"))?;
        self.info.schema = row
            .try_get(2)
            .map_err(|_| driver_error("PostgreSQL returned an invalid schema name"))?;
        self.info.session_id = Some(pid.to_string());
        Ok(())
    }

    fn client(&self) -> Result<&Client> {
        self.client
            .as_ref()
            .ok_or_else(|| driver_error("session is closed"))
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.intent == SessionIntent::ReadOnly {
            return Err(DbmError::Safety(
                "a read-only PostgreSQL session cannot execute mutations".to_owned(),
            ));
        }
        if self.closed {
            return Err(driver_error("session is closed"));
        }
        Ok(())
    }

    fn ensure_metadata_transaction(&self) -> Result<()> {
        self.ensure_writable()?;
        if self.held_lock.is_none() {
            return Err(DbmError::Safety(
                "PostgreSQL migration metadata writes require the session advisory lock".to_owned(),
            ));
        }
        if !self.transaction_open {
            return Err(DbmError::Safety(
                "PostgreSQL migration metadata writes require an open transaction".to_owned(),
            ));
        }
        Ok(())
    }

    fn classify_statement_error(error: tokio_postgres::Error, operation: &str) -> DbmError {
        if error.as_db_error().is_some() {
            let sql_state = error
                .as_db_error()
                .map(|database_error| database_error.code().code())
                .unwrap_or("unknown SQLSTATE");
            DbmError::Execution(format!(
                "PostgreSQL rejected {operation} with SQLSTATE {sql_state}; server detail is redacted"
            ))
        } else {
            DbmError::ReconciliationRequired(format!(
                "PostgreSQL session was lost while {operation}; the server outcome is unknown"
            ))
        }
    }

    async fn finish_connection(&mut self) -> Result<()> {
        self.client.take();
        let Some(task) = self.connection_task.take() else {
            self.closed = true;
            return Ok(());
        };
        match task.await {
            Ok(Ok(())) => {
                self.closed = true;
                Ok(())
            }
            Ok(Err(_)) | Err(_) => {
                self.closed = true;
                Err(driver_error(
                    "PostgreSQL connection task ended unexpectedly",
                ))
            }
        }
    }
}

impl DatabaseSession for PostgresSession {
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
            let row = self
                .client()?
                .query_opt(
                    "SELECT adoption_id FROM schema_migration_adoptions \
                     WHERE namespace = $1 AND legacy_migration_key = $2",
                    &[&namespace, &legacy_migration_key],
                )
                .await
                .map_err(|_| driver_error("could not read PostgreSQL legacy adoption"))?;
            match row {
                Some(row) => row
                    .try_get(0)
                    .map(Some)
                    .map_err(|_| metadata_row_error("legacy adoption id")),
                None => Ok(None),
            }
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
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;

            let metadata = self
                .client()?
                .query_opt(
                    "SELECT metadata_format_version::bigint, installation_id \
                     FROM schema_migration_meta WHERE singleton = 1",
                    &[],
                )
                .await
                .map_err(|_| driver_error("could not read PostgreSQL metadata before upgrade"))?
                .ok_or_else(|| {
                    DbmError::Diverged(
                        "PostgreSQL metadata identity row is missing during upgrade".to_owned(),
                    )
                })?;
            let version: i64 = metadata
                .try_get(0)
                .map_err(|_| metadata_row_error("metadata format version"))?;
            let installation_id: String = metadata
                .try_get(1)
                .map_err(|_| metadata_row_error("metadata installation id"))?;
            if !matches!(
                version,
                crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION
                    | crate::storage::PREVIOUS_METADATA_FORMAT_VERSION
            ) || installation_id.is_empty()
            {
                return Err(DbmError::Diverged(format!(
                    "explicit PostgreSQL metadata upgrade supports formats 3 and 4 only; found {version}"
                )));
            }

            let counts = self
                .client()?
                .query_one(METADATA_TABLE_COUNT_QUERY, &[])
                .await
                .map_err(|_| {
                    driver_error("could not inspect PostgreSQL metadata before upgrade")
                })?;
            let relation_count: i64 = counts
                .try_get(0)
                .map_err(|_| metadata_row_error("reserved object count"))?;
            let table_count: i64 = counts
                .try_get(1)
                .map_err(|_| metadata_row_error("metadata table count"))?;
            let expected_count =
                if version == crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION {
                    5
                } else {
                    8
                };
            if (relation_count, table_count) != (expected_count, expected_count) {
                return Err(DbmError::Diverged(format!(
                    "PostgreSQL format {version} upgrade requires exactly {expected_count} intact metadata tables; found {table_count} tables among {relation_count} reserved objects"
                )));
            }

            if version == crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION {
                self.client()?
                    .batch_execute(POSTGRES_METADATA_V3_TO_V4_DDL)
                    .await
                    .map_err(|error| {
                        Self::classify_statement_error(
                            error,
                            "upgrading PostgreSQL metadata to format 4",
                        )
                    })?;
                let changed = self
                    .client()?
                    .execute(
                        "UPDATE schema_migration_meta \
                         SET metadata_format_version = 4, upgraded_at = CURRENT_TIMESTAMP \
                         WHERE singleton = 1 AND metadata_format_version = 3",
                        &[],
                    )
                    .await
                    .map_err(|error| {
                        Self::classify_statement_error(
                            error,
                            "recording PostgreSQL metadata format 4",
                        )
                    })?;
                if changed != 1 {
                    return Err(DbmError::Diverged(
                        "PostgreSQL metadata format changed during explicit upgrade".to_owned(),
                    ));
                }
            }

            self.client()?
                .batch_execute(POSTGRES_METADATA_V4_TO_V5_DDL)
                .await
                .map_err(|error| {
                    Self::classify_statement_error(
                        error,
                        "upgrading PostgreSQL metadata to format 5",
                    )
                })?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_meta \
                     SET metadata_format_version = 5, upgraded_at = CURRENT_TIMESTAMP \
                     WHERE singleton = 1 AND metadata_format_version = 4",
                    &[],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "recording PostgreSQL metadata format 5")
                })?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL metadata format changed during explicit v5 upgrade".to_owned(),
                ));
            }
            self.verify_session().await
        })
    }

    fn insert_planned_run<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
        run: PlannedRunData<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;
            let key = migration.migration_key();
            let attempt_row = self
                .client()?
                .query_one(
                    "SELECT COALESCE(MAX(attempt), 0) + 1 FROM schema_migration_runs \
                     WHERE namespace = $1 AND migration_key = $2 AND direction = $3",
                    &[&migration.identity.namespace, &key, &run.direction],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "reading migration attempt")
                })?;
            let attempt: i64 = attempt_row
                .try_get(0)
                .map_err(|_| metadata_row_error("next run attempt"))?;
            self.client()?
                .execute(
                    "INSERT INTO schema_migration_runs \
                     (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                      state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
                     VALUES ($1, $2, $3, $4, $5, $6, 'planned', $7, ($8::text)::timestamptz, $9, $10, 'human')",
                    &[
                        &run.run_id,
                        &run.batch_id,
                        &migration.identity.namespace,
                        &key,
                        &migration.identity.version,
                        &run.direction,
                        &attempt,
                        &run.created_at,
                        &migration.checksum,
                        &run.plan_hash,
                    ],
                )
                .await
                .map_err(|error| Self::classify_statement_error(error, "recording a planned migration run"))?;
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_runs SET state = 'started', started_at = ($1::text)::timestamptz \
                     WHERE run_id = $2 AND state = 'planned'",
                    &[&started_at, &run_id],
                )
                .await
                .map_err(|error| Self::classify_statement_error(error, "starting a migration run"))?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL run cannot transition from its current state to started".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn insert_step<'a>(&'a mut self, step: StepData<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;
            self.client()?
                .execute(
                    "INSERT INTO schema_migration_steps \
                      (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                       transaction_class, state, started_at, acknowledged_at, error_code, evidence, attempt_id) \
                      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, ($9::text)::timestamptz, \
                              CASE WHEN $10::text IS NULL THEN NULL ELSE ($10::text)::timestamptz END, $11, $12, $1)",
                    &[
                        &step.run_id,
                        &step.step_index,
                        &step.step_key,
                        &step.direction,
                        &step.statement_hash,
                        &step.statement_kind,
                        &step.transaction_class,
                        &step.state,
                        &step.started_at,
                        &step.acknowledged_at,
                        &step.error_code,
                        &step.evidence,
                    ],
                )
                .await
                .map_err(|error| Self::classify_statement_error(error, "recording a migration journal step"))?;
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_steps \
                     SET state = 'acknowledged', acknowledged_at = ($1::text)::timestamptz \
                     WHERE run_id = $2 AND step_index = $3 AND state = 'intent_recorded'",
                    &[&acknowledged_at, &run_id, &step_index],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "acknowledging a migration journal step")
                })?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL migration journal step cannot be acknowledged".to_owned(),
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_steps \
                     SET state = 'indeterminate', error_code = $1, evidence = $2 \
                     WHERE run_id = $3 AND step_index = $4 AND state = 'intent_recorded'",
                    &[&error_code, &evidence, &run_id, &step_index],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(
                        error,
                        "marking a migration journal step indeterminate",
                    )
                })?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL migration journal step cannot be marked indeterminate".to_owned(),
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_runs \
                     SET reconciled_from_state = state, state = 'reconciled', \
                         reconciliation_result = $1, reconciliation_plan = $2, \
                         reconciliation_evidence_ref = $3, reconciliation_reason = $4, \
                         reconciled_by = $5 \
                     WHERE run_id = $6 AND state <> 'reconciled'",
                    &[
                        &reconciliation.result,
                        &reconciliation.repair_plan,
                        &reconciliation.evidence_ref,
                        &reconciliation.reason,
                        &reconciliation.actor,
                        &reconciliation.run_id,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "reconciling a PostgreSQL migration run")
                })?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL run is missing or already reconciled".to_owned(),
                ));
            }

            let inserted = self
                .client()?
                .execute(
                    "INSERT INTO schema_migration_runs \
                     (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                      state, attempt, created_at, started_at, ended_at, duration_ms, \
                      checksum_at_execution, plan_hash, reconciliation_result, \
                      reconciliation_plan, reconciliation_evidence_ref, reconciliation_reason, \
                      reconciled_by, related_run_id, actor_kind) \
                     SELECT $1, batch_id, namespace, migration_key, migration_version, \
                            'reconcile', 'succeeded', 1, ($2::text)::timestamptz, \
                            ($2::text)::timestamptz, ($2::text)::timestamptz, 0, \
                            checksum_at_execution, plan_hash, $3, $4, $5, $6, $7, run_id, 'human' \
                     FROM schema_migration_runs WHERE run_id = $8",
                    &[
                        &reconciliation.reconcile_run_id,
                        &reconciliation.now,
                        &reconciliation.result,
                        &reconciliation.repair_plan,
                        &reconciliation.evidence_ref,
                        &reconciliation.reason,
                        &reconciliation.actor,
                        &reconciliation.run_id,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(
                        error,
                        "recording PostgreSQL reconciliation audit",
                    )
                })?;
            if inserted != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL reconciliation audit row could not be inserted".to_owned(),
                ));
            }
            self.client()?
                .execute(
                    "UPDATE schema_migration_steps SET state = 'reconciled', \
                     evidence = 'step state was reconciled by an approved operator decision' \
                     WHERE run_id = $1 AND state IN ('intent_recorded', 'indeterminate')",
                    &[&reconciliation.run_id],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "reconciling PostgreSQL journal steps")
                })?;
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_locks \
                     SET state = 'released', run_id = NULL, \
                         last_observed_at = ($1::text)::timestamptz \
                     WHERE scope = 'database' AND run_id = $2 AND state = 'observed_active'",
                    &[&released_at, &run_id],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(
                        error,
                        "releasing reconciled PostgreSQL lock metadata",
                    )
                })?;
            if changed != 1 {
                let active: bool = self
                    .client()?
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM schema_migration_locks \
                         WHERE scope = 'database' AND state = 'observed_active')",
                        &[],
                    )
                    .await
                    .map_err(|error| {
                        Self::classify_statement_error(error, "checking PostgreSQL lock metadata")
                    })?
                    .try_get(0)
                    .map_err(|_| metadata_row_error("active PostgreSQL lock metadata"))?;
                if active {
                    return Err(DbmError::Lock(
                        "active PostgreSQL lock metadata is not associated with the selected run"
                            .to_owned(),
                    ));
                }
            }
            Ok(())
        })
    }

    fn inspect_postgres_index<'a>(
        &'a mut self,
        create_statement: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;
            let (index_name, expected_table, expected_unique, expected_columns) =
                match parse_postgres_nontransactional_index_operation(create_statement)? {
                    PostgresNonTransactionalIndexOperation::Create {
                        index_name,
                        table_name,
                        unique,
                        columns,
                    } => (index_name, table_name, unique, columns),
                    PostgresNonTransactionalIndexOperation::Drop { .. } => {
                        return Err(DbmError::Safety(
                            "catalog reconciliation requires the migration's CREATE INDEX CONCURRENTLY up statement"
                                .to_owned(),
                        ));
                    }
                };
            let rows = self
                .client()?
                .query(
                    "SELECT index_schemas.nspname, table_schemas.nspname, \
                            table_relation.relname, index_data.indisvalid, index_data.indisready, \
                            index_data.indislive, index_data.indisunique, \
                            index_data.indnullsnotdistinct, access_methods.amname, \
                            index_data.indpred IS NULL, index_data.indexprs IS NULL, \
                            index_data.indnatts::bigint, index_data.indnkeyatts::bigint, \
                            keys.ordinality::bigint, attributes.attname::text, \
                            operator_classes.opcdefault, \
                            keys.collation_oid = attributes.attcollation AS default_collation, \
                            keys.option_bits = 0 AS default_order \
                     FROM pg_catalog.pg_class AS index_relation \
                     JOIN pg_catalog.pg_index AS index_data \
                       ON index_data.indexrelid = index_relation.oid \
                     JOIN pg_catalog.pg_namespace AS index_schemas \
                       ON index_schemas.oid = index_relation.relnamespace \
                     JOIN pg_catalog.pg_class AS table_relation \
                       ON table_relation.oid = index_data.indrelid \
                     JOIN pg_catalog.pg_namespace AS table_schemas \
                       ON table_schemas.oid = table_relation.relnamespace \
                     JOIN pg_catalog.pg_am AS access_methods \
                       ON access_methods.oid = index_relation.relam \
                     LEFT JOIN LATERAL unnest(\
                         index_data.indkey, index_data.indclass, \
                         index_data.indcollation, index_data.indoption\
                     ) WITH ORDINALITY AS keys(\
                         attnum, opclass_oid, collation_oid, option_bits, ordinality\
                     ) ON keys.ordinality <= index_data.indnkeyatts \
                     LEFT JOIN pg_catalog.pg_attribute AS attributes \
                       ON attributes.attrelid = table_relation.oid \
                      AND attributes.attnum = keys.attnum \
                     LEFT JOIN pg_catalog.pg_opclass AS operator_classes \
                       ON operator_classes.oid = keys.opclass_oid \
                     WHERE index_relation.oid = pg_catalog.to_regclass($1) \
                     ORDER BY keys.ordinality",
                    &[&index_name],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "inspecting a PostgreSQL migration index")
                })?;
            let Some(first_row) = rows.first() else {
                return Ok(false);
            };
            let index_schema: String = first_row
                .try_get(0)
                .map_err(|_| metadata_row_error("index schema"))?;
            let table_schema: String = first_row
                .try_get(1)
                .map_err(|_| metadata_row_error("index table schema"))?;
            let table_name: String = first_row
                .try_get(2)
                .map_err(|_| metadata_row_error("index table name"))?;
            let valid: bool = first_row
                .try_get(3)
                .map_err(|_| metadata_row_error("index validity"))?;
            let ready: bool = first_row
                .try_get(4)
                .map_err(|_| metadata_row_error("index readiness"))?;
            let live: bool = first_row
                .try_get(5)
                .map_err(|_| metadata_row_error("index liveness"))?;
            let unique: bool = first_row
                .try_get(6)
                .map_err(|_| metadata_row_error("index uniqueness"))?;
            let nulls_not_distinct: bool = first_row
                .try_get(7)
                .map_err(|_| metadata_row_error("index null uniqueness"))?;
            let access_method: String = first_row
                .try_get(8)
                .map_err(|_| metadata_row_error("index access method"))?;
            let no_predicate: bool = first_row
                .try_get(9)
                .map_err(|_| metadata_row_error("index predicate"))?;
            let no_expressions: bool = first_row
                .try_get(10)
                .map_err(|_| metadata_row_error("index expressions"))?;
            let attribute_count: i64 = first_row
                .try_get(11)
                .map_err(|_| metadata_row_error("index attribute count"))?;
            let key_count: i64 = first_row
                .try_get(12)
                .map_err(|_| metadata_row_error("index key count"))?;
            let mut actual_columns = Vec::with_capacity(rows.len());
            let mut default_index_options = true;
            for row in &rows {
                let column: Option<String> = row
                    .try_get(14)
                    .map_err(|_| metadata_row_error("index key column"))?;
                let default_opclass: Option<bool> = row
                    .try_get(15)
                    .map_err(|_| metadata_row_error("index operator class"))?;
                let default_collation: Option<bool> = row
                    .try_get(16)
                    .map_err(|_| metadata_row_error("index collation"))?;
                let default_order: Option<bool> = row
                    .try_get(17)
                    .map_err(|_| metadata_row_error("index key ordering"))?;
                match (column, default_opclass, default_collation, default_order) {
                    (Some(column), Some(true), Some(true), Some(true)) => {
                        actual_columns.push(column.to_ascii_lowercase());
                    }
                    _ => default_index_options = false,
                }
            }
            let current_schema: Option<String> = self
                .client()?
                .query_one("SELECT current_schema()::text", &[])
                .await
                .map_err(|_| driver_error("could not determine the PostgreSQL current schema"))?
                .try_get(0)
                .map_err(|_| metadata_row_error("current schema"))?;
            let Some(current_schema) = current_schema else {
                return Err(DbmError::ReconciliationRequired(
                    "PostgreSQL has no current schema for index reconciliation".to_owned(),
                ));
            };
            if !valid
                || !ready
                || !live
                || index_schema != current_schema
                || table_schema != current_schema
                || table_name != expected_table
                || unique != expected_unique
                || nulls_not_distinct
                || access_method != "btree"
                || !no_predicate
                || !no_expressions
                || attribute_count != key_count
                || actual_columns != expected_columns
                || !default_index_options
            {
                return Err(DbmError::ReconciliationRequired(format!(
                    "PostgreSQL index {index_name} exists but its validity, schema, table, columns, or definition does not prove the migration outcome"
                )));
            }
            Ok(true)
        })
    }

    fn insert_applied<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
        applied_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;
            let key = migration.migration_key();
            let order_row = self
                .client()?
                .query_one(
                    "SELECT COALESCE(MAX(apply_order), 0) + 1 FROM schema_migrations WHERE namespace = $1",
                    &[&migration.identity.namespace],
                )
                .await
                .map_err(|error| Self::classify_statement_error(error, "reading migration apply order"))?;
            let apply_order: i64 = order_row
                .try_get(0)
                .map_err(|_| metadata_row_error("next migration apply order"))?;
            let sql = match migration.kind {
                MigrationKind::Versioned => {
                    "INSERT INTO schema_migrations \
                     (namespace, migration_key, kind, native_version, repeatable_name, filename, source_path, description, \
                      authoring_type, content_checksum, apply_order, applied_at) \
                     VALUES ($1, $2, 'versioned', $3, NULL, $4, $5, $6, 'sql', $7, $8, ($9::text)::timestamptz)"
                }
                MigrationKind::Repeatable => {
                    "INSERT INTO schema_migrations \
                     (namespace, migration_key, kind, native_version, repeatable_name, filename, source_path, description, \
                      authoring_type, content_checksum, apply_order, applied_at) \
                     VALUES ($1, $2, 'repeatable', NULL, $3, $4, $5, $6, 'sql', $7, $8, ($9::text)::timestamptz) \
                     ON CONFLICT (namespace, migration_key) DO UPDATE SET \
                       repeatable_name = EXCLUDED.repeatable_name, filename = EXCLUDED.filename, \
                       source_path = EXCLUDED.source_path, description = EXCLUDED.description, \
                       content_checksum = EXCLUDED.content_checksum, apply_order = EXCLUDED.apply_order, \
                       applied_at = EXCLUDED.applied_at"
                }
            };
            self.client()?
                .execute(
                    sql,
                    &[
                        &migration.identity.namespace,
                        &key,
                        &migration.identity.version,
                        &migration.filename,
                        &migration.source_path,
                        &migration.description,
                        &migration.checksum,
                        &apply_order,
                        &applied_at,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "recording applied migration state")
                })?;
            Ok(())
        })
    }

    fn delete_applied<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_metadata_transaction()?;
            self.verify_session().await?;
            if migration.kind == MigrationKind::Repeatable {
                return Err(DbmError::Safety(
                    "repeatable migrations do not support automatic rollback".to_owned(),
                ));
            }
            let key = migration.migration_key();
            let changed = self
                .client()?
                .execute(
                    "DELETE FROM schema_migrations WHERE namespace = $1 AND migration_key = $2 AND native_version = $3",
                    &[&migration.identity.namespace, &key, &migration.identity.version],
                )
                .await
                .map_err(|error| Self::classify_statement_error(error, "removing rolled-back migration state"))?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "applied PostgreSQL migration state changed during rollback".to_owned(),
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
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_runs \
                     SET state = $1, ended_at = ($2::text)::timestamptz, duration_ms = $3, \
                         error_code = $4, error_summary_redacted = $5 \
                     WHERE run_id = $6 AND state = 'started'",
                    &[
                        &run.state,
                        &run.ended_at,
                        &run.duration_ms,
                        &run.error_code,
                        &run.error_summary_redacted,
                        &run.run_id,
                    ],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "finishing a migration run")
                })?;
            if changed != 1 {
                return Err(DbmError::Diverged(
                    "PostgreSQL run cannot transition from its current state to a terminal state"
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
                    "PostgreSQL lock metadata requires the session advisory lock".to_owned(),
                ));
            }
            self.verify_session().await?;
            let current_row = self
                .client()?
                .query_opt(
                    "SELECT state, lock_epoch FROM schema_migration_locks \
                     WHERE scope = 'database' FOR UPDATE",
                    &[],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "reading PostgreSQL lock metadata")
                })?;
            let current = match current_row {
                Some(row) => {
                    let state: String = row
                        .try_get(0)
                        .map_err(|_| metadata_row_error("lock state"))?;
                    let current_epoch: i64 = row
                        .try_get(1)
                        .map_err(|_| metadata_row_error("lock epoch"))?;
                    if state != "released" {
                        return Err(DbmError::Lock(
                            "PostgreSQL migration lock metadata is unresolved; inspect with doctor and reconcile explicitly"
                                .to_owned(),
                        ));
                    }
                    Some(current_epoch)
                }
                None => None,
            };
            let lock_epoch = match current {
                Some(current_epoch) => current_epoch.checked_add(1).ok_or_else(|| {
                    DbmError::Diverged("PostgreSQL lock epoch is exhausted".to_owned())
                })?,
                None => 1,
            };
            let session_id = self
                .info
                .session_id
                .as_deref()
                .map(|pid| format!("backend:{pid}"));
            match current.is_some() {
                true => {
                    self.client()?
                        .execute(
                            "UPDATE schema_migration_locks \
                             SET owner_id = $1, session_id_redacted = $2, \
                                 lock_strategy = 'postgres_advisory_session', \
                                 acquired_at = ($3::text)::timestamptz, \
                                 last_observed_at = ($3::text)::timestamptz, run_id = $4, \
                                 lock_epoch = $5, state = 'observed_active' \
                             WHERE scope = 'database'",
                            &[
                                &lock.owner_id,
                                &session_id,
                                &lock.now,
                                &lock.run_id,
                                &lock_epoch,
                            ],
                        )
                        .await
                        .map_err(|error| {
                            Self::classify_statement_error(
                                error,
                                "recording PostgreSQL lock ownership",
                            )
                        })?;
                }
                false => {
                    self.client()?
                        .execute(
                            "INSERT INTO schema_migration_locks \
                             (scope, owner_id, session_id_redacted, lock_strategy, acquired_at, \
                              last_observed_at, run_id, lock_epoch, state) \
                             VALUES ('database', $1, $2, 'postgres_advisory_session', \
                                     ($3::text)::timestamptz, ($3::text)::timestamptz, $4, $5, \
                                     'observed_active')",
                            &[
                                &lock.owner_id,
                                &session_id,
                                &lock.now,
                                &lock.run_id,
                                &lock_epoch,
                            ],
                        )
                        .await
                        .map_err(|error| {
                            Self::classify_statement_error(
                                error,
                                "recording PostgreSQL lock ownership",
                            )
                        })?;
                }
            }
            Ok(lock_epoch)
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
                    "PostgreSQL lock metadata requires the session advisory lock".to_owned(),
                ));
            }
            self.verify_session().await?;
            let changed = self
                .client()?
                .execute(
                    "UPDATE schema_migration_locks \
                     SET state = 'released', run_id = NULL, \
                         last_observed_at = ($1::text)::timestamptz \
                     WHERE scope = 'database' AND owner_id = $2 AND state = 'observed_active'",
                    &[&released_at, &owner_id],
                )
                .await
                .map_err(|error| {
                    Self::classify_statement_error(error, "releasing PostgreSQL lock metadata")
                })?;
            if changed != 1 {
                return Err(DbmError::Lock(
                    "PostgreSQL lock metadata could not confirm release for this owner".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn verify_session<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Err(driver_error("session is closed"));
            }
            if self
                .connection_task
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
            {
                if let Some(task) = self.connection_task.take() {
                    let _ = task.await;
                }
                return Err(DbmError::ReconciliationRequired(
                    "PostgreSQL connection task ended; session-bound state is no longer available"
                        .to_owned(),
                ));
            }
            let row = self
                .client()?
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "PostgreSQL pinned session could not be verified".to_owned(),
                    )
                })?;
            let current_pid: i32 = row.try_get(0).map_err(|_| {
                driver_error("PostgreSQL returned invalid session verification results")
            })?;
            let expected_pid = self
                .info
                .session_id
                .as_deref()
                .and_then(|pid| pid.parse::<i32>().ok());
            if expected_pid != Some(current_pid) {
                return Err(DbmError::ReconciliationRequired(
                    "PostgreSQL backend session changed; session-bound lock ownership is unknown"
                        .to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn acquire_lock<'a>(&'a mut self, timeout: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let lock_key = advisory_lock_key(&self.info)?;
            if self.held_lock.is_some() {
                return Err(DbmError::Lock(
                    "PostgreSQL session already owns an advisory lock".to_owned(),
                ));
            }
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                self.verify_session().await?;
                let row = self
                    .client()?
                    .query_one("SELECT pg_try_advisory_lock($1)", &[&lock_key])
                    .await
                    .map_err(|_| {
                        DbmError::ReconciliationRequired(
                            "PostgreSQL advisory-lock acquisition lost its pinned session"
                                .to_owned(),
                        )
                    })?;
                let acquired: bool = row.try_get(0).map_err(|_| {
                    driver_error("PostgreSQL returned invalid advisory-lock results")
                })?;
                if acquired {
                    self.held_lock = Some(lock_key);
                    self.verify_session().await?;
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(DbmError::Lock(
                        "timed out waiting for the PostgreSQL session advisory lock".to_owned(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }

    fn release_lock<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let lock_key = self.held_lock.ok_or_else(|| {
                DbmError::Lock("PostgreSQL session does not own an advisory lock".to_owned())
            })?;
            self.verify_session().await?;
            let row = self
                .client()?
                .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key])
                .await
                .map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "PostgreSQL advisory-lock release could not be confirmed".to_owned(),
                    )
                })?;
            let released: bool = row
                .try_get(0)
                .map_err(|_| driver_error("PostgreSQL returned invalid advisory-lock results"))?;
            if !released {
                return Err(DbmError::Lock(
                    "PostgreSQL reported that this session did not own the advisory lock"
                        .to_owned(),
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
                    "PostgreSQL session already has an open transaction".to_owned(),
                ));
            }
            self.verify_session().await?;
            self.client()?
                .batch_execute("BEGIN")
                .await
                .map_err(|error| Self::classify_statement_error(error, "starting a transaction"))?;
            self.transaction_open = true;
            self.verify_session().await?;
            Ok(())
        })
    }

    fn execute_statement<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if !self.transaction_open {
                return Err(DbmError::Safety(
                    "PostgreSQL migration statements require an open managed transaction"
                        .to_owned(),
                ));
            }
            self.verify_session().await?;
            self.client()?.batch_execute(sql).await.map_err(|error| {
                Self::classify_statement_error(error, "executing a migration statement")
            })?;
            self.verify_session().await
        })
    }

    fn execute_nontransactional_statement<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if self.held_lock.is_none() {
                return Err(DbmError::Safety(
                    "PostgreSQL non-transactional statements require the session advisory lock"
                        .to_owned(),
                ));
            }
            if self.transaction_open {
                return Err(DbmError::Safety(
                    "PostgreSQL non-transactional statements cannot run inside a managed transaction"
                        .to_owned(),
                ));
            }
            self.verify_session().await?;
            self.client()?.batch_execute(sql).await.map_err(|error| {
                Self::classify_statement_error(
                    error,
                    "executing a non-transactional migration statement",
                )
            })?;
            self.verify_session().await
        })
    }

    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            if !self.transaction_open {
                return Err(DbmError::Safety(
                    "PostgreSQL session has no open transaction to commit".to_owned(),
                ));
            }
            self.verify_session().await?;
            self.client()?.batch_execute("COMMIT").await.map_err(|_| {
                DbmError::ReconciliationRequired(
                    "PostgreSQL COMMIT response was lost or rejected; inspect state before retrying"
                        .to_owned(),
                )
            })?;
            self.transaction_open = false;
            Ok(())
        })
    }

    fn rollback<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Err(driver_error("session is closed"));
            }
            if !self.transaction_open {
                return Err(DbmError::Safety(
                    "PostgreSQL session has no open transaction to roll back".to_owned(),
                ));
            }
            self.client()?
                .batch_execute("ROLLBACK")
                .await
                .map_err(|_| {
                    DbmError::ReconciliationRequired(
                        "PostgreSQL ROLLBACK response could not be confirmed".to_owned(),
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
            if let Err(error) = self.finish_connection().await
                && close_error.is_none()
            {
                close_error = Some(error);
            }
            match close_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

impl Drop for PostgresSession {
    fn drop(&mut self) {
        self.client.take();
        if let Some(task) = self.connection_task.take() {
            task.abort();
        }
    }
}

fn prepare_connection_config(
    connection_string: &str,
    allow_insecure_transport: bool,
    environment: Environment,
) -> Result<(PostgresConfig, bool)> {
    let mut config = PostgresConfig::from_str(connection_string).map_err(|_| {
        DbmError::Configuration(
            "PostgreSQL connection settings are invalid; the connection value is redacted"
                .to_owned(),
        )
    })?;
    if config.get_hosts().is_empty() {
        return Err(DbmError::Configuration(
            "PostgreSQL connection settings must specify a hostname".to_owned(),
        ));
    }

    if config.get_ssl_mode() == SslMode::Disable {
        if !allow_insecure_transport
            || environment == Environment::Production
            || !all_hosts_are_local(&config)
        {
            return Err(DbmError::Safety(
                "plaintext PostgreSQL is allowed only with explicit opt-in for a local development/test target"
                    .to_owned(),
            ));
        }
        return Ok((config, false));
    }

    config.ssl_mode(SslMode::Require);
    Ok((config, true))
}

fn all_hosts_are_local(config: &PostgresConfig) -> bool {
    config.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) => is_loopback_name(host),
        Host::Unix(_) => true,
    }) && config.get_hostaddrs().iter().all(IpAddr::is_loopback)
}

fn postgres_target_identity(config: &PostgresConfig) -> String {
    let ports = config.get_ports();
    let mut hosts = Vec::with_capacity(config.get_hosts().len());
    for (index, host) in config.get_hosts().iter().enumerate() {
        let port = ports.get(index).copied().unwrap_or_default();
        let host = match host {
            Host::Tcp(host) => format!("tcp:{host}:{port}"),
            Host::Unix(path) => format!("unix:{}:{port}", path.display()),
        };
        hosts.push(host);
    }
    let database = config.get_dbname().unwrap_or("<default>");
    format!("postgres://{};database={database}", hosts.join(","))
}

fn is_loopback_name(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    IpAddr::from_str(host)
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

async fn make_tls_connector(ca_cert_environment_name: Option<&str>) -> Result<MakeRustlsConnect> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(environment_name) = ca_cert_environment_name {
        let path = std::env::var(environment_name).map_err(|_| {
            DbmError::Configuration(format!(
                "PostgreSQL CA certificate path variable {environment_name} is not set or not valid Unicode"
            ))
        })?;
        if path.trim().is_empty() {
            return Err(DbmError::Configuration(format!(
                "PostgreSQL CA certificate path variable {environment_name} is empty"
            )));
        }
        let certificates = run_blocking(move || {
            let file = std::fs::File::open(&path)
                .map_err(|_| driver_error("could not read the configured PostgreSQL CA file"))?;
            parse_ca_certificates(file)
        })
        .await?;
        for certificate in certificates {
            roots
                .add(certificate)
                .map_err(|_| driver_error("configured PostgreSQL CA certificate is invalid"))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| driver_error("could not initialize the PostgreSQL TLS provider"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(tls_config))
}

fn parse_ca_certificates(
    reader: impl Read,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let certificates = rustls::pki_types::CertificateDer::pem_reader_iter(reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| driver_error("configured PostgreSQL CA file is not valid PEM"))?;
    if certificates.is_empty() {
        return Err(driver_error(
            "configured PostgreSQL CA file contains no certificates",
        ));
    }
    Ok(certificates)
}

fn driver_error(message: &str) -> DbmError {
    DbmError::Driver {
        driver: "PostgreSQL",
        message: message.to_owned(),
    }
}

fn metadata_row_error(field: &str) -> DbmError {
    DbmError::Diverged(format!(
        "PostgreSQL migration metadata has an invalid {field}"
    ))
}

fn legacy_row_error(tool: &str, field: &str) -> DbmError {
    DbmError::InvalidMigration {
        path: format!("legacy:{tool}:{field}"),
        message: "legacy history contains a value with an unsupported type".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::time::Duration;

    use tokio_postgres::config::Config as PostgresConfig;

    use super::{
        PostgresAdapter, advisory_lock_key, all_hosts_are_local, parse_ca_certificates,
        prepare_connection_config,
    };
    use crate::config::{self, Environment};
    use crate::database::{DatabaseAdapter, DatabaseSession, SessionIntent};
    use crate::error::DbmError;
    use crate::identity::MigrationIdentity;
    use crate::legacy::{LegacyImportWrite, LegacyTool};
    use crate::migration::{
        Migration, MigrationFormat, MigrationKind, SqlDialect, TransactionPolicy,
    };
    use crate::storage::{FinishedRunData, LockData, PlannedRunData, StepData};
    use tokio_postgres::config::SslMode;

    #[test]
    fn parses_all_ca_certificates_and_ignores_other_pem_sections() {
        let pem = concat!(
            "-----BEGIN PRIVATE KEY-----\nAQID\n-----END PRIVATE KEY-----\n",
            "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
            "-----BEGIN CERTIFICATE-----\nBAUG\n-----END CERTIFICATE-----\n",
        );

        let certificates = parse_ca_certificates(Cursor::new(pem))
            .expect("parse every certificate and ignore non-certificate PEM sections");

        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].as_ref(), &[1, 2, 3]);
        assert_eq!(certificates[1].as_ref(), &[4, 5, 6]);
    }

    #[test]
    fn rejects_ca_pem_without_certificates() {
        let pem = "-----BEGIN PRIVATE KEY-----\nAQID\n-----END PRIVATE KEY-----\n";

        let error = parse_ca_certificates(Cursor::new(pem))
            .expect_err("a CA file without certificates must be rejected");

        assert!(matches!(
            error,
            DbmError::Driver {
                driver: "PostgreSQL",
                ..
            }
        ));
    }

    #[test]
    fn rejects_malformed_ca_pem() {
        let pem = "-----BEGIN CERTIFICATE-----\nnot-base64!\n-----END CERTIFICATE-----\n";

        let error = parse_ca_certificates(Cursor::new(pem))
            .expect_err("malformed certificate PEM must be rejected");

        assert!(matches!(
            error,
            DbmError::Driver {
                driver: "PostgreSQL",
                ..
            }
        ));
    }

    fn sample_postgres_migration() -> Migration {
        Migration {
            identity: MigrationIdentity {
                namespace: "default".to_owned(),
                version: "20261024120000000000".to_owned(),
            },
            kind: MigrationKind::Versioned,
            description: "create_probe".to_owned(),
            filename: "20261024120000000000_create_probe.sql".to_owned(),
            source_path: "migrations/versioned/20261024120000000000_create_probe.sql".to_owned(),
            checksum: "sha256:test-checksum".to_owned(),
            up_sql: "CREATE TABLE transactional_probe (id integer PRIMARY KEY);".to_owned(),
            down_sql: Some("DROP TABLE transactional_probe;".to_owned()),
            dialect: SqlDialect::Postgres,
            format: MigrationFormat::Single,
            transaction: TransactionPolicy::Always,
        }
    }

    fn local_postgres_config(
        allow_insecure_transport: bool,
        tls_ca_cert_env: Option<&str>,
    ) -> (tempfile::TempDir, crate::config::LoadedConfig) {
        let connection_string = std::env::var("DBM_TEST_DATABASE_URL")
            .expect("DBM_TEST_DATABASE_URL must identify a disposable local PostgreSQL target");
        let parsed = connection_string
            .parse::<PostgresConfig>()
            .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
        assert!(
            all_hosts_are_local(&parsed),
            "PostgreSQL integration tests refuse non-loopback hosts"
        );
        if allow_insecure_transport {
            assert_eq!(
                parsed.get_ssl_mode(),
                SslMode::Disable,
                "plaintext integration test requires sslmode=disable"
            );
        } else {
            assert_ne!(
                parsed.get_ssl_mode(),
                SslMode::Disable,
                "TLS integration test rejects sslmode=disable"
            );
        }

        let directory = tempfile::tempdir().expect("temporary project directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create empty migrations source");
        let tls_ca_config = match tls_ca_cert_env {
            Some(name) => format!("tls_ca_cert_env = '{name}'\n"),
            None => String::new(),
        };
        let config_text = format!(
            "version = 1\nnamespace = 'default'\nsource = ['migrations/versioned']\n\n[target]\ndriver = 'postgres'\nurl_env = 'DBM_TEST_DATABASE_URL'\nenvironment = 'test'\nallow_insecure_transport = {allow_insecure_transport}\n{tls_ca_config}"
        );
        let config_path = directory.path().join("dbm.toml");
        fs::write(&config_path, config_text).expect("write test-only config");
        let loaded = config::load(&config_path).expect("load test-only config");
        (directory, loaded)
    }

    async fn create_isolated_schema(
        adapter: &PostgresAdapter,
        config: &crate::config::LoadedConfig,
    ) -> String {
        let schema = format!("dbm_test_{}", uuid::Uuid::new_v4().simple());
        let mut session = adapter
            .connect(config, SessionIntent::ReadWrite)
            .await
            .expect("connect to create isolated test schema");
        session
            .begin()
            .await
            .expect("begin isolated schema creation transaction");
        session
            .execute_statement(&format!("CREATE SCHEMA {schema}"))
            .await
            .expect("create isolated PostgreSQL test schema");
        session
            .commit()
            .await
            .expect("commit isolated schema creation");
        session.close().await.expect("close schema setup session");
        schema
    }

    async fn connect_in_schema(
        adapter: &PostgresAdapter,
        config: &crate::config::LoadedConfig,
        schema: &str,
        intent: SessionIntent,
    ) -> Box<dyn DatabaseSession> {
        let mut session = adapter
            .connect(config, intent)
            .await
            .expect("connect to isolated PostgreSQL test schema");
        session
            .begin()
            .await
            .expect("begin isolated search-path transaction");
        session
            .execute_statement(&format!("SET search_path TO {schema}"))
            .await
            .expect("set isolated PostgreSQL schema search path");
        session
            .commit()
            .await
            .expect("commit isolated search-path setting");
        session
    }

    async fn drop_isolated_schema(
        adapter: &PostgresAdapter,
        config: &crate::config::LoadedConfig,
        schema: &str,
    ) {
        let mut session = adapter
            .connect(config, SessionIntent::ReadWrite)
            .await
            .expect("connect to remove isolated test schema");
        session
            .begin()
            .await
            .expect("begin isolated schema cleanup transaction");
        session
            .execute_statement(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .expect("drop isolated PostgreSQL test schema");
        session
            .commit()
            .await
            .expect("commit isolated schema cleanup");
        session.close().await.expect("close schema cleanup session");
    }

    #[test]
    fn requires_tls_and_forces_the_validating_connector_by_default() {
        let (config, use_tls) = prepare_connection_config(
            "host=db.example.test user=dbm dbname=app sslmode=prefer",
            false,
            Environment::Development,
        )
        .expect("secure connection configuration");

        assert!(use_tls);
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
    }

    #[test]
    fn rejects_plaintext_for_remote_and_production_targets() {
        let remote = prepare_connection_config(
            "host=db.example.test user=dbm dbname=app sslmode=disable",
            true,
            Environment::Test,
        );
        assert!(remote.is_err());

        let production = prepare_connection_config(
            "host=localhost user=dbm dbname=app sslmode=disable",
            true,
            Environment::Production,
        );
        assert!(production.is_err());
    }

    #[test]
    fn allows_plaintext_only_for_explicit_local_nonproduction_targets() {
        let (config, use_tls) = prepare_connection_config(
            "host=127.0.0.1 user=dbm dbname=app sslmode=disable",
            true,
            Environment::Test,
        )
        .expect("explicit local test target");

        assert!(!use_tls);
        assert_eq!(config.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn configuration_errors_never_echo_connection_credentials() {
        let error = prepare_connection_config(
            "postgresql://user:sentinel-credential@localhost:invalid/app",
            false,
            Environment::Test,
        )
        .expect_err("invalid connection settings");

        assert!(!error.to_string().contains("sentinel-credential"));
    }

    #[test]
    fn postgres_lock_key_is_stable_and_schema_scoped() {
        let info = super::DatabaseInfo {
            dialect: "postgres",
            server_version: "18".to_owned(),
            database: Some("application".to_owned()),
            schema: Some("public".to_owned()),
            target_identity: Some("postgres://tcp:localhost:5432".to_owned()),
            session_id: Some("123".to_owned()),
        };
        let first = advisory_lock_key(&info).expect("derive advisory lock key");
        let repeated = advisory_lock_key(&info).expect("derive same advisory lock key");
        assert_eq!(first, repeated);

        let mut another_schema = info.clone();
        another_schema.schema = Some("tenant".to_owned());
        assert_ne!(
            first,
            advisory_lock_key(&another_schema).expect("derive other schema lock key")
        );
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_session_keeps_advisory_lock_on_one_backend() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let mut read_session = adapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect read-only PostgreSQL session");
        assert_eq!(read_session.info().dialect, "postgres");
        let snapshot = read_session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read empty migration metadata");
        assert!(!snapshot.initialized);
        read_session
            .close()
            .await
            .expect("close read-only PostgreSQL session");

        let mut first =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        let mut second =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        first
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("first session acquires the advisory lock");
        assert!(matches!(
            second.acquire_lock(Duration::from_millis(150)).await,
            Err(DbmError::Lock(_))
        ));
        first
            .release_lock()
            .await
            .expect("first session releases its advisory lock");
        second
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("competing session acquires the released lock");
        second
            .release_lock()
            .await
            .expect("second session releases its advisory lock");
        first.close().await.expect("close first lock session");
        second.close().await.expect("close second lock session");
        drop_isolated_schema(&adapter, &config, &schema).await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_PGBOUNCER_URL through transaction pooling with DEFAULT_POOL_SIZE >= 2"]
    async fn local_pgbouncer_backend_change_is_rejected_after_session_lock_acquisition() {
        let (_directory, mut config) = local_postgres_config(true, None);
        let pooler_url = std::env::var("DBM_TEST_PGBOUNCER_URL")
            .expect("DBM_TEST_PGBOUNCER_URL must point to a disposable local PgBouncer");
        let pooler_config = pooler_url
            .parse::<PostgresConfig>()
            .expect("DBM_TEST_PGBOUNCER_URL must be a valid PostgreSQL connection string");
        assert!(
            all_hosts_are_local(&pooler_config),
            "PgBouncer integration tests refuse non-loopback hosts"
        );
        assert_eq!(
            pooler_config.get_ssl_mode(),
            SslMode::Disable,
            "local PgBouncer test requires sslmode=disable"
        );
        config.config.target.url_env = Some("DBM_TEST_PGBOUNCER_URL".to_owned());

        let adapter = PostgresAdapter;
        let mut session = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect through transaction-pooling PgBouncer");
        session
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("acquire advisory lock before adversarial backend switch");
        let original_pid = session
            .info()
            .session_id
            .as_deref()
            .expect("initial PostgreSQL backend PID")
            .parse::<i32>()
            .expect("valid initial PostgreSQL backend PID");
        let lock_key = advisory_lock_key(session.info()).expect("derive session advisory lock");

        let (mut blocker, blocker_connection) = pooler_config
            .connect(tokio_postgres::NoTls)
            .await
            .expect("connect a second PgBouncer client");
        let blocker_task = tokio::spawn(blocker_connection);
        let transaction = blocker
            .transaction()
            .await
            .expect("hold one transaction-pool backend");
        let blocked_pid: i32 = transaction
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("read the backend held by the competing transaction")
            .try_get(0)
            .expect("read competing backend PID");
        assert_eq!(
            blocked_pid, original_pid,
            "PgBouncer must assign the previously used backend to the competing transaction; configure DEFAULT_POOL_SIZE >= 2"
        );
        let lock_still_owned: bool = transaction
            .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key])
            .await
            .expect("verify session lock remained on the original backend")
            .try_get(0)
            .expect("read advisory unlock result");
        assert!(lock_still_owned, "PgBouncer reset the session-level lock");
        transaction
            .query_one("SELECT pg_advisory_lock($1)", &[&lock_key])
            .await
            .expect("restore the lock while the competing transaction owns the backend");

        let verification = tokio::time::timeout(Duration::from_secs(2), session.verify_session())
            .await
            .expect("the pooler should provide its second backend")
            .expect_err("the changed physical backend must be rejected");
        assert!(matches!(verification, DbmError::ReconciliationRequired(_)));

        let unlocked: bool = transaction
            .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key])
            .await
            .expect("release the isolated session lock before returning backend")
            .try_get(0)
            .expect("read advisory unlock result");
        assert!(unlocked);
        transaction
            .commit()
            .await
            .expect("release the pooler backend transaction");
        drop(blocker);
        blocker_task.abort();
        let _ = blocker_task.await;
        let _ = session.close().await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_session_loss_leaves_reconciliation_metadata() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let mut migration_session =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        let mut observer =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        let migration = sample_postgres_migration();
        migration_session
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("acquire session advisory lock");
        migration_session
            .begin()
            .await
            .expect("begin metadata bootstrap transaction");
        migration_session
            .initialize_metadata("2026-10-24T12:00:00Z")
            .await
            .expect("initialize metadata under advisory lock");
        migration_session
            .commit()
            .await
            .expect("commit metadata bootstrap");
        migration_session
            .begin()
            .await
            .expect("begin planned run transaction");
        migration_session
            .insert_planned_run(
                &migration,
                PlannedRunData {
                    run_id: "session-loss-run",
                    batch_id: "session-loss-batch",
                    direction: "up",
                    created_at: "2026-10-24T12:00:01Z",
                    plan_hash: "sha256:session-loss-plan",
                },
            )
            .await
            .expect("persist planned migration run");
        migration_session
            .record_lock(LockData {
                owner_id: "session-loss-owner",
                run_id: "session-loss-run",
                now: "2026-10-24T12:00:01Z",
            })
            .await
            .expect("persist active lock metadata");
        migration_session
            .commit()
            .await
            .expect("commit planned run and active lock metadata");
        migration_session
            .begin()
            .await
            .expect("begin run-start transaction");
        migration_session
            .mark_run_started("session-loss-run", "2026-10-24T12:00:02Z")
            .await
            .expect("persist started run state");
        migration_session
            .commit()
            .await
            .expect("commit started run state");

        let backend_pid = migration_session
            .info()
            .session_id
            .as_deref()
            .expect("pinned backend pid")
            .parse::<i32>()
            .expect("valid backend pid");
        observer
            .begin()
            .await
            .expect("begin observer termination transaction");
        observer
            .execute_statement(&format!("SELECT pg_terminate_backend({backend_pid})"))
            .await
            .expect("terminate the migration backend from the observer session");
        observer
            .commit()
            .await
            .expect("commit observer termination transaction");
        assert!(matches!(
            migration_session.verify_session().await,
            Err(DbmError::ReconciliationRequired(_))
        ));
        let _ = migration_session.close().await;

        let snapshot = observer
            .read_snapshot("default")
            .await
            .expect("read durable evidence after session loss");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "started");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("observed_active")
        );

        observer
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("server releases the physical session lock after backend loss");
        observer
            .release_lock()
            .await
            .expect("observer releases its physical lock");
        let snapshot = observer
            .read_snapshot("default")
            .await
            .expect("read lock metadata after physical lock is available");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("observed_active"),
            "session loss must not silently clear durable reconciliation evidence"
        );

        observer.close().await.expect("close observer session");
        drop_isolated_schema(&adapter, &config, &schema).await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_transaction_rolls_back_schema_and_keeps_session_lock() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let mut first =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        let mut second =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;

        first
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("first session acquires advisory lock");
        first.begin().await.expect("begin migration transaction");
        first
            .execute_statement("CREATE TABLE transactional_probe (id integer PRIMARY KEY)")
            .await
            .expect("create a table inside transaction");
        assert!(matches!(
            first.execute_statement("SELECT 1 / 0").await,
            Err(DbmError::Execution(_))
        ));
        first
            .rollback()
            .await
            .expect("roll back migration transaction after SQL failure");

        assert!(matches!(
            second.acquire_lock(Duration::from_millis(150)).await,
            Err(DbmError::Lock(_))
        ));

        first.begin().await.expect("begin verification transaction");
        first
            .execute_statement("CREATE TABLE transactional_probe (id integer PRIMARY KEY)")
            .await
            .expect("rolled-back table does not exist");
        first
            .commit()
            .await
            .expect("commit verification transaction");
        first
            .release_lock()
            .await
            .expect("release session advisory lock");
        second
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("second session acquires released advisory lock");
        second
            .release_lock()
            .await
            .expect("release second session advisory lock");

        first
            .begin()
            .await
            .expect("begin committed table cleanup transaction");
        first
            .execute_statement("DROP TABLE transactional_probe")
            .await
            .expect("remove committed verification table");
        first
            .commit()
            .await
            .expect("commit verification table cleanup");
        first.close().await.expect("close first session");
        second.close().await.expect("close second session");
        drop_isolated_schema(&adapter, &config, &schema).await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_reads_versioned_metadata_as_a_shared_snapshot() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let mut session =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        session
            .begin()
            .await
            .expect("begin unprotected bootstrap attempt");
        assert!(matches!(
            session.initialize_metadata("2026-10-24T12:00:00Z").await,
            Err(DbmError::Safety(_))
        ));
        session
            .rollback()
            .await
            .expect("roll back unprotected bootstrap attempt");
        session
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("acquire advisory lock before metadata bootstrap");
        session.begin().await.expect("begin metadata transaction");
        session
            .initialize_metadata("2026-10-24T12:00:00Z")
            .await
            .expect("initialize PostgreSQL metadata under lock");
        session
            .rollback()
            .await
            .expect("roll back PostgreSQL metadata bootstrap");
        let uninitialized = session
            .read_snapshot("default")
            .await
            .expect("read target after rolled-back metadata bootstrap");
        assert!(!uninitialized.initialized);

        session
            .begin()
            .await
            .expect("begin metadata retry transaction");
        session
            .initialize_metadata("2026-10-24T12:00:00Z")
            .await
            .expect("initialize PostgreSQL metadata under lock");
        session.commit().await.expect("commit metadata bootstrap");

        let migration = sample_postgres_migration();
        session
            .begin()
            .await
            .expect("begin PostgreSQL planned-run transaction");
        session
            .insert_planned_run(
                &migration,
                PlannedRunData {
                    run_id: "integration-run",
                    batch_id: "integration-batch",
                    direction: "up",
                    created_at: "2026-10-24T12:00:01Z",
                    plan_hash: "sha256:plan",
                },
            )
            .await
            .expect("persist planned run through PostgreSQL adapter");
        session
            .commit()
            .await
            .expect("commit PostgreSQL planned-run transaction");

        session
            .begin()
            .await
            .expect("begin PostgreSQL lock metadata transaction");
        assert_eq!(
            session
                .record_lock(LockData {
                    owner_id: "integration-owner",
                    run_id: "integration-run",
                    now: "2026-10-24T12:00:01.500000Z",
                })
                .await
                .expect("record PostgreSQL advisory-lock metadata"),
            1
        );
        session
            .commit()
            .await
            .expect("commit PostgreSQL lock metadata");

        session
            .begin()
            .await
            .expect("begin PostgreSQL run-start transaction");
        session
            .mark_run_started("integration-run", "2026-10-24T12:00:02Z")
            .await
            .expect("persist started run through PostgreSQL adapter");
        session
            .commit()
            .await
            .expect("commit PostgreSQL run-start transaction");

        session
            .begin()
            .await
            .expect("begin PostgreSQL migration transaction");
        session
            .insert_step(StepData {
                run_id: "integration-run",
                step_index: 0,
                step_key: "step-0",
                direction: "up",
                statement_hash: "sha256:step",
                statement_kind: "CREATE",
                transaction_class: "transactional",
                state: "intent_recorded",
                started_at: "2026-10-24T12:00:03Z",
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            })
            .await
            .expect("persist PostgreSQL step intent");
        session
            .execute_statement(&migration.up_sql)
            .await
            .expect("execute PostgreSQL migration statement");
        session
            .acknowledge_step("integration-run", 0, "2026-10-24T12:00:04Z")
            .await
            .expect("acknowledge PostgreSQL migration statement");
        session
            .insert_applied(&migration, "2026-10-24T12:00:04Z")
            .await
            .expect("persist PostgreSQL applied state");
        session
            .finish_run(FinishedRunData {
                run_id: "integration-run",
                state: "succeeded",
                ended_at: "2026-10-24T12:00:04Z",
                duration_ms: 1,
                error_code: None,
                error_summary_redacted: None,
            })
            .await
            .expect("persist PostgreSQL successful run");
        session
            .commit()
            .await
            .expect("commit PostgreSQL schema and migration state together");

        session
            .begin()
            .await
            .expect("begin PostgreSQL lock metadata release transaction");
        session
            .release_recorded_lock("integration-owner", "2026-10-24T12:00:05Z")
            .await
            .expect("record PostgreSQL advisory-lock release");
        session
            .commit()
            .await
            .expect("commit PostgreSQL lock metadata release");

        let snapshot = session
            .read_snapshot("default")
            .await
            .expect("read versioned PostgreSQL migration metadata");
        assert!(snapshot.initialized);
        assert_eq!(snapshot.applied.len(), 1);
        assert_eq!(snapshot.applied[0].version, "20261024120000000000");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].run_id, "integration-run");
        assert_eq!(snapshot.runs[0].state, "succeeded");
        assert_eq!(snapshot.steps.len(), 1);
        assert_eq!(snapshot.steps[0].state, "acknowledged");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("released")
        );
        session
            .begin()
            .await
            .expect("read snapshot leaves no transaction open");
        session
            .rollback()
            .await
            .expect("close verification transaction");

        session
            .begin()
            .await
            .expect("begin metadata corruption setup transaction");
        session
            .execute_statement("DROP TABLE schema_migration_steps")
            .await
            .expect("remove one metadata table to exercise fail-closed detection");
        session
            .commit()
            .await
            .expect("commit metadata corruption setup");
        assert!(matches!(
            session.read_snapshot("default").await,
            Err(DbmError::Diverged(_))
        ));
        session
            .begin()
            .await
            .expect("failed read snapshot rolls back its transaction");
        session
            .rollback()
            .await
            .expect("close post-error verification transaction");
        session
            .release_lock()
            .await
            .expect("release advisory lock after metadata read");
        session
            .close()
            .await
            .expect("close metadata reader session");
        drop_isolated_schema(&adapter, &config, &schema).await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_metadata_v3_and_v4_upgrades_preserve_native_rows_and_run_journal() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let mut session =
            connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
        session
            .acquire_lock(Duration::from_secs(2))
            .await
            .expect("acquire PostgreSQL session lock for metadata upgrade test");
        session
            .begin()
            .await
            .expect("begin initial metadata transaction");
        session
            .initialize_metadata("2026-10-24T12:00:00Z")
            .await
            .expect("initialize format 4 fixture");
        session
            .commit()
            .await
            .expect("commit format 4 fixture bootstrap");

        session.begin().await.expect("begin v3 fixture transaction");
        session
            .execute_statement(
                "INSERT INTO schema_migrations \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES ('default', repeat('a', 64), 'versioned', \
                         '20261024120000000000', '20261024120000000000_probe.sql', \
                         'migrations/versioned/20261024120000000000_probe.sql', \
                         'probe', 'sql', 'sha256:fixture', 1, CURRENT_TIMESTAMP); \
                 INSERT INTO schema_migration_runs \
                 (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                  state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
                 VALUES ('upgrade-run', 'upgrade-batch', 'default', repeat('a', 64), \
                         '20261024120000000000', 'up', 'succeeded', 1, CURRENT_TIMESTAMP, \
                         'sha256:fixture', 'sha256:plan', 'human'); \
                 INSERT INTO schema_migration_steps \
                 (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                  transaction_class, state, started_at, attempt_id) \
                 VALUES ('upgrade-run', 0, 'upgrade-step', 'up', 'sha256:step', 'CREATE', \
                         'transactional', 'acknowledged', CURRENT_TIMESTAMP, 'upgrade-attempt');",
            )
            .await
            .expect("seed native migration and run history");
        session
            .commit()
            .await
            .expect("commit native migration and run history");

        session
            .begin()
            .await
            .expect("begin downgrade-to-v3 fixture transaction");
        session
            .execute_statement(
                "DROP TABLE schema_migration_adoptions; \
                 DROP TABLE schema_migration_import_events; \
                 ALTER TABLE schema_migrations \
                     DROP CONSTRAINT schema_migrations_legacy_import_id_fkey; \
                 DROP TABLE schema_migration_imports; \
                 ALTER TABLE schema_migrations \
                     DROP CONSTRAINT schema_migrations_check; \
                 ALTER TABLE schema_migrations \
                     DROP COLUMN repeatable_name CASCADE; \
                 ALTER TABLE schema_migrations \
                     DROP CONSTRAINT schema_migrations_kind_check, \
                     DROP CONSTRAINT schema_migrations_authoring_type_check; \
                 ALTER TABLE schema_migrations \
                     DROP COLUMN original_version, DROP COLUMN origin_tool, \
                     DROP COLUMN origin_database_identity, DROP COLUMN origin_identity, \
                     DROP COLUMN legacy_import_id, DROP COLUMN batch_id, \
                     DROP COLUMN checksum_algorithm, DROP COLUMN checksum_value, \
                     DROP COLUMN execution_time_ms, DROP COLUMN verification_status, \
                     DROP COLUMN origin_applied_at, DROP COLUMN observed_at, \
                     ALTER COLUMN native_version SET NOT NULL, \
                     ALTER COLUMN filename SET NOT NULL, \
                     ALTER COLUMN source_path SET NOT NULL, \
                     ALTER COLUMN description SET NOT NULL, \
                     ALTER COLUMN content_checksum SET NOT NULL, \
                     ALTER COLUMN applied_at SET NOT NULL, \
                     ADD CONSTRAINT schema_migrations_kind_check CHECK (kind = 'versioned'), \
                     ADD CONSTRAINT schema_migrations_authoring_type_check CHECK (authoring_type = 'sql'); \
                 ALTER TABLE schema_migration_runs \
                     DROP CONSTRAINT schema_migration_runs_direction_check, \
                     ALTER COLUMN migration_version TYPE VARCHAR(20), \
                     ADD CONSTRAINT schema_migration_runs_direction_check \
                         CHECK (direction IN ('up', 'down', 'reconcile')); \
                 UPDATE schema_migration_meta SET metadata_format_version = 3 WHERE singleton = 1;",
            )
            .await
            .expect("convert the isolated v4 fixture to metadata format 3");
        session
            .commit()
            .await
            .expect("commit metadata format 3 fixture");

        let before = session
            .read_snapshot("default")
            .await
            .expect("read the format 3 fixture");
        assert_eq!(before.metadata_format_version, Some(3));
        assert_eq!(before.applied.len(), 1);
        assert_eq!(before.runs.len(), 1);
        assert_eq!(before.steps.len(), 1);

        session
            .begin()
            .await
            .expect("begin explicit format upgrade transaction");
        session
            .upgrade_metadata_to_current()
            .await
            .expect("upgrade PostgreSQL metadata from v3 through v5");
        session
            .commit()
            .await
            .expect("commit PostgreSQL metadata upgrade");
        let after = session
            .read_snapshot("default")
            .await
            .expect("read upgraded PostgreSQL metadata");
        assert_eq!(
            after.metadata_format_version,
            Some(crate::storage::METADATA_FORMAT_VERSION)
        );
        assert_eq!(after.applied.len(), 1);
        assert_eq!(after.runs.len(), 1);
        assert_eq!(after.steps.len(), 1);
        assert_eq!(after.applied[0].version, "20261024120000000000");
        assert_eq!(after.runs[0].run_id, "upgrade-run");

        session
            .begin()
            .await
            .expect("begin isolated v4 fixture conversion");
        session
            .execute_statement(
                "ALTER TABLE schema_migrations \
                    DROP CONSTRAINT schema_migrations_legacy_shape_check; \
                 ALTER TABLE schema_migrations \
                    DROP COLUMN repeatable_name CASCADE; \
                 ALTER TABLE schema_migrations \
                    DROP CONSTRAINT schema_migrations_kind_check, \
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
                         AND observed_at IS NOT NULL)); \
                 ALTER TABLE schema_migration_runs \
                    DROP CONSTRAINT schema_migration_runs_direction_check, \
                    ALTER COLUMN migration_version TYPE VARCHAR(20), \
                    ADD CONSTRAINT schema_migration_runs_direction_check \
                        CHECK (direction IN ('up', 'down', 'reconcile')); \
                 UPDATE schema_migration_meta \
                    SET metadata_format_version = 4 WHERE singleton = 1;",
            )
            .await
            .expect("restore the v4 metadata schema in the isolated test namespace");
        session
            .commit()
            .await
            .expect("commit isolated v4 fixture conversion");
        let v4_fixture = session
            .read_snapshot("default")
            .await
            .expect("read direct v4 fixture");
        assert_eq!(v4_fixture.metadata_format_version, Some(4));
        assert_eq!(v4_fixture.applied.len(), 1);
        assert_eq!(v4_fixture.runs.len(), 1);
        assert_eq!(v4_fixture.steps.len(), 1);

        session
            .begin()
            .await
            .expect("begin direct v4-to-v5 upgrade transaction");
        session
            .upgrade_metadata_to_current()
            .await
            .expect("upgrade PostgreSQL metadata directly from v4 to v5");
        session
            .commit()
            .await
            .expect("commit direct v4-to-v5 metadata upgrade");
        let after_v4 = session
            .read_snapshot("default")
            .await
            .expect("read direct v4-to-v5 upgrade result");
        assert_eq!(
            after_v4.metadata_format_version,
            Some(crate::storage::METADATA_FORMAT_VERSION)
        );
        assert_eq!(after_v4.applied.len(), 1);
        assert_eq!(after_v4.runs.len(), 1);
        assert_eq!(after_v4.steps.len(), 1);
        assert_eq!(after_v4.applied[0].version, "20261024120000000000");
        assert_eq!(after_v4.runs[0].run_id, "upgrade-run");

        session
            .release_lock()
            .await
            .expect("release PostgreSQL session lock");
        session
            .close()
            .await
            .expect("close PostgreSQL upgrade session");
        drop_isolated_schema(&adapter, &config, &schema).await;
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL and DBM_TEST_POSTGRES_CA_FILE for a local TLS PostgreSQL instance"]
    async fn local_postgres_tls_connection_validates_a_configured_ca() {
        let (_directory, config) = local_postgres_config(false, Some("DBM_TEST_POSTGRES_CA_FILE"));
        let mut session = PostgresAdapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect with a validated local PostgreSQL TLS certificate");
        assert_eq!(session.info().dialect, "postgres");
        assert!(!session.info().server_version.is_empty());
        session.close().await.expect("close TLS PostgreSQL session");

        let mut untrusted_config = config;
        untrusted_config.config.target.tls_ca_cert_env = None;
        assert!(matches!(
            PostgresAdapter
                .connect(&untrusted_config, SessionIntent::ReadOnly)
                .await,
            Err(DbmError::Driver { .. })
        ));
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_legacy_import_adapters_preserve_knex_goose_and_sqlx_history() {
        let (_directory, config) = local_postgres_config(true, None);
        let adapter = PostgresAdapter;
        let schema = create_isolated_schema(&adapter, &config).await;
        let result = async {
            let mut session =
                connect_in_schema(&adapter, &config, &schema, SessionIntent::ReadWrite).await;
            session.begin().await?;
            session
                .execute_statement(
                    "CREATE TABLE knex_migrations (\
                         id BIGINT PRIMARY KEY, name TEXT NOT NULL, batch BIGINT NOT NULL, \
                         migration_time TIMESTAMPTZ\
                     );\
                     CREATE TABLE knex_migrations_lock (\
                         \"index\" BIGINT PRIMARY KEY, is_locked BIGINT NOT NULL\
                     );\
                     INSERT INTO knex_migrations VALUES \
                         (1, '001_create_legacy_users.sql', 3, '2026-10-01T00:00:00Z'),\
                         (2, '002_create_legacy_orders.sql', 4, '2026-10-02T00:00:00Z');\
                     INSERT INTO knex_migrations_lock VALUES (1, 0);\
                     CREATE TABLE goose_db_version (\
                         id BIGINT PRIMARY KEY, version_id BIGINT NOT NULL, \
                         is_applied BOOLEAN NOT NULL, tstamp TIMESTAMPTZ\
                     );\
                     INSERT INTO goose_db_version VALUES \
                         (1, 0, TRUE, '2026-10-01T00:00:00Z'),\
                         (2, 100, TRUE, '2026-10-01T00:01:00Z'),\
                         (3, 200, TRUE, '2026-10-01T00:02:00Z'),\
                         (4, 100, FALSE, '2026-10-01T00:03:00Z');\
                     CREATE TABLE _sqlx_migrations (\
                         version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
                         installed_on TIMESTAMPTZ, success BOOLEAN NOT NULL, \
                         checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL\
                     );\
                     INSERT INTO _sqlx_migrations VALUES \
                         (42, 'create_accounts', '2026-10-03T00:00:00Z', TRUE, \
                          decode(repeat('ab', 48), 'hex'), 19);",
                )
                .await?;
            session.commit().await?;

            session.acquire_lock(Duration::from_secs(2)).await?;
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
                assert!(source.blockers.is_empty());
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
            session.begin().await?;
            session
                .execute_statement(
                    "INSERT INTO _sqlx_migrations VALUES \
                     (43, 'create_invoices', NULL, FALSE, decode(repeat('cd', 48), 'hex'), 0)",
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
                    "UPDATE knex_migrations_lock SET is_locked = 1 WHERE \"index\" = 1",
                )
                .await?;
            session.commit().await?;
            let active_knex_lock = session
                .inspect_legacy_history(LegacyTool::Knex, &format!("sha256:{}", "a".repeat(64)))
                .await?;
            assert!(
                active_knex_lock
                    .blockers
                    .iter()
                    .any(|blocker| blocker.code == "LEGACY_LOCK_ACTIVE")
            );
            session.close().await?;
            Ok::<(), DbmError>(())
        }
        .await;
        drop_isolated_schema(&adapter, &config, &schema).await;
        result.expect("PostgreSQL legacy import adapter conformance");
    }
}
