use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::LoadedConfig;
use crate::error::{DbmError, Result};

const IMPORT_SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LegacyTool {
    Knex,
    Goose,
    Sqlx,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyChecksum {
    pub(crate) algorithm: String,
    pub(crate) value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyEvent {
    pub(crate) event_id: String,
    pub(crate) original_version: String,
    pub(crate) state: LegacyEventState,
    pub(crate) filename: Option<String>,
    pub(crate) batch_id: Option<String>,
    pub(crate) applied_at: Option<String>,
    pub(crate) checksum: Option<LegacyChecksum>,
    pub(crate) execution_time_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LegacyEventState {
    Applied,
    Reverted,
    Failed,
    Sentinel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationStatus {
    Partial,
    Unverifiable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ImportedMigration {
    pub(crate) original_identity: String,
    pub(crate) original_version: String,
    pub(crate) filename: Option<String>,
    pub(crate) batch_id: Option<String>,
    pub(crate) applied_at: Option<String>,
    pub(crate) checksum: Option<LegacyChecksum>,
    pub(crate) execution_time_ms: Option<i64>,
    pub(crate) verification_status: VerificationStatus,
    pub(crate) rollback_blocked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ImportBlocker {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyImportSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) tool: LegacyTool,
    /// A digest of the database identity; raw connection details are never retained.
    pub(crate) source_identity: String,
    pub(crate) events: Vec<LegacyEvent>,
    pub(crate) current_migrations: Vec<ImportedMigration>,
    pub(crate) blockers: Vec<ImportBlocker>,
    pub(crate) digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyImportPlan {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) source: LegacyImportSnapshot,
    pub(crate) local_migrations: Vec<NativeMigrationFingerprint>,
    pub(crate) blockers: Vec<ImportBlocker>,
    pub(crate) digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyImportApplyReport {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) import_id: String,
    pub(crate) digest: String,
    pub(crate) event_count: usize,
    pub(crate) current_migration_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyImportVerifyReport {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) import_id: String,
    pub(crate) snapshot_digest: String,
    pub(crate) plan_digest: String,
    pub(crate) event_count: usize,
    pub(crate) current_migration_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyImportAdoptReport {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) import_id: String,
    pub(crate) legacy_identity: String,
    pub(crate) native_version: String,
    pub(crate) adoption_id: String,
}

pub(crate) struct LegacyAdoptRequest<'a> {
    pub(crate) tool: LegacyTool,
    pub(crate) import_id: &'a str,
    pub(crate) legacy_identity: &'a str,
    pub(crate) native_version: &'a str,
    pub(crate) evidence_ref: &'a str,
    pub(crate) reason: &'a str,
    pub(crate) actor: &'a str,
    pub(crate) approved: bool,
}

pub(crate) struct LegacyImportWrite<'a> {
    pub(crate) namespace: &'a str,
    pub(crate) import_id: &'a str,
    pub(crate) tool: LegacyTool,
    pub(crate) source: &'a LegacyImportSnapshot,
    pub(crate) plan_digest: &'a str,
    pub(crate) imported_at: &'a str,
}

pub(crate) struct LegacyAdoptionWrite<'a> {
    pub(crate) adoption_id: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) import_id: &'a str,
    pub(crate) legacy_migration_key: &'a str,
    pub(crate) native_migration_key: &'a str,
    pub(crate) native_version: &'a str,
    pub(crate) native_checksum: &'a str,
    pub(crate) evidence_ref: &'a str,
    pub(crate) actor: &'a str,
    pub(crate) reason: &'a str,
    pub(crate) adopted_at: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KnexRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) batch: i64,
    pub(crate) migration_time: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KnexLock {
    pub(crate) is_locked: bool,
    pub(crate) index: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GooseRow {
    pub(crate) id: i64,
    pub(crate) version_id: i64,
    pub(crate) is_applied: bool,
    pub(crate) timestamp: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SqlxRow {
    pub(crate) version: i64,
    pub(crate) description: String,
    pub(crate) installed_on: Option<String>,
    pub(crate) success: bool,
    pub(crate) checksum: Vec<u8>,
    pub(crate) execution_time_ms: i64,
}

pub(crate) fn inspect_from_config(
    config: &LoadedConfig,
    tool: LegacyTool,
) -> Result<LegacyImportSnapshot> {
    if config.config.target.driver != "sqlite" {
        return Err(DbmError::Unsupported(
            "legacy import inspection currently supports SQLite targets only".to_owned(),
        ));
    }
    let mut connection = crate::storage::open_readonly(config)?.ok_or_else(|| {
        DbmError::Configuration(
            "legacy import source database does not exist; inspection never creates it".to_owned(),
        )
    })?;
    let transaction = connection.transaction()?;
    let source_identity =
        source_identity_fingerprint(&config.target_description(), &config.config.namespace);

    let report = match tool {
        LegacyTool::Knex => {
            require_sqlite_table(&transaction, "knex_migrations")?;
            let mut statement = transaction.prepare(
                "SELECT id, name, batch, CAST(migration_time AS TEXT) \
                 FROM knex_migrations ORDER BY id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(KnexRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    batch: row.get(2)?,
                    migration_time: row.get(3)?,
                })
            })?;
            let mut migrations = Vec::new();
            for row in rows {
                migrations.push(row?);
            }

            let lock = if sqlite_table_exists(&transaction, "knex_migrations_lock")? {
                let mut statement = transaction.prepare(
                    "SELECT is_locked, \"index\" FROM knex_migrations_lock ORDER BY \"index\"",
                )?;
                let rows = statement.query_map([], |row| {
                    let value: i64 = row.get(0)?;
                    Ok(KnexLock {
                        is_locked: value != 0,
                        index: row.get(1)?,
                    })
                })?;
                let mut locks = Vec::new();
                for row in rows {
                    locks.push(row?);
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
            inspect_knex(&source_identity, migrations, lock)
        }
        LegacyTool::Goose => {
            require_sqlite_table(&transaction, "goose_db_version")?;
            let mut statement = transaction.prepare(
                "SELECT id, version_id, is_applied, CAST(tstamp AS TEXT) \
                 FROM goose_db_version ORDER BY id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(GooseRow {
                    id: row.get(0)?,
                    version_id: row.get(1)?,
                    is_applied: row.get::<_, i64>(2)? != 0,
                    timestamp: row.get(3)?,
                })
            })?;
            let mut events = Vec::new();
            for row in rows {
                events.push(row?);
            }
            inspect_goose(&source_identity, events)
        }
        LegacyTool::Sqlx => {
            require_sqlite_table(&transaction, "_sqlx_migrations")?;
            let mut statement = transaction.prepare(
                "SELECT version, description, CAST(installed_on AS TEXT), success, checksum, execution_time \
                 FROM _sqlx_migrations ORDER BY version",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(SqlxRow {
                    version: row.get(0)?,
                    description: row.get(1)?,
                    installed_on: row.get(2)?,
                    success: row.get::<_, i64>(3)? != 0,
                    checksum: row.get(4)?,
                    execution_time_ms: row.get(5)?,
                })
            })?;
            let mut migrations = Vec::new();
            for row in rows {
                migrations.push(row?);
            }
            inspect_sqlx(&source_identity, migrations)
        }
    };
    if report.is_ok() {
        transaction.commit()?;
    }
    report
}

pub(crate) async fn inspect_async(
    config: &LoadedConfig,
    tool: LegacyTool,
) -> Result<LegacyImportSnapshot> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        return crate::async_runtime::run_blocking(move || inspect_from_config(&config, tool))
            .await;
    }
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "legacy import inspection cannot verify registered runner migrations yet".to_owned(),
        ));
    }

    let adapter = crate::database::adapter_for(config)?;
    let mut session = adapter
        .connect(config, crate::database::SessionIntent::ReadOnly)
        .await?;
    let result = async {
        let source_identity = source_identity_for_session(config, session.as_ref())?;
        session.inspect_legacy_history(tool, &source_identity).await
    }
    .await;
    close_session_with_result(session.as_mut(), result).await
}

pub(crate) fn plan_from_config(
    config: &LoadedConfig,
    tool: LegacyTool,
) -> Result<LegacyImportPlan> {
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "legacy import planning cannot verify registered runner migrations yet".to_owned(),
        ));
    }
    let source = inspect_from_config(config, tool)?;
    let blockers = source.blockers.clone();
    let metadata = crate::storage::read_snapshot(config)?;
    build_plan_from_source(config, source, metadata.initialized, blockers)
}

pub(crate) async fn plan_async(
    config: &LoadedConfig,
    tool: LegacyTool,
) -> Result<LegacyImportPlan> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        return crate::async_runtime::run_blocking(move || plan_from_config(&config, tool)).await;
    }
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "legacy import planning cannot verify registered runner migrations yet".to_owned(),
        ));
    }

    let adapter = crate::database::adapter_for(config)?;
    let mut session = adapter
        .connect(config, crate::database::SessionIntent::ReadOnly)
        .await?;
    let result = async {
        let source_identity = source_identity_for_session(config, session.as_ref())?;
        let source = session
            .inspect_legacy_history(tool, &source_identity)
            .await?;
        let metadata = session.read_snapshot(&config.config.namespace).await?;
        build_plan_from_source(
            config,
            source.clone(),
            metadata.initialized,
            source.blockers.clone(),
        )
    }
    .await;
    close_session_with_result(session.as_mut(), result).await
}

fn build_plan_from_source(
    config: &LoadedConfig,
    source: LegacyImportSnapshot,
    metadata_initialized: bool,
    mut blockers: Vec<ImportBlocker>,
) -> Result<LegacyImportPlan> {
    if metadata_initialized {
        blockers.push(ImportBlocker {
            code: "TARGET_METADATA_ALREADY_INITIALIZED",
            message: "the target already has Database Migrator metadata; import is blocked"
                .to_owned(),
        });
    }

    let native_migrations = crate::source::discover(config)?;
    let mut imported_native_versions = Vec::new();
    for imported in &source.current_migrations {
        if crate::identity::validate_version(&imported.original_version).is_ok() {
            imported_native_versions.push(imported.original_version.clone());
        } else if let Some(filename) = imported.filename.as_deref()
            && let Some((prefix, _)) = filename.split_once('_')
            && crate::identity::validate_version(prefix).is_ok()
        {
            imported_native_versions.push(prefix.to_owned());
        }
    }
    imported_native_versions.sort();
    imported_native_versions.dedup();

    let mut native_fingerprints = Vec::with_capacity(native_migrations.len());
    let latest_imported = imported_native_versions.last().cloned();
    for migration in native_migrations {
        if imported_native_versions
            .iter()
            .any(|version| version == &migration.identity.version)
            || source.current_migrations.iter().any(|imported| {
                imported.original_version == migration.identity.version
                    || imported.filename.as_deref() == Some(migration.filename.as_str())
            })
        {
            blockers.push(ImportBlocker {
                code: "LEGACY_NATIVE_IDENTITY_COLLISION",
                message: format!(
                    "legacy history collides with local migration {} ({})",
                    migration.identity.version, migration.filename
                ),
            });
        }
        if latest_imported
            .as_deref()
            .is_some_and(|latest| migration.identity.version.as_str() < latest)
        {
            blockers.push(ImportBlocker {
                code: "NATIVE_MIGRATION_OUT_OF_ORDER",
                message: format!(
                    "local migration {} sorts before the latest timestamped legacy migration; review ordering before cutover",
                    migration.identity.version
                ),
            });
        }
        native_fingerprints.push(NativeMigrationFingerprint {
            namespace: migration.identity.namespace,
            version: migration.identity.version,
            filename: migration.filename,
            checksum: migration.checksum,
        });
    }
    blockers.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then_with(|| left.message.cmp(&right.message))
    });

    let digest_input = ImportPlanDigestInput {
        schema_version: IMPORT_SNAPSHOT_VERSION,
        source_digest: &source.digest,
        source_identity: &source.source_identity,
        namespace: &config.config.namespace,
        native_migrations: &native_fingerprints,
        blockers: &blockers,
    };
    let canonical = serde_json::to_vec(&digest_input)?;
    let digest = format!("sha256:{}", hex_digest(&Sha256::digest(canonical)));
    let status = if blockers.is_empty() {
        "ready"
    } else {
        "blocked"
    };
    Ok(LegacyImportPlan {
        schema_version: "1.0",
        command: "import plan",
        status,
        source,
        local_migrations: native_fingerprints,
        blockers,
        digest,
    })
}

pub(crate) fn apply_from_config(
    config: &LoadedConfig,
    tool: LegacyTool,
    approved_digest: &str,
) -> Result<LegacyImportApplyReport> {
    require_sqlite_import(config)?;
    crate::engine::require_mutation_environment(config)?;
    validate_approved_digest(approved_digest)?;

    let mut connection = crate::storage::open_mutable(config)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let source = inspect_from_config(config, tool)?;
    let current_metadata = crate::storage::read_snapshot(config)?;
    if current_metadata.initialized {
        if let Some(existing) = current_metadata.legacy_imports.iter().find(|import| {
            import.origin_tool == tool_name(tool)
                && import.source_identity == source.source_identity
        }) {
            let plan =
                build_plan_from_source(config, source.clone(), false, source.blockers.clone())?;
            if existing.snapshot_digest != source.digest || existing.plan_digest != plan.digest {
                return Err(DbmError::Diverged(
                    "an existing legacy import no longer matches its approved source and plan digests"
                        .to_owned(),
                ));
            }
            if approved_digest != existing.plan_digest {
                return Err(DbmError::Safety(
                    "approved import digest does not match the existing import".to_owned(),
                ));
            }
            verify_import_contents(&current_metadata, existing, &source)?;
            transaction.commit()?;
            return Ok(LegacyImportApplyReport {
                schema_version: "1.0",
                command: "import apply",
                status: "already_applied",
                import_id: existing.import_id.clone(),
                digest: existing.plan_digest.clone(),
                event_count: usize::try_from(existing.event_count).unwrap_or(0),
                current_migration_count: usize::try_from(existing.current_migration_count)
                    .unwrap_or(0),
            });
        }
        return Err(DbmError::Safety(
            "legacy import requires an uninitialized Database Migrator target".to_owned(),
        ));
    }

    let plan = build_plan_from_source(config, source.clone(), false, source.blockers.clone())?;
    if plan.status != "ready" {
        return Err(DbmError::Safety(format!(
            "legacy import plan is blocked: {}",
            blocker_summary(&plan.blockers)
        )));
    }
    if plan.digest != approved_digest {
        return Err(DbmError::Safety(
            "legacy source or native migration plan changed after approval; review a new `import plan` digest"
                .to_owned(),
        ));
    }

    let now = crate::identity::utc_timestamp()?;
    crate::storage::ensure_schema(&transaction, &now)?;
    let import_id = Uuid::new_v4().simple().to_string();
    let event_count = i64::try_from(source.events.len()).map_err(|_| {
        DbmError::Execution("legacy event count exceeds metadata storage limits".to_owned())
    })?;
    let current_migration_count = i64::try_from(source.current_migrations.len()).map_err(|_| {
        DbmError::Execution("legacy migration count exceeds metadata storage limits".to_owned())
    })?;
    transaction.execute(
        "INSERT INTO main.schema_migration_imports \
         (import_id, namespace, origin_tool, source_identity, snapshot_digest, plan_digest, \
          imported_at, event_count, current_migration_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            import_id,
            config.config.namespace,
            tool_name(tool),
            source.source_identity,
            source.digest,
            plan.digest,
            now,
            event_count,
            current_migration_count,
        ],
    )?;

    for (event_index, event) in source.events.iter().enumerate() {
        transaction.execute(
            "INSERT INTO main.schema_migration_import_events \
             (import_id, event_index, event_id, original_version, state, filename, batch_id, \
              applied_at, checksum_algorithm, checksum_value, execution_time_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                import_id,
                i64::try_from(event_index).map_err(|_| {
                    DbmError::Execution("legacy event index exceeds storage limits".to_owned())
                })?,
                event.event_id,
                event.original_version,
                event_state_name(event.state),
                event.filename,
                event.batch_id,
                event.applied_at,
                event
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.algorithm.as_str()),
                event
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.value.as_str()),
                event.execution_time_ms,
            ],
        )?;
    }

    for (position, migration) in source.current_migrations.iter().enumerate() {
        let migration_key = crate::identity::legacy_migration_key(
            &config.config.namespace,
            tool_name(tool),
            &source.source_identity,
            &migration.original_identity,
        );
        let apply_order = i64::try_from(position + 1).map_err(|_| {
            DbmError::Execution("legacy apply order exceeds metadata storage limits".to_owned())
        })?;
        transaction.execute(
            "INSERT INTO main.schema_migrations \
             (namespace, migration_key, kind, native_version, original_version, filename, \
              source_path, description, authoring_type, content_checksum, origin_tool, \
              origin_database_identity, origin_identity, legacy_import_id, batch_id, \
              checksum_algorithm, checksum_value, execution_time_ms, verification_status, \
              apply_order, applied_at, origin_applied_at, observed_at) \
             VALUES (?1, ?2, 'legacy', NULL, ?3, ?4, NULL, ?5, 'legacy_unknown', NULL, \
                     ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, NULL, ?16, ?17)",
            rusqlite::params![
                config.config.namespace,
                migration_key,
                migration.original_version,
                migration.filename,
                migration.filename,
                tool_name(tool),
                source.source_identity,
                migration.original_identity,
                import_id,
                migration.batch_id,
                migration
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.algorithm.as_str()),
                migration
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.value.as_str()),
                migration.execution_time_ms,
                verification_status_name(migration.verification_status),
                apply_order,
                migration.applied_at,
                now,
            ],
        )?;
    }

    transaction.commit()?;
    Ok(LegacyImportApplyReport {
        schema_version: "1.0",
        command: "import apply",
        status: "applied",
        import_id,
        digest: plan.digest,
        event_count: source.events.len(),
        current_migration_count: source.current_migrations.len(),
    })
}

pub(crate) async fn apply_async(
    config: &LoadedConfig,
    tool: LegacyTool,
    approved_digest: &str,
) -> Result<LegacyImportApplyReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        let approved_digest = approved_digest.to_owned();
        return crate::async_runtime::run_blocking(move || {
            apply_from_config(&config, tool, &approved_digest)
        })
        .await;
    }
    require_network_import(config)?;
    crate::engine::require_mutation_environment(config)?;
    validate_approved_digest(approved_digest)?;

    let adapter = crate::database::adapter_for(config)?;
    let mut session = adapter
        .connect(config, crate::database::SessionIntent::ReadWrite)
        .await?;
    let result = async {
        session
            .acquire_lock(std::time::Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
        session.begin().await?;

        let source_identity = source_identity_for_session(config, session.as_ref())?;
        let source = session
            .inspect_legacy_history(tool, &source_identity)
            .await?;
        let metadata = session.read_snapshot(&config.config.namespace).await?;

        if metadata.initialized {
            if metadata.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION) {
                return Err(DbmError::Safety(
                    "legacy import requires metadata format 4; explicitly upgrade existing metadata first"
                        .to_owned(),
                ));
            }
            if let Some(existing) = metadata.legacy_imports.iter().find(|import| {
                import.origin_tool == tool_name(tool)
                    && import.source_identity == source.source_identity
            }) {
                let plan = build_plan_from_source(
                    config,
                    source.clone(),
                    false,
                    source.blockers.clone(),
                )?;
                if existing.snapshot_digest != source.digest || existing.plan_digest != plan.digest {
                    return Err(DbmError::Diverged(
                        "an existing legacy import no longer matches its approved source and plan digests"
                            .to_owned(),
                    ));
                }
                if approved_digest != existing.plan_digest {
                    return Err(DbmError::Safety(
                        "approved import digest does not match the existing import".to_owned(),
                    ));
                }
                verify_import_contents(&metadata, existing, &source)?;
                session.rollback().await?;
                session.release_lock().await?;
                return Ok(LegacyImportApplyReport {
                    schema_version: "1.0",
                    command: "import apply",
                    status: "already_applied",
                    import_id: existing.import_id.clone(),
                    digest: existing.plan_digest.clone(),
                    event_count: usize::try_from(existing.event_count).unwrap_or(0),
                    current_migration_count: usize::try_from(existing.current_migration_count)
                        .unwrap_or(0),
                });
            }
            return Err(DbmError::Safety(
                "legacy import requires an uninitialized Database Migrator target".to_owned(),
            ));
        }

        let plan = build_plan_from_source(
            config,
            source.clone(),
            false,
            source.blockers.clone(),
        )?;
        if plan.status != "ready" {
            return Err(DbmError::Safety(format!(
                "legacy import plan is blocked: {}",
                blocker_summary(&plan.blockers)
            )));
        }
        if plan.digest != approved_digest {
            return Err(DbmError::Safety(
                "legacy source or native migration plan changed after approval; review a new `import plan` digest"
                    .to_owned(),
            ));
        }

        let imported_at = crate::identity::utc_timestamp()?;
        let import_id = Uuid::new_v4().simple().to_string();
        session.initialize_metadata(&imported_at).await?;
        session
            .persist_legacy_import(LegacyImportWrite {
                namespace: &config.config.namespace,
                import_id: &import_id,
                tool,
                source: &source,
                plan_digest: &plan.digest,
                imported_at: &imported_at,
            })
            .await?;
        session.commit().await?;
        session.release_lock().await?;
        Ok(LegacyImportApplyReport {
            schema_version: "1.0",
            command: "import apply",
            status: "applied",
            import_id,
            digest: plan.digest,
            event_count: source.events.len(),
            current_migration_count: source.current_migrations.len(),
        })
    }
    .await;
    close_session_with_result(session.as_mut(), result).await
}

pub(crate) fn verify_from_config(
    config: &LoadedConfig,
    tool: LegacyTool,
    import_id: &str,
) -> Result<LegacyImportVerifyReport> {
    require_sqlite_import(config)?;
    validate_nonempty_field(import_id, "import id")?;
    let source = inspect_from_config(config, tool)?;
    let metadata = crate::storage::read_snapshot(config)?;
    if !metadata.initialized
        || metadata.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
    {
        return Err(DbmError::Safety(
            "legacy import verification requires initialized metadata format 4".to_owned(),
        ));
    }
    let import = metadata
        .legacy_imports
        .iter()
        .find(|import| import.import_id == import_id)
        .ok_or_else(|| {
            DbmError::Configuration(format!("legacy import {import_id} was not found"))
        })?;
    if import.origin_tool != tool_name(tool) {
        return Err(DbmError::Safety(
            "requested legacy tool does not match the stored import".to_owned(),
        ));
    }
    verify_import_contents(&metadata, import, &source)?;
    let plan = build_plan_from_source(config, source.clone(), false, source.blockers.clone())?;
    if import.plan_digest != plan.digest {
        return Err(DbmError::Diverged(
            "local migration files or import blockers changed after the legacy import".to_owned(),
        ));
    }
    Ok(LegacyImportVerifyReport {
        schema_version: "1.0",
        command: "import verify",
        status: "verified",
        import_id: import.import_id.clone(),
        snapshot_digest: source.digest,
        plan_digest: plan.digest,
        event_count: usize::try_from(import.event_count).unwrap_or(0),
        current_migration_count: usize::try_from(import.current_migration_count).unwrap_or(0),
    })
}

pub(crate) async fn verify_async(
    config: &LoadedConfig,
    tool: LegacyTool,
    import_id: &str,
) -> Result<LegacyImportVerifyReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        let import_id = import_id.to_owned();
        return crate::async_runtime::run_blocking(move || {
            verify_from_config(&config, tool, &import_id)
        })
        .await;
    }
    require_network_import(config)?;
    validate_nonempty_field(import_id, "import id")?;

    let adapter = crate::database::adapter_for(config)?;
    let mut session = adapter
        .connect(config, crate::database::SessionIntent::ReadOnly)
        .await?;
    let result = async {
        let source_identity = source_identity_for_session(config, session.as_ref())?;
        let source = session
            .inspect_legacy_history(tool, &source_identity)
            .await?;
        let metadata = session.read_snapshot(&config.config.namespace).await?;
        if !metadata.initialized
            || metadata.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
        {
            return Err(DbmError::Safety(
                "legacy import verification requires initialized metadata format 4".to_owned(),
            ));
        }
        let import = metadata
            .legacy_imports
            .iter()
            .find(|import| import.import_id == import_id)
            .ok_or_else(|| {
                DbmError::Configuration(format!("legacy import {import_id} was not found"))
            })?;
        if import.origin_tool != tool_name(tool) {
            return Err(DbmError::Safety(
                "requested legacy tool does not match the stored import".to_owned(),
            ));
        }
        verify_import_contents(&metadata, import, &source)?;
        let plan = build_plan_from_source(config, source.clone(), false, source.blockers.clone())?;
        if import.plan_digest != plan.digest {
            return Err(DbmError::Diverged(
                "local migration files or import blockers changed after the legacy import"
                    .to_owned(),
            ));
        }
        Ok(LegacyImportVerifyReport {
            schema_version: "1.0",
            command: "import verify",
            status: "verified",
            import_id: import.import_id.clone(),
            snapshot_digest: source.digest,
            plan_digest: plan.digest,
            event_count: usize::try_from(import.event_count).unwrap_or(0),
            current_migration_count: usize::try_from(import.current_migration_count).unwrap_or(0),
        })
    }
    .await;
    close_session_with_result(session.as_mut(), result).await
}

pub(crate) fn adopt_from_config(
    config: &LoadedConfig,
    request: LegacyAdoptRequest<'_>,
) -> Result<LegacyImportAdoptReport> {
    let LegacyAdoptRequest {
        tool,
        import_id,
        legacy_identity,
        native_version,
        evidence_ref,
        reason,
        actor,
        approved,
    } = request;
    require_sqlite_import(config)?;
    crate::engine::require_mutation_environment(config)?;
    if !approved {
        return Err(DbmError::Safety(
            "legacy adoption requires explicit --yes approval".to_owned(),
        ));
    }
    for (value, name) in [
        (import_id, "import id"),
        (legacy_identity, "legacy identity"),
        (native_version, "native version"),
        (evidence_ref, "evidence reference"),
        (reason, "adoption reason"),
        (actor, "approving actor"),
    ] {
        validate_nonempty_field(value, name)?;
    }
    crate::identity::validate_version(native_version)?;

    let mut connection = crate::storage::open_mutable(config)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let source = inspect_from_config(config, tool)?;
    let metadata = crate::storage::read_snapshot(config)?;
    let import = metadata
        .legacy_imports
        .iter()
        .find(|import| import.import_id == import_id)
        .ok_or_else(|| {
            DbmError::Configuration(format!("legacy import {import_id} was not found"))
        })?;
    if import.origin_tool != tool_name(tool) {
        return Err(DbmError::Safety(
            "requested legacy tool does not match the stored import".to_owned(),
        ));
    }
    verify_import_contents(&metadata, import, &source)?;
    let plan = build_plan_from_source(config, source.clone(), false, source.blockers.clone())?;
    if import.plan_digest != plan.digest {
        return Err(DbmError::Diverged(
            "local migration files or import blockers changed after the legacy import".to_owned(),
        ));
    }

    let imported = source
        .current_migrations
        .iter()
        .find(|migration| migration.original_identity == legacy_identity)
        .ok_or_else(|| {
            DbmError::Configuration(format!(
                "legacy identity {legacy_identity} is not currently applied in the source"
            ))
        })?;
    let legacy_key = crate::identity::legacy_migration_key(
        &config.config.namespace,
        tool_name(tool),
        &source.source_identity,
        &imported.original_identity,
    );
    let applied_legacy = metadata
        .legacy_applied
        .iter()
        .find(|migration| migration.import_id == import_id && migration.migration_key == legacy_key)
        .ok_or_else(|| DbmError::Diverged("imported legacy migration row is missing".to_owned()))?;
    if applied_legacy.verification_status == "adopted" {
        let adoption_id: String = transaction.query_row(
            "SELECT adoption_id FROM main.schema_migration_adoptions \
             WHERE namespace = ?1 AND legacy_migration_key = ?2",
            rusqlite::params![config.config.namespace, legacy_key],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        return Ok(LegacyImportAdoptReport {
            schema_version: "1.0",
            command: "import adopt",
            status: "already_adopted",
            import_id: import_id.to_owned(),
            legacy_identity: legacy_identity.to_owned(),
            native_version: native_version.to_owned(),
            adoption_id,
        });
    }

    let migrations = crate::source::discover(config)?;
    let native = migrations
        .iter()
        .find(|migration| migration.identity.version == native_version)
        .ok_or_else(|| {
            DbmError::Configuration(format!(
                "native migration version {native_version} was not found in local sources"
            ))
        })?;
    if native.down_sql.is_none() {
        return Err(DbmError::Safety(format!(
            "native migration {native_version} has no Down section and cannot adopt rollback responsibility"
        )));
    }
    let applied_native = metadata
        .applied
        .iter()
        .find(|migration| migration.version == native_version)
        .ok_or_else(|| {
            DbmError::Safety(format!(
                "native migration {native_version} must be successfully applied before adoption"
            ))
        })?;
    if let Some(reason) = crate::engine::applied_mismatch(native, applied_native) {
        return Err(DbmError::Diverged(format!(
            "native migration {native_version} is not currently verified: {reason}"
        )));
    }

    let adoption_id = Uuid::new_v4().simple().to_string();
    let adopted_at = crate::identity::utc_timestamp()?;
    let changed = transaction.execute(
        "UPDATE main.schema_migrations SET verification_status = 'adopted' \
         WHERE namespace = ?1 AND migration_key = ?2 AND kind = 'legacy' \
           AND legacy_import_id = ?3 AND verification_status <> 'adopted'",
        rusqlite::params![config.config.namespace, legacy_key, import_id],
    )?;
    if changed != 1 {
        return Err(DbmError::Diverged(
            "legacy adoption state changed while the operation was in progress".to_owned(),
        ));
    }
    transaction.execute(
        "INSERT INTO main.schema_migration_adoptions \
         (adoption_id, namespace, import_id, legacy_migration_key, native_migration_key, \
          native_version, native_checksum, evidence_ref, approved_by, reason, adopted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            adoption_id,
            config.config.namespace,
            import_id,
            legacy_key,
            applied_native.migration_key,
            native_version,
            native.checksum,
            evidence_ref,
            actor,
            reason,
            adopted_at,
        ],
    )?;
    transaction.commit()?;
    Ok(LegacyImportAdoptReport {
        schema_version: "1.0",
        command: "import adopt",
        status: "adopted",
        import_id: import_id.to_owned(),
        legacy_identity: legacy_identity.to_owned(),
        native_version: native_version.to_owned(),
        adoption_id,
    })
}

pub(crate) async fn adopt_async(
    config: &LoadedConfig,
    request: LegacyAdoptRequest<'_>,
) -> Result<LegacyImportAdoptReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        let tool = request.tool;
        let import_id = request.import_id.to_owned();
        let legacy_identity = request.legacy_identity.to_owned();
        let native_version = request.native_version.to_owned();
        let evidence_ref = request.evidence_ref.to_owned();
        let reason = request.reason.to_owned();
        let actor = request.actor.to_owned();
        let approved = request.approved;
        return crate::async_runtime::run_blocking(move || {
            adopt_from_config(
                &config,
                LegacyAdoptRequest {
                    tool,
                    import_id: &import_id,
                    legacy_identity: &legacy_identity,
                    native_version: &native_version,
                    evidence_ref: &evidence_ref,
                    reason: &reason,
                    actor: &actor,
                    approved,
                },
            )
        })
        .await;
    }
    require_network_import(config)?;
    crate::engine::require_mutation_environment(config)?;
    let LegacyAdoptRequest {
        tool,
        import_id,
        legacy_identity,
        native_version,
        evidence_ref,
        reason,
        actor,
        approved,
    } = request;
    if !approved {
        return Err(DbmError::Safety(
            "legacy adoption requires explicit --yes approval".to_owned(),
        ));
    }
    for (value, name) in [
        (import_id, "import id"),
        (legacy_identity, "legacy identity"),
        (native_version, "native version"),
        (evidence_ref, "evidence reference"),
        (reason, "adoption reason"),
        (actor, "approving actor"),
    ] {
        validate_nonempty_field(value, name)?;
    }
    crate::identity::validate_version(native_version)?;

    let migrations = {
        let config = config.clone();
        crate::async_runtime::run_blocking(move || crate::source::discover(&config)).await?
    };
    let adapter = crate::database::adapter_for(config)?;
    let mut session = adapter
        .connect(config, crate::database::SessionIntent::ReadWrite)
        .await?;
    let result = async {
        session
            .acquire_lock(std::time::Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
        session.begin().await?;

        let source_identity = source_identity_for_session(config, session.as_ref())?;
        let source = session
            .inspect_legacy_history(tool, &source_identity)
            .await?;
        let metadata = session.read_snapshot(&config.config.namespace).await?;
        if !metadata.initialized
            || metadata.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
        {
            return Err(DbmError::Safety(
                "legacy adoption requires initialized metadata format 4".to_owned(),
            ));
        }
        let import = metadata
            .legacy_imports
            .iter()
            .find(|import| import.import_id == import_id)
            .ok_or_else(|| {
                DbmError::Configuration(format!("legacy import {import_id} was not found"))
            })?;
        if import.origin_tool != tool_name(tool) {
            return Err(DbmError::Safety(
                "requested legacy tool does not match the stored import".to_owned(),
            ));
        }
        verify_import_contents(&metadata, import, &source)?;
        let plan = build_plan_from_source(
            config,
            source.clone(),
            false,
            source.blockers.clone(),
        )?;
        if import.plan_digest != plan.digest {
            return Err(DbmError::Diverged(
                "local migration files or import blockers changed after the legacy import"
                    .to_owned(),
            ));
        }

        let imported = source
            .current_migrations
            .iter()
            .find(|migration| migration.original_identity == legacy_identity)
            .ok_or_else(|| {
                DbmError::Configuration(format!(
                    "legacy identity {legacy_identity} is not currently applied in the source"
                ))
            })?;
        let legacy_key = crate::identity::legacy_migration_key(
            &config.config.namespace,
            tool_name(tool),
            &source.source_identity,
            &imported.original_identity,
        );
        let applied_legacy = metadata
            .legacy_applied
            .iter()
            .find(|migration| {
                migration.import_id == import_id && migration.migration_key == legacy_key
            })
            .ok_or_else(|| DbmError::Diverged("imported legacy migration row is missing".to_owned()))?;
        if applied_legacy.verification_status == "adopted" {
            let adoption_id = session
                .read_legacy_adoption_id(&config.config.namespace, &legacy_key)
                .await?
                .ok_or_else(|| {
                    DbmError::Diverged(
                        "adopted legacy migration has no adoption audit record".to_owned(),
                    )
                })?;
            session.rollback().await?;
            session.release_lock().await?;
            return Ok(LegacyImportAdoptReport {
                schema_version: "1.0",
                command: "import adopt",
                status: "already_adopted",
                import_id: import_id.to_owned(),
                legacy_identity: legacy_identity.to_owned(),
                native_version: native_version.to_owned(),
                adoption_id,
            });
        }

        let native = migrations
            .iter()
            .find(|migration| migration.identity.version == native_version)
            .ok_or_else(|| {
                DbmError::Configuration(format!(
                    "native migration version {native_version} was not found in local sources"
                ))
            })?;
        if native.down_sql.is_none() {
            return Err(DbmError::Safety(format!(
                "native migration {native_version} has no Down section and cannot adopt rollback responsibility"
            )));
        }
        let applied_native = metadata
            .applied
            .iter()
            .find(|migration| migration.version == native_version)
            .ok_or_else(|| {
                DbmError::Safety(format!(
                    "native migration {native_version} must be successfully applied before adoption"
                ))
            })?;
        if let Some(mismatch) = crate::engine::applied_mismatch(native, applied_native) {
            return Err(DbmError::Diverged(format!(
                "native migration {native_version} is not currently verified: {mismatch}"
            )));
        }

        let adoption_id = Uuid::new_v4().simple().to_string();
        let adopted_at = crate::identity::utc_timestamp()?;
        session
            .persist_legacy_adoption(LegacyAdoptionWrite {
                adoption_id: &adoption_id,
                namespace: &config.config.namespace,
                import_id,
                legacy_migration_key: &legacy_key,
                native_migration_key: &applied_native.migration_key,
                native_version,
                native_checksum: &native.checksum,
                evidence_ref,
                actor,
                reason,
                adopted_at: &adopted_at,
            })
            .await?;
        session.commit().await?;
        session.release_lock().await?;
        Ok(LegacyImportAdoptReport {
            schema_version: "1.0",
            command: "import adopt",
            status: "adopted",
            import_id: import_id.to_owned(),
            legacy_identity: legacy_identity.to_owned(),
            native_version: native_version.to_owned(),
            adoption_id,
        })
    }
    .await;
    close_session_with_result(session.as_mut(), result).await
}

fn require_sqlite_import(config: &LoadedConfig) -> Result<()> {
    if config.config.target.driver != "sqlite" {
        return Err(DbmError::Unsupported(
            "legacy import apply, verify, and adopt currently support SQLite targets only"
                .to_owned(),
        ));
    }
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "legacy import operations cannot verify registered runner migrations yet".to_owned(),
        ));
    }
    config.sqlite_database_path()?;
    Ok(())
}

fn validate_approved_digest(digest: &str) -> Result<()> {
    if digest.len() != 71
        || !digest.starts_with("sha256:")
        || !digest[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DbmError::Configuration(
            "approved import digest must be a 64-digit sha256 value from `dbm import plan`"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_nonempty_field(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(DbmError::Configuration(format!(
            "{label} must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

fn blocker_summary(blockers: &[ImportBlocker]) -> String {
    if blockers.is_empty() {
        return "unspecified legacy import blocker".to_owned();
    }
    blockers
        .iter()
        .map(|blocker| format!("{}: {}", blocker.code, blocker.message))
        .collect::<Vec<_>>()
        .join("; ")
}

pub(crate) fn tool_name(tool: LegacyTool) -> &'static str {
    match tool {
        LegacyTool::Knex => "knex",
        LegacyTool::Goose => "goose",
        LegacyTool::Sqlx => "sqlx",
    }
}

pub(crate) fn event_state_name(state: LegacyEventState) -> &'static str {
    match state {
        LegacyEventState::Applied => "applied",
        LegacyEventState::Reverted => "reverted",
        LegacyEventState::Failed => "failed",
        LegacyEventState::Sentinel => "sentinel",
    }
}

pub(crate) fn verification_status_name(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Partial => "partial",
        VerificationStatus::Unverifiable => "unverifiable",
    }
}

pub(crate) fn parse_legacy_bool(value: &str, tool: &str, field: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" => Ok(true),
        "0" | "false" | "f" | "no" => Ok(false),
        _ => Err(DbmError::InvalidMigration {
            path: format!("legacy:{tool}:{field}"),
            message: "source boolean field has an unsupported representation".to_owned(),
        }),
    }
}

fn verify_import_contents(
    metadata: &crate::storage::MetadataSnapshot,
    import: &crate::storage::LegacyImportRecord,
    source: &LegacyImportSnapshot,
) -> Result<()> {
    if import.snapshot_digest != source.digest || import.source_identity != source.source_identity {
        return Err(DbmError::Diverged(
            "legacy source history or target identity changed after import".to_owned(),
        ));
    }
    let events: Vec<_> = metadata
        .legacy_import_events
        .iter()
        .filter(|event| event.import_id == import.import_id)
        .collect();
    if usize::try_from(import.event_count).ok() != Some(source.events.len())
        || events.len() != source.events.len()
    {
        return Err(DbmError::Diverged(
            "legacy event timeline count differs from the imported snapshot".to_owned(),
        ));
    }
    for (index, (stored, observed)) in events.iter().zip(&source.events).enumerate() {
        let expected_index = i64::try_from(index).map_err(|_| {
            DbmError::Execution("legacy event index exceeds storage limits".to_owned())
        })?;
        if stored.event_index != expected_index
            || stored.event_id != observed.event_id
            || stored.original_version != observed.original_version
            || stored.state != event_state_name(observed.state)
            || stored.filename != observed.filename
            || stored.batch_id != observed.batch_id
            || stored.applied_at != observed.applied_at
            || stored.checksum_algorithm
                != observed
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.algorithm.clone())
            || stored.checksum_value
                != observed
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.value.clone())
            || stored.execution_time_ms != observed.execution_time_ms
        {
            return Err(DbmError::Diverged(format!(
                "legacy source event {} differs from its imported timeline",
                observed.event_id
            )));
        }
    }

    let migrations: Vec<_> = metadata
        .legacy_applied
        .iter()
        .filter(|migration| migration.import_id == import.import_id)
        .collect();
    if usize::try_from(import.current_migration_count).ok() != Some(source.current_migrations.len())
        || migrations.len() != source.current_migrations.len()
    {
        return Err(DbmError::Diverged(
            "current legacy migration rows differ from the imported snapshot".to_owned(),
        ));
    }
    for observed in &source.current_migrations {
        let expected_key = crate::identity::legacy_migration_key(
            &import.namespace,
            &import.origin_tool,
            &source.source_identity,
            &observed.original_identity,
        );
        let stored = migrations
            .iter()
            .find(|migration| migration.migration_key == expected_key)
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "imported legacy identity {} is missing from current state",
                    observed.original_identity
                ))
            })?;
        let expected_status = verification_status_name(observed.verification_status);
        if stored.origin_identity != observed.original_identity
            || stored.origin_tool != import.origin_tool
            || stored.source_identity != source.source_identity
            || stored.original_version != observed.original_version
            || stored.filename != observed.filename
            || stored.batch_id != observed.batch_id
            || stored.applied_at != observed.applied_at
            || stored.checksum_algorithm
                != observed
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.algorithm.clone())
            || stored.checksum_value
                != observed
                    .checksum
                    .as_ref()
                    .map(|checksum| checksum.value.clone())
            || stored.execution_time_ms != observed.execution_time_ms
            || (stored.verification_status != expected_status
                && stored.verification_status != "adopted")
        {
            return Err(DbmError::Diverged(format!(
                "imported legacy identity {} differs from the current source state",
                observed.original_identity
            )));
        }
    }
    Ok(())
}

pub(crate) fn inspect_knex(
    source_identity: &str,
    mut rows: Vec<KnexRow>,
    lock: Option<KnexLock>,
) -> Result<LegacyImportSnapshot> {
    validate_source_identity(source_identity)?;
    sort_and_validate_ids(&mut rows, |row| row.id, "Knex")?;

    let mut events = Vec::with_capacity(rows.len());
    let mut current_migrations = Vec::with_capacity(rows.len());
    for row in rows {
        let original_identity = row.id.to_string();
        let original_version = row.name.clone();
        events.push(LegacyEvent {
            event_id: original_identity.clone(),
            original_version: original_version.clone(),
            state: LegacyEventState::Applied,
            filename: Some(row.name.clone()),
            batch_id: Some(row.batch.to_string()),
            applied_at: row.migration_time.clone(),
            checksum: None,
            execution_time_ms: None,
        });
        current_migrations.push(ImportedMigration {
            original_identity,
            original_version,
            filename: Some(row.name),
            batch_id: Some(row.batch.to_string()),
            applied_at: row.migration_time,
            checksum: None,
            execution_time_ms: None,
            verification_status: VerificationStatus::Unverifiable,
            rollback_blocked: true,
        });
    }

    let mut blockers = Vec::new();
    match lock {
        Some(lock) if lock.is_locked => {
            blockers.push(ImportBlocker {
                code: "LEGACY_LOCK_ACTIVE",
                message: match lock.index {
                    Some(index) => {
                        format!("Knex reports an active migration lock at index {index}")
                    }
                    None => "Knex reports an active migration lock".to_owned(),
                },
            });
        }
        Some(_) => {}
        None => {
            blockers.push(ImportBlocker {
                code: "LEGACY_LOCK_STATE_UNKNOWN",
                message: "Knex lock state is unavailable; cutover cannot establish that no legacy migration is active".to_owned(),
            });
        }
    }

    build_snapshot(
        LegacyTool::Knex,
        source_identity,
        events,
        current_migrations,
        blockers,
    )
}

pub(crate) fn inspect_goose(
    source_identity: &str,
    mut rows: Vec<GooseRow>,
) -> Result<LegacyImportSnapshot> {
    validate_source_identity(source_identity)?;
    sort_and_validate_ids(&mut rows, |row| row.id, "Goose")?;

    let mut events = Vec::with_capacity(rows.len());
    let mut latest_by_version = HashMap::<i64, usize>::new();
    for row in rows {
        let state = if row.version_id == 0 {
            LegacyEventState::Sentinel
        } else if row.is_applied {
            LegacyEventState::Applied
        } else {
            LegacyEventState::Reverted
        };
        let event_index = events.len();
        events.push(LegacyEvent {
            event_id: row.id.to_string(),
            original_version: row.version_id.to_string(),
            state,
            filename: None,
            batch_id: None,
            applied_at: row.timestamp,
            checksum: None,
            execution_time_ms: None,
        });
        if row.version_id != 0 {
            latest_by_version.insert(row.version_id, event_index);
        }
    }

    let mut current_migrations = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let version = event.original_version.parse::<i64>().map_err(|_| {
            invalid_legacy_row(
                "Goose",
                &event.event_id,
                "version_id is not a valid integer",
            )
        })?;
        let is_latest = latest_by_version.get(&version) == Some(&index);
        if is_latest && event.state == LegacyEventState::Applied {
            current_migrations.push(ImportedMigration {
                original_identity: event.event_id.clone(),
                original_version: event.original_version.clone(),
                filename: None,
                batch_id: None,
                applied_at: event.applied_at.clone(),
                checksum: None,
                execution_time_ms: None,
                verification_status: VerificationStatus::Unverifiable,
                rollback_blocked: true,
            });
        }
    }

    build_snapshot(
        LegacyTool::Goose,
        source_identity,
        events,
        current_migrations,
        Vec::new(),
    )
}

pub(crate) fn inspect_sqlx(
    source_identity: &str,
    mut rows: Vec<SqlxRow>,
) -> Result<LegacyImportSnapshot> {
    validate_source_identity(source_identity)?;
    sort_and_validate_ids(&mut rows, |row| row.version, "SQLx")?;

    let mut events = Vec::with_capacity(rows.len());
    let mut current_migrations = Vec::with_capacity(rows.len());
    let mut blockers = Vec::new();
    for row in rows {
        if row.checksum.len() != 48 {
            return Err(invalid_legacy_row(
                "SQLx",
                &row.version.to_string(),
                "checksum is not a 48-byte SHA-384 digest",
            ));
        }
        if row.execution_time_ms < 0 {
            return Err(invalid_legacy_row(
                "SQLx",
                &row.version.to_string(),
                "execution_time is negative",
            ));
        }
        let checksum = LegacyChecksum {
            algorithm: "sha384".to_owned(),
            value: hex_digest(&row.checksum),
        };
        let version = row.version.to_string();
        let event_state = if row.success {
            LegacyEventState::Applied
        } else {
            blockers.push(ImportBlocker {
                code: "SQLX_MIGRATION_FAILED",
                message: format!(
                    "SQLx migration version {version} is marked success=false; reconcile it before cutover"
                ),
            });
            LegacyEventState::Failed
        };
        events.push(LegacyEvent {
            event_id: version.clone(),
            original_version: version.clone(),
            state: event_state,
            filename: Some(row.description.clone()),
            batch_id: None,
            applied_at: row.installed_on.clone(),
            checksum: Some(checksum.clone()),
            execution_time_ms: Some(row.execution_time_ms),
        });
        if row.success {
            current_migrations.push(ImportedMigration {
                original_identity: version.clone(),
                original_version: version,
                filename: Some(row.description),
                batch_id: None,
                applied_at: row.installed_on,
                checksum: Some(checksum),
                execution_time_ms: Some(row.execution_time_ms),
                verification_status: VerificationStatus::Partial,
                rollback_blocked: true,
            });
        }
    }

    build_snapshot(
        LegacyTool::Sqlx,
        source_identity,
        events,
        current_migrations,
        blockers,
    )
}

fn build_snapshot(
    tool: LegacyTool,
    source_identity: &str,
    events: Vec<LegacyEvent>,
    current_migrations: Vec<ImportedMigration>,
    blockers: Vec<ImportBlocker>,
) -> Result<LegacyImportSnapshot> {
    let digest_input = DigestInput {
        schema_version: IMPORT_SNAPSHOT_VERSION,
        tool,
        source_identity,
        events: &events,
        current_migrations: &current_migrations,
        blockers: &blockers,
    };
    let canonical = serde_json::to_vec(&digest_input)?;
    let digest = format!("sha256:{}", hex_digest(&Sha256::digest(canonical)));
    Ok(LegacyImportSnapshot {
        schema_version: IMPORT_SNAPSHOT_VERSION,
        tool,
        source_identity: source_identity.to_owned(),
        events,
        current_migrations,
        blockers,
        digest,
    })
}

#[derive(Serialize)]
struct DigestInput<'a> {
    schema_version: u32,
    tool: LegacyTool,
    source_identity: &'a str,
    events: &'a [LegacyEvent],
    current_migrations: &'a [ImportedMigration],
    blockers: &'a [ImportBlocker],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct NativeMigrationFingerprint {
    pub(crate) namespace: String,
    pub(crate) version: String,
    pub(crate) filename: String,
    pub(crate) checksum: String,
}

#[derive(Serialize)]
struct ImportPlanDigestInput<'a> {
    schema_version: u32,
    source_digest: &'a str,
    source_identity: &'a str,
    namespace: &'a str,
    native_migrations: &'a [NativeMigrationFingerprint],
    blockers: &'a [ImportBlocker],
}

fn sort_and_validate_ids<T>(rows: &mut [T], id: impl Fn(&T) -> i64, tool: &str) -> Result<()> {
    rows.sort_by_key(&id);
    let mut previous_id = None;
    for row in rows {
        let row_id = id(row);
        if previous_id == Some(row_id) {
            return Err(invalid_legacy_row(
                tool,
                &row_id.to_string(),
                "source event identifier is duplicated",
            ));
        }
        previous_id = Some(row_id);
    }
    Ok(())
}

fn validate_source_identity(source_identity: &str) -> Result<()> {
    if source_identity.is_empty() || source_identity.chars().any(char::is_control) {
        return Err(DbmError::Configuration(
            "legacy import requires a non-empty, control-free source identity digest".to_owned(),
        ));
    }
    Ok(())
}

fn source_identity_fingerprint(target_identity: &str, namespace: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-legacy-source-identity-v1\0");
    hasher.update(target_identity.as_bytes());
    hasher.update([0]);
    hasher.update(namespace.as_bytes());
    format!("sha256:{}", hex_digest(&hasher.finalize()))
}

fn source_identity_for_session(
    config: &LoadedConfig,
    session: &dyn crate::database::DatabaseSession,
) -> Result<String> {
    let info = session.info();
    if info.dialect != config.config.target.driver
        || info.server_version.is_empty()
        || info.target_identity.as_deref().is_none_or(str::is_empty)
    {
        return Err(DbmError::Driver {
            driver: "database adapter",
            message: "connected session returned invalid identity for legacy import".to_owned(),
        });
    }
    let adapter_identity = info
        .target_identity
        .as_deref()
        .ok_or_else(|| DbmError::Driver {
            driver: "database adapter",
            message: "connected session has no target identity for legacy import".to_owned(),
        })?;
    let target_identity = if info.dialect == "postgres" {
        let schema = info.schema.as_deref().ok_or_else(|| DbmError::Driver {
            driver: "PostgreSQL adapter",
            message: "connected session has no schema identity for legacy import".to_owned(),
        })?;
        format!("{adapter_identity}\0schema={schema}")
    } else {
        adapter_identity.to_owned()
    };
    Ok(source_identity_fingerprint(
        &target_identity,
        &config.config.namespace,
    ))
}

fn require_network_import(config: &LoadedConfig) -> Result<()> {
    if !matches!(config.config.target.driver.as_str(), "postgres" | "mysql") {
        return Err(DbmError::Unsupported(format!(
            "legacy import is not available for database driver '{}'",
            config.config.target.driver
        )));
    }
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "legacy import operations cannot verify registered runner migrations yet".to_owned(),
        ));
    }
    Ok(())
}

async fn close_session_with_result<T>(
    session: &mut dyn crate::database::DatabaseSession,
    result: Result<T>,
) -> Result<T> {
    let close_result = session.close().await;
    match (result, close_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn require_sqlite_table(connection: &rusqlite::Connection, table: &str) -> Result<()> {
    if sqlite_table_exists(connection, table)? {
        return Ok(());
    }
    Err(DbmError::Configuration(format!(
        "legacy {table} table was not found in the configured SQLite database"
    )))
}

fn sqlite_table_exists(connection: &rusqlite::Connection, table: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(DbmError::from)
}

fn invalid_legacy_row(tool: &str, event_id: &str, reason: &str) -> DbmError {
    DbmError::InvalidMigration {
        path: format!("legacy:{tool}:{event_id}"),
        message: reason.to_owned(),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        GooseRow, KnexLock, KnexRow, LegacyEventState, SqlxRow, VerificationStatus, inspect_goose,
        inspect_knex, inspect_sqlx,
    };

    const SOURCE_IDENTITY: &str = "sha256:source-db-fixture";

    #[test]
    fn knex_preserves_full_filename_batch_order_and_active_lock() {
        let snapshot = inspect_knex(
            SOURCE_IDENTITY,
            vec![
                KnexRow {
                    id: 2,
                    name: "20261002_create_orders.ts".to_owned(),
                    batch: 7,
                    migration_time: Some("2026-10-02 03:04:05.000".to_owned()),
                },
                KnexRow {
                    id: 1,
                    name: "20261001_create_users.js".to_owned(),
                    batch: 6,
                    migration_time: None,
                },
            ],
            Some(KnexLock {
                is_locked: true,
                index: Some(1),
            }),
        )
        .expect("inspect Knex history");

        assert_eq!(snapshot.events[0].event_id, "1");
        assert_eq!(
            snapshot.events[1].filename.as_deref(),
            Some("20261002_create_orders.ts")
        );
        assert_eq!(
            snapshot.current_migrations[1].batch_id.as_deref(),
            Some("7")
        );
        assert_eq!(
            snapshot.current_migrations[0].verification_status,
            VerificationStatus::Unverifiable
        );
        assert!(
            snapshot
                .current_migrations
                .iter()
                .all(|item| item.rollback_blocked)
        );
        assert_eq!(snapshot.blockers[0].code, "LEGACY_LOCK_ACTIVE");
    }

    #[test]
    fn knex_requires_a_known_lock_state_before_import_planning() {
        let snapshot = inspect_knex(SOURCE_IDENTITY, Vec::new(), None)
            .expect("inspect Knex without lock metadata");
        assert_eq!(snapshot.blockers[0].code, "LEGACY_LOCK_STATE_UNKNOWN");
    }

    #[test]
    fn goose_derives_latest_applied_state_and_preserves_version_zero_event() {
        let snapshot = inspect_goose(
            SOURCE_IDENTITY,
            vec![
                GooseRow {
                    id: 1,
                    version_id: 0,
                    is_applied: true,
                    timestamp: Some("2026-10-01T00:00:00Z".to_owned()),
                },
                GooseRow {
                    id: 2,
                    version_id: 100,
                    is_applied: true,
                    timestamp: Some("2026-10-01T00:01:00Z".to_owned()),
                },
                GooseRow {
                    id: 3,
                    version_id: 200,
                    is_applied: true,
                    timestamp: Some("2026-10-01T00:02:00Z".to_owned()),
                },
                GooseRow {
                    id: 4,
                    version_id: 100,
                    is_applied: false,
                    timestamp: Some("2026-10-01T00:03:00Z".to_owned()),
                },
            ],
        )
        .expect("inspect Goose history");

        assert_eq!(snapshot.events[0].state, LegacyEventState::Sentinel);
        assert_eq!(snapshot.events[3].state, LegacyEventState::Reverted);
        assert_eq!(snapshot.current_migrations.len(), 1);
        assert_eq!(snapshot.current_migrations[0].original_version, "200");
        assert_eq!(snapshot.current_migrations[0].original_identity, "3");
    }

    #[test]
    fn sqlx_keeps_sha384_and_blocks_failed_history() {
        let snapshot = inspect_sqlx(
            SOURCE_IDENTITY,
            vec![
                SqlxRow {
                    version: 42,
                    description: "create_accounts".to_owned(),
                    installed_on: Some("2026-10-03T00:00:00Z".to_owned()),
                    success: true,
                    checksum: vec![0xab; 48],
                    execution_time_ms: 19,
                },
                SqlxRow {
                    version: 43,
                    description: "create_invoices".to_owned(),
                    installed_on: None,
                    success: false,
                    checksum: vec![0xcd; 48],
                    execution_time_ms: 0,
                },
            ],
        )
        .expect("inspect SQLx history");

        assert_eq!(
            snapshot.events[0].checksum.as_ref().unwrap().algorithm,
            "sha384"
        );
        assert_eq!(
            snapshot.events[0].checksum.as_ref().unwrap().value,
            "ab".repeat(48)
        );
        assert_eq!(snapshot.current_migrations.len(), 1);
        assert_eq!(
            snapshot.current_migrations[0].verification_status,
            VerificationStatus::Partial
        );
        assert_eq!(snapshot.blockers[0].code, "SQLX_MIGRATION_FAILED");
    }

    #[test]
    fn import_digest_is_stable_and_sensitive_to_source_history_changes() {
        let row = || KnexRow {
            id: 1,
            name: "001_create_users.js".to_owned(),
            batch: 1,
            migration_time: Some("2026-10-01T00:00:00Z".to_owned()),
        };
        let first = inspect_knex(SOURCE_IDENTITY, vec![row()], None).expect("first snapshot");
        let repeat = inspect_knex(SOURCE_IDENTITY, vec![row()], None).expect("repeat snapshot");
        let changed = inspect_knex(SOURCE_IDENTITY, vec![KnexRow { batch: 2, ..row() }], None)
            .expect("changed snapshot");

        assert_eq!(first.digest, repeat.digest);
        assert_ne!(first.digest, changed.digest);
    }

    #[test]
    fn malformed_legacy_source_rows_fail_closed() {
        assert!(
            inspect_knex(
                SOURCE_IDENTITY,
                vec![
                    KnexRow {
                        id: 1,
                        name: "first".to_owned(),
                        batch: 1,
                        migration_time: None,
                    },
                    KnexRow {
                        id: 1,
                        name: "duplicate".to_owned(),
                        batch: 1,
                        migration_time: None,
                    },
                ],
                None,
            )
            .is_err()
        );
        assert!(
            inspect_sqlx(
                SOURCE_IDENTITY,
                vec![SqlxRow {
                    version: 1,
                    description: "broken_checksum".to_owned(),
                    installed_on: None,
                    success: true,
                    checksum: vec![0; 32],
                    execution_time_ms: 1,
                }],
            )
            .is_err()
        );
    }
}
