use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::async_runtime::run_blocking;
use crate::config::{Environment, LoadedConfig};
use crate::database::{self, SessionIntent};
use crate::error::{DbmError, Result};
use crate::identity::{hex_digest, utc_timestamp};
use crate::migration::{Migration, TransactionPolicy, split_sql_statements_for_dialect};
use crate::registry::MigrationRegistry;
use crate::source::{self, discover};
use crate::storage::{
    AppliedMigration, FinishedRunData, LockData, MetadataSnapshot, PlannedRunData,
    ReconciliationData, RunRecord, StepData, StepRecord, acknowledge_step, acquire_lock,
    delete_applied, ensure_metadata_path_is_not_directory, ensure_no_unresolved_runs,
    ensure_run_lock_ownership, ensure_schema, finish_run, insert_applied, insert_planned_run,
    insert_step, mark_run_started, metadata_presence, open_mutable, open_readonly, read_snapshot,
    read_snapshot_from_connection, reconcile_run as reconcile_run_record, release_lock,
    release_reconciled_lock, upgrade_metadata_v1_to_current, upgrade_metadata_v2_to_current,
    upgrade_metadata_v3_to_current, upgrade_metadata_v4_to_current,
};

const JSON_SCHEMA_VERSION: &str = "1.0";

#[derive(Clone, Debug, Serialize)]
pub struct ValidateReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub migration_count: usize,
    pub irreversible_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanOperation {
    pub migration_id: String,
    pub kind: crate::migration::MigrationKind,
    pub filename: String,
    pub direction: &'static str,
    pub transaction: &'static str,
    pub risk_level: &'static str,
    pub capability_status: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub target: String,
    pub namespace: String,
    pub status: &'static str,
    pub plan_hash: String,
    pub read_only: bool,
    pub operations: Vec<PlanOperation>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusEntry {
    pub migration_id: String,
    pub kind: String,
    pub filename: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub target: String,
    pub namespace: String,
    pub status: String,
    pub migrations: Vec<StatusEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub target: String,
    pub namespace: String,
    pub runs: Vec<RunRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorFinding {
    pub code: String,
    pub severity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorStep {
    pub run_id: String,
    pub step_index: usize,
    pub statement_hash: String,
    pub statement_kind: String,
    pub transaction_class: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_state: Option<String>,
    pub inspection: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub target: String,
    pub namespace: String,
    pub status: String,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_format_version: Option<i64>,
    pub findings: Vec<DoctorFinding>,
    pub steps: Vec<DoctorStep>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetadataUpgradeReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_version: Option<i64>,
    pub to_version: i64,
    pub runs_preserved: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReconcileReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub target: String,
    pub namespace: String,
    pub run_id: String,
    pub reconciliation_run_id: String,
    pub operation_result: String,
    pub evidence_ref: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ReconcileRequest<'a> {
    pub run_id: &'a str,
    pub operation_result: &'a str,
    pub evidence_ref: &'a str,
    pub repair_plan_json: &'a str,
    pub reason: &'a str,
    pub actor: &'a str,
    pub approved: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairPlan {
    schema_version: String,
    summary: String,
    steps: Vec<RepairPlanStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairPlanStep {
    step_index: usize,
    statement_hash: String,
    state: RepairPlanStepState,
    inspection: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RepairPlanStepState {
    ConfirmedApplied,
    ConfirmedNotApplied,
    Indeterminate,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub target: String,
    pub namespace: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub migrations: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunnerAction {
    Validate,
    Plan,
    Status,
    History,
    Doctor,
    Up,
    Down { steps: usize },
}

pub(crate) struct RunnerPreflight {
    pub(crate) manifest_fingerprint: String,
    pub(crate) plan_hash: String,
    pub(crate) target_fingerprint: String,
    pub(crate) report: PlanReport,
}

pub fn validate(config: &LoadedConfig) -> Result<ValidateReport> {
    let migrations = discover(config)?;
    validate_migrations(&migrations)
}

fn validate_migrations(migrations: &[Migration]) -> Result<ValidateReport> {
    let irreversible_count = migrations
        .iter()
        .filter(|migration| migration.down_sql.is_none())
        .count();
    let warnings = if irreversible_count > 0 {
        vec![format!(
            "{irreversible_count} migration(s) have no down section and cannot be rolled back"
        )]
    } else {
        Vec::new()
    };

    Ok(ValidateReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "validate",
        status: "valid",
        migration_count: migrations.len(),
        irreversible_count,
        warnings,
    })
}

pub fn plan(config: &LoadedConfig) -> Result<PlanReport> {
    config.sqlite_database_path()?;
    let migrations = discover(config)?;
    let snapshot = read_snapshot(config)?;
    plan_from_snapshot(config, &migrations, &snapshot)
}

pub fn status(config: &LoadedConfig) -> Result<StatusReport> {
    config.sqlite_database_path()?;
    let migrations = discover(config)?;
    let snapshot = read_snapshot(config)?;
    let entries = build_status_entries(&migrations, &snapshot)?;
    let overall = overall_status(&entries);

    Ok(StatusReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: overall,
        migrations: entries,
    })
}

pub fn history(config: &LoadedConfig) -> Result<HistoryReport> {
    config.sqlite_database_path()?;
    let snapshot = read_snapshot(config)?;
    Ok(HistoryReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "history",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        runs: snapshot.runs,
    })
}

pub fn doctor(config: &LoadedConfig) -> Result<DoctorReport> {
    doctor_with_discovery(config, || discover(config))
}

fn doctor_with_registry(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<DoctorReport> {
    doctor_with_discovery(config, || source::discover_with_registry(config, registry))
}

fn doctor_with_discovery(
    config: &LoadedConfig,
    discover_migrations: impl FnOnce() -> Result<Vec<Migration>>,
) -> Result<DoctorReport> {
    config.sqlite_database_path()?;
    let target = config.target_description();
    let namespace = config.config.namespace.clone();
    let Some(connection) = open_readonly(config)? else {
        return Ok(DoctorReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            target,
            namespace,
            status: "not_initialized".to_owned(),
            read_only: true,
            metadata_format_version: None,
            findings: vec![DoctorFinding {
                code: "METADATA_NOT_INITIALIZED".to_owned(),
                severity: "info",
                run_id: None,
                migration_id: None,
                message: "target does not exist; doctor did not create it".to_owned(),
            }],
            steps: Vec::new(),
        });
    };

    let presence = metadata_presence(&connection)?;
    if presence == 0 {
        return Ok(DoctorReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            target,
            namespace,
            status: "not_initialized".to_owned(),
            read_only: true,
            metadata_format_version: None,
            findings: vec![DoctorFinding {
                code: "METADATA_NOT_INITIALIZED".to_owned(),
                severity: "info",
                run_id: None,
                migration_id: None,
                message: "target has no migrator metadata; doctor did not change it".to_owned(),
            }],
            steps: Vec::new(),
        });
    }

    if presence == 3 {
        let version = read_metadata_version(&connection)?;
        if version == Some(1) {
            return Ok(DoctorReport {
                schema_version: JSON_SCHEMA_VERSION,
                command: "doctor",
                target,
                namespace,
                status: "metadata_upgrade_required".to_owned(),
                read_only: true,
                metadata_format_version: version,
                findings: vec![DoctorFinding {
                    code: "METADATA_UPGRADE_REQUIRED".to_owned(),
                    severity: "warning",
                    run_id: None,
                    migration_id: None,
                    message: "metadata format 1 is intact; use `dbm metadata upgrade --yes` to upgrade explicitly".to_owned(),
                }],
                steps: Vec::new(),
            });
        }
    }

    if presence != 5 {
        return Ok(DoctorReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            target,
            namespace,
            status: "metadata_inconsistent".to_owned(),
            read_only: true,
            metadata_format_version: read_metadata_version(&connection)?,
            findings: vec![DoctorFinding {
                code: "METADATA_INCONSISTENT".to_owned(),
                severity: "error",
                run_id: None,
                migration_id: None,
                message: format!(
                    "expected either 0, 3 legacy, or 5 current metadata tables; found {presence}"
                ),
            }],
            steps: Vec::new(),
        });
    }

    let format_version = read_metadata_version(&connection)?;
    if format_version != Some(crate::storage::METADATA_FORMAT_VERSION) {
        return Ok(DoctorReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            target,
            namespace,
            status: "metadata_inconsistent".to_owned(),
            read_only: true,
            metadata_format_version: format_version,
            findings: vec![DoctorFinding {
                code: "METADATA_FORMAT_UNSUPPORTED".to_owned(),
                severity: "error",
                run_id: None,
                migration_id: None,
                message: format!(
                    "metadata format {format_version:?} is not supported by this binary"
                ),
            }],
            steps: Vec::new(),
        });
    }

    let snapshot = read_snapshot_from_connection(&connection, &config.config.namespace)?;
    let migrations = discover_migrations()?;
    doctor_from_snapshot(config, &migrations, &snapshot, format_version)
}

fn doctor_from_snapshot(
    config: &LoadedConfig,
    migrations: &[Migration],
    snapshot: &MetadataSnapshot,
    format_version: Option<i64>,
) -> Result<DoctorReport> {
    let target = config.target_description();
    let namespace = config.config.namespace.clone();
    let mut findings = Vec::new();
    let entries = build_status_entries(migrations, snapshot)?;
    for migration in snapshot
        .legacy_applied
        .iter()
        .filter(|migration| migration.verification_status != "adopted")
    {
        findings.push(DoctorFinding {
            code: "LEGACY_ROLLBACK_BLOCKED".to_owned(),
            severity: "warning",
            run_id: None,
            migration_id: Some(format!(
                "legacy:{}:{}",
                migration.origin_tool, migration.original_version
            )),
            message: "imported legacy state remains current; verify and adopt a matching, applied native migration before any rollback"
                .to_owned(),
        });
    }
    for entry in entries.iter().filter(|entry| entry.state == "diverged") {
        findings.push(DoctorFinding {
            code: "SOURCE_DIVERGED".to_owned(),
            severity: "error",
            run_id: None,
            migration_id: Some(entry.migration_id.clone()),
            message: entry
                .reason
                .clone()
                .unwrap_or_else(|| "migration source and applied state disagree".to_owned()),
        });
    }
    for entry in entries.iter().filter(|entry| entry.state == "blocked") {
        findings.push(DoctorFinding {
            code: "EXECUTION_ORDER_BLOCKED".to_owned(),
            severity: "error",
            run_id: None,
            migration_id: Some(entry.migration_id.clone()),
            message: entry
                .reason
                .clone()
                .unwrap_or_else(|| "migration order is inconsistent".to_owned()),
        });
    }

    let lock_active = snapshot
        .lock
        .as_ref()
        .is_some_and(|lock| lock.state == "observed_active");
    let lock_run_id = snapshot
        .lock
        .as_ref()
        .filter(|lock| lock.state == "observed_active")
        .and_then(|lock| lock.run_id.as_deref());
    if let Some(lock) = snapshot
        .lock
        .as_ref()
        .filter(|lock| lock.state == "observed_active")
    {
        findings.push(DoctorFinding {
            code: "LOCK_HELD".to_owned(),
            severity: "error",
            run_id: lock.run_id.clone(),
            migration_id: None,
            message: "a database lock is recorded active; it may be live or abandoned and must not be stolen automatically".to_owned(),
        });
    }

    let mut doctor_steps = Vec::new();
    for run in snapshot.runs.iter().filter(|run| {
        matches!(run.state.as_str(), "planned" | "started" | "indeterminate")
            || run.state == "failed"
            || (lock_active && lock_run_id == Some(run.run_id.as_str()))
    }) {
        let current = snapshot
            .applied
            .iter()
            .find(|applied| applied.version == run.migration_id);
        let migration = migrations
            .iter()
            .find(|migration| migration.identity.version == run.migration_id);
        let unresolved_result = !matches!(run.state.as_str(), "succeeded" | "reconciled");
        let nontransactional_reconciliation_required = unresolved_result
            && migration.is_some_and(|migration| migration.transaction == TransactionPolicy::Never);
        let source_missing = unresolved_result && migration.is_none();
        let observed_result = if nontransactional_reconciliation_required || source_missing {
            "indeterminate"
        } else {
            diagnose_run_result(run, current, lock_active)
        };
        let unresolved = matches!(run.state.as_str(), "planned" | "started" | "indeterminate");
        if unresolved {
            findings.push(DoctorFinding {
                code: observed_result.to_ascii_uppercase(),
                severity: if observed_result == "metadata_inconsistent" { "error" } else { "warning" },
                run_id: Some(run.run_id.clone()),
                migration_id: Some(run.migration_id.clone()),
                message: match (source_missing, nontransactional_reconciliation_required) {
                    (true, _) => format!(
                        "run is {}; its migration source is missing, so the execution outcome cannot be proven; restore the exact source and reconcile before another write",
                        run.state
                    ),
                    (false, true) => format!(
                        "run is {}; its non-transactional SQL outcome cannot be inferred from the applied-migration row; inspect database effects and reconcile explicitly before another write",
                        run.state
                    ),
                    (false, false) => format!(
                        "run is {}; current transactional state indicates {observed_result}; explicit reconciliation is required before another write",
                        run.state
                    ),
                },
            });
        }
        if let Some(migration) = migration {
            if migration.checksum == run.checksum_at_execution {
                doctor_steps.extend(build_doctor_steps(
                    run,
                    migration,
                    &snapshot.steps,
                    observed_result,
                ));
            } else {
                findings.push(DoctorFinding {
                    code: "RUN_SOURCE_DIVERGED".to_owned(),
                    severity: "error",
                    run_id: Some(run.run_id.clone()),
                    migration_id: Some(run.migration_id.clone()),
                    message: "current migration checksum differs from the source captured by this run; reconciliation is blocked".to_owned(),
                });
            }
        } else if unresolved {
            findings.push(DoctorFinding {
                code: "RUN_SOURCE_MISSING".to_owned(),
                severity: "error",
                run_id: Some(run.run_id.clone()),
                migration_id: Some(run.migration_id.clone()),
                message: "migration source is missing; restore the checksum-matching source before reconciliation".to_owned(),
            });
        }
    }

    for run in snapshot.runs.iter().filter(|run| run.state == "failed") {
        findings.push(DoctorFinding {
            code: "RUN_FAILED".to_owned(),
            severity: "warning",
            run_id: Some(run.run_id.clone()),
            migration_id: Some(run.migration_id.clone()),
            message: run
                .error_summary_redacted
                .clone()
                .unwrap_or_else(|| "run failed; inspect the recorded step journal".to_owned()),
        });
    }

    for step in snapshot
        .steps
        .iter()
        .filter(|step| matches!(step.state.as_str(), "intent_recorded" | "indeterminate"))
    {
        findings.push(DoctorFinding {
            code: "STEP_UNACKNOWLEDGED".to_owned(),
            severity: "warning",
            run_id: Some(step.run_id.clone()),
            migration_id: None,
            message: format!(
                "step {} has state {}; SQL text is not stored",
                step.step_index, step.state
            ),
        });
    }

    let status = if findings
        .iter()
        .any(|finding| finding.code == "METADATA_INCONSISTENT")
    {
        "metadata_inconsistent"
    } else if lock_active {
        "lock_held"
    } else if findings.iter().any(|finding| {
        matches!(
            finding.code.as_str(),
            "KNOWN_APPLIED" | "KNOWN_NOT_APPLIED" | "INDETERMINATE"
        )
    }) || findings
        .iter()
        .any(|finding| finding.code == "STEP_UNACKNOWLEDGED")
    {
        "requires_reconciliation"
    } else if findings.iter().any(|finding| {
        matches!(
            finding.code.as_str(),
            "SOURCE_DIVERGED" | "RUN_SOURCE_DIVERGED"
        )
    }) {
        "source_diverged"
    } else if findings
        .iter()
        .any(|finding| finding.code == "EXECUTION_ORDER_BLOCKED")
    {
        "execution_order_blocked"
    } else if findings.iter().any(|finding| finding.code == "RUN_FAILED") {
        "known_failures"
    } else if findings
        .iter()
        .any(|finding| finding.code == "LEGACY_ROLLBACK_BLOCKED")
    {
        "legacy_adoption_required"
    } else {
        "healthy"
    };

    Ok(DoctorReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "doctor",
        target,
        namespace,
        status: status.to_owned(),
        read_only: true,
        metadata_format_version: format_version,
        findings,
        steps: doctor_steps,
    })
}

fn diagnose_run_result(
    run: &RunRecord,
    applied: Option<&AppliedMigration>,
    lock_active: bool,
) -> &'static str {
    match run.state.as_str() {
        "succeeded" => match (run.direction.as_str(), applied) {
            ("up", Some(applied)) if applied.checksum == run.checksum_at_execution => {
                "known_applied"
            }
            ("down", None) => "known_applied",
            _ => "metadata_inconsistent",
        },
        "failed" => match (run.direction.as_str(), applied) {
            ("up", None) => "known_not_applied",
            ("down", Some(applied)) if applied.checksum == run.checksum_at_execution => {
                "known_not_applied"
            }
            _ => "metadata_inconsistent",
        },
        _ if lock_active => "indeterminate",
        _ => match (run.direction.as_str(), applied) {
            ("up", Some(applied)) if applied.checksum == run.checksum_at_execution => {
                "known_applied"
            }
            ("up", None) => "known_not_applied",
            ("down", Some(applied)) if applied.checksum == run.checksum_at_execution => {
                "known_not_applied"
            }
            ("down", None) => "known_applied",
            _ => "metadata_inconsistent",
        },
    }
}

fn build_doctor_steps(
    run: &RunRecord,
    migration: &Migration,
    recorded_steps: &[StepRecord],
    observed_result: &str,
) -> Vec<DoctorStep> {
    let sql = match run.direction.as_str() {
        "up" => migration.up_sql.as_str(),
        "down" => match migration.down_sql.as_deref() {
            Some(sql) => sql,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let transactional_step_state = match observed_result {
        "known_applied" => "confirmed_applied",
        "known_not_applied" => "confirmed_not_applied",
        "indeterminate" => "indeterminate",
        _ => "metadata_inconsistent",
    };
    let transactional_inspection = match observed_result {
        "known_applied" => {
            "the applied migration row and captured checksum prove this transaction committed; inspect the target catalog if external changes are possible"
        }
        "known_not_applied" => {
            "the transactional migration row is absent for an up run, or remains present for a down run; inspect the target catalog for external changes"
        }
        "indeterminate" => {
            "the database lock may still belong to a live process; do not infer statement outcome until the lock owner is resolved"
        }
        _ => {
            "metadata and applied state disagree; inspect the database catalog and migration history before taking action"
        }
    };

    split_sql_statements_for_dialect(sql, migration.dialect)
        .iter()
        .enumerate()
        .map(|(index, statement)| {
            let step_index = i64::try_from(index).unwrap_or(i64::MAX);
            let recorded = recorded_steps
                .iter()
                .find(|step| step.run_id == run.run_id && step.step_index == step_index);
            let default_transaction_class = match migration.transaction {
                TransactionPolicy::Never => "non_transactional",
                TransactionPolicy::Auto | TransactionPolicy::Always => "transactional",
            };
            let transaction_class = match recorded {
                Some(step) => step.transaction_class.as_str(),
                None => default_transaction_class,
            };
            let (state, inspection) = if transaction_class == "non_transactional" {
                (
                    "indeterminate",
                    "an applied-migration row cannot prove a non-transactional statement outcome; inspect the PostgreSQL catalog and index validity before reconciliation",
                )
            } else {
                (transactional_step_state, transactional_inspection)
            };
            DoctorStep {
                run_id: run.run_id.clone(),
                step_index: index,
                statement_hash: sql_hash(statement),
                statement_kind: statement_kind(statement),
                transaction_class: transaction_class.to_owned(),
                state: state.to_owned(),
                recorded_state: recorded.map(|step| step.state.clone()),
                inspection: inspection.to_owned(),
            }
        })
        .collect()
}

pub async fn validate_async(config: &LoadedConfig) -> Result<ValidateReport> {
    let config = config.clone();
    run_blocking(move || validate(&config)).await
}

pub async fn validate_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<ValidateReport> {
    let config = config.clone();
    let registry = registry.clone();
    run_blocking(move || {
        let migrations = source::discover_with_registry(&config, &registry)?;
        validate_migrations(&migrations)
    })
    .await
}

pub async fn plan_async(config: &LoadedConfig) -> Result<PlanReport> {
    let migrations = discover_on_worker(config).await?;
    let snapshot = read_adapter_snapshot(config).await?;
    let mut report = plan_from_snapshot(config, &migrations, &snapshot)?;
    if config.config.target.driver == "postgres" {
        report.warnings.push(
            "PostgreSQL planning is read-only and does not initialize metadata; up/down writes require metadata format 4 and a non-production target"
                .to_owned(),
        );
    }
    if config.config.target.driver == "mysql" {
        report.warnings.push(
            "MySQL/MariaDB planning is read-only; initial migration execution requires explicit transaction=never and metadata format 4"
                .to_owned(),
        );
    }
    if matches!(config.config.target.driver.as_str(), "postgres" | "mysql") && !snapshot.initialized
    {
        report.warnings.push(
            "database migration metadata is not initialized; no target schema or metadata was created"
                .to_owned(),
        );
    }
    Ok(report)
}

pub async fn status_async(config: &LoadedConfig) -> Result<StatusReport> {
    let migrations = discover_on_worker(config).await?;
    let snapshot = read_adapter_snapshot(config).await?;
    let entries = build_status_entries(&migrations, &snapshot)?;
    Ok(StatusReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: overall_status(&entries),
        migrations: entries,
    })
}

pub async fn status_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<StatusReport> {
    let migrations = discover_on_worker_with_registry(config, Some(registry)).await?;
    let (snapshot, info) = read_adapter_snapshot_with_info(config).await?;
    registry.validate_database_info(&info)?;
    let entries = build_status_entries(&migrations, &snapshot)?;
    Ok(StatusReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: overall_status(&entries),
        migrations: entries,
    })
}

pub async fn history_async(config: &LoadedConfig) -> Result<HistoryReport> {
    let snapshot = read_adapter_snapshot(config).await?;
    Ok(HistoryReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "history",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        runs: snapshot.runs,
    })
}

pub async fn doctor_async(config: &LoadedConfig) -> Result<DoctorReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        return run_blocking(move || doctor(&config)).await;
    }

    let migrations = discover_on_worker(config).await?;
    let snapshot = read_adapter_snapshot(config).await?;
    if snapshot.initialized {
        return doctor_from_snapshot(
            config,
            &migrations,
            &snapshot,
            snapshot.metadata_format_version,
        );
    }
    Ok(DoctorReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "doctor",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: "not_initialized".to_owned(),
        read_only: true,
        metadata_format_version: None,
        findings: vec![DoctorFinding {
            code: "METADATA_NOT_INITIALIZED".to_owned(),
            severity: "info",
            run_id: None,
            migration_id: None,
            message:
                "PostgreSQL target has no Database Migrator metadata; doctor did not modify it"
                    .to_owned(),
        }],
        steps: Vec::new(),
    })
}

pub async fn doctor_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<DoctorReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        let registry = registry.clone();
        return run_blocking(move || doctor_with_registry(&config, &registry)).await;
    }

    let migrations = discover_on_worker_with_registry(config, Some(registry)).await?;
    let (snapshot, info) = read_adapter_snapshot_with_info(config).await?;
    registry.validate_database_info(&info)?;
    if snapshot.initialized {
        return doctor_from_snapshot(
            config,
            &migrations,
            &snapshot,
            snapshot.metadata_format_version,
        );
    }
    Ok(DoctorReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "doctor",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: "not_initialized".to_owned(),
        read_only: true,
        metadata_format_version: None,
        findings: vec![DoctorFinding {
            code: "METADATA_NOT_INITIALIZED".to_owned(),
            severity: "info",
            run_id: None,
            migration_id: None,
            message: "database target has no Database Migrator metadata; doctor did not modify it"
                .to_owned(),
        }],
        steps: Vec::new(),
    })
}

async fn discover_on_worker(config: &LoadedConfig) -> Result<Vec<Migration>> {
    let config = config.clone();
    run_blocking(move || discover(&config)).await
}

async fn discover_on_worker_with_registry(
    config: &LoadedConfig,
    registry: Option<&MigrationRegistry>,
) -> Result<Vec<Migration>> {
    let Some(registry) = registry else {
        return discover_on_worker(config).await;
    };
    let config = config.clone();
    let registry = registry.clone();
    run_blocking(move || source::discover_with_registry(&config, &registry)).await
}

async fn read_adapter_snapshot(config: &LoadedConfig) -> Result<MetadataSnapshot> {
    let (snapshot, _) = read_adapter_snapshot_with_info(config).await?;
    Ok(snapshot)
}

async fn read_adapter_snapshot_with_info(
    config: &LoadedConfig,
) -> Result<(MetadataSnapshot, database::DatabaseInfo)> {
    let adapter = database::adapter_for(config)?;
    let mut session = adapter.connect(config, SessionIntent::ReadOnly).await?;
    let info = session.info().clone();
    if info.dialect != config.config.target.driver
        || info.server_version.is_empty()
        || info.target_identity.as_deref().is_none_or(str::is_empty)
    {
        let _ = session.close().await;
        return Err(DbmError::Driver {
            driver: "database adapter",
            message: "connected session returned invalid server identity".to_owned(),
        });
    }

    if let Err(error) = session.verify_session().await {
        let _ = session.close().await;
        return Err(error);
    }
    let snapshot = session.read_snapshot(&config.config.namespace).await;
    let close = session.close().await;
    match (snapshot, close) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(snapshot), Ok(())) => Ok((snapshot, info)),
    }
}

pub fn upgrade_metadata(config: &LoadedConfig, approved: bool) -> Result<MetadataUpgradeReport> {
    config.sqlite_database_path()?;
    let Some(connection) = open_readonly(config)? else {
        return Ok(metadata_upgrade_report("no_metadata", true, None, 0));
    };
    let presence = metadata_presence(&connection)?;
    if presence == 0 {
        return Ok(metadata_upgrade_report("no_metadata", true, None, 0));
    }
    let from_version = read_metadata_version(&connection)?;
    let full_presence = crate::storage::metadata_v4_presence(&connection)?;
    if full_presence == 8 && from_version == Some(crate::storage::METADATA_FORMAT_VERSION) {
        return Ok(metadata_upgrade_report("current", true, from_version, 0));
    }
    let upgrade_from = match (presence, from_version) {
        (3, Some(crate::storage::LEGACY_METADATA_FORMAT_VERSION)) => 1,
        (5, Some(crate::storage::SECOND_PREVIOUS_METADATA_FORMAT_VERSION)) => 2,
        (5, Some(crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION)) => 3,
        (8, Some(crate::storage::PREVIOUS_METADATA_FORMAT_VERSION)) => 4,
        _ => 0,
    };
    if upgrade_from == 0 {
        return Err(DbmError::Diverged(format!(
            "explicit metadata upgrade supports intact formats 1 through 4 only; found {presence} core tables and version {from_version:?}"
        )));
    }
    let runs_preserved: i64 = connection.query_row(
        "SELECT COUNT(*) FROM main.schema_migration_runs",
        [],
        |row| row.get(0),
    )?;
    drop(connection);
    if !approved {
        return Ok(metadata_upgrade_report(
            "requires_approval",
            true,
            from_version,
            usize::try_from(runs_preserved).unwrap_or(0),
        ));
    }
    require_mutation_environment(config)?;

    let mut connection = open_mutable(config)?;
    let transaction = begin_immediate(&mut connection)?;
    let upgraded_at = utc_timestamp()?;
    match upgrade_from {
        1 => upgrade_metadata_v1_to_current(&transaction, &upgraded_at)?,
        2 => upgrade_metadata_v2_to_current(&transaction, &upgraded_at)?,
        3 => upgrade_metadata_v3_to_current(&transaction, &upgraded_at)?,
        4 => upgrade_metadata_v4_to_current(&transaction, &upgraded_at)?,
        _ => unreachable!("upgrade source version was validated above"),
    }
    if upgrade_from < 4 {
        upgrade_metadata_v4_to_current(&transaction, &upgraded_at)?;
    }
    transaction.commit()?;
    Ok(metadata_upgrade_report(
        "upgraded",
        false,
        from_version,
        usize::try_from(runs_preserved).unwrap_or(0),
    ))
}

pub async fn upgrade_metadata_async(
    config: &LoadedConfig,
    approved: bool,
) -> Result<MetadataUpgradeReport> {
    if config.config.target.driver == "sqlite" {
        let config = config.clone();
        return run_blocking(move || upgrade_metadata(&config, approved)).await;
    }

    if !matches!(config.config.target.driver.as_str(), "postgres" | "mysql") {
        return Err(DbmError::Unsupported(
            "explicit metadata upgrade is not implemented for this database adapter".to_owned(),
        ));
    }
    let postgres = config.config.target.driver == "postgres";

    let snapshot = read_adapter_snapshot(config).await?;
    if !snapshot.initialized {
        return Ok(metadata_upgrade_report("no_metadata", true, None, 0));
    }
    let from_version = snapshot.metadata_format_version;
    if from_version == Some(crate::storage::METADATA_FORMAT_VERSION) {
        return Ok(metadata_upgrade_report(
            "current",
            true,
            from_version,
            snapshot.runs.len(),
        ));
    }
    if !matches!(
        from_version,
        Some(crate::storage::THIRD_PREVIOUS_METADATA_FORMAT_VERSION)
            | Some(crate::storage::PREVIOUS_METADATA_FORMAT_VERSION)
    ) {
        return Err(DbmError::Diverged(format!(
            "explicit metadata upgrade supports intact formats 3 and 4 only; found {from_version:?}"
        )));
    }
    let runs_preserved = snapshot.runs.len();
    if !approved {
        return Ok(metadata_upgrade_report(
            "requires_approval",
            true,
            from_version,
            runs_preserved,
        ));
    }
    require_mutation_environment(config)?;

    let adapter = database::adapter_for(config)?;
    let mut session = adapter.connect(config, SessionIntent::ReadWrite).await?;
    let result = async {
        session
            .acquire_lock(Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
        if postgres {
            session.begin().await?;
        }

        let locked_snapshot = session.read_snapshot(&config.config.namespace).await?;
        if locked_snapshot.metadata_format_version != from_version {
            return Err(DbmError::Safety(
                "database metadata changed after inspection; no upgrade was applied".to_owned(),
            ));
        }
        if locked_snapshot.runs.len() != runs_preserved {
            return Err(DbmError::Safety(
                "database run history changed after inspection; review the target and retry"
                    .to_owned(),
            ));
        }

        session.upgrade_metadata_to_current().await?;
        if postgres {
            session.commit().await?;
        }
        session.release_lock().await?;
        Ok(())
    }
    .await;
    let close_result = session.close().await;
    match (result, close_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(metadata_upgrade_report(
            "upgraded",
            false,
            from_version,
            runs_preserved,
        )),
    }
}

fn metadata_upgrade_report(
    status: &'static str,
    read_only: bool,
    from_version: Option<i64>,
    runs_preserved: usize,
) -> MetadataUpgradeReport {
    MetadataUpgradeReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "metadata upgrade",
        status,
        read_only,
        from_version,
        to_version: crate::storage::METADATA_FORMAT_VERSION,
        runs_preserved,
    }
}

pub fn reconcile(config: &LoadedConfig, request: ReconcileRequest<'_>) -> Result<ReconcileReport> {
    let ReconcileRequest {
        run_id,
        operation_result,
        evidence_ref,
        repair_plan_json,
        reason,
        actor,
        approved,
    } = request;
    let database_path = config.sqlite_database_path()?;
    require_mutation_environment(config)?;
    validate_reconcile_request_fields(
        operation_result,
        evidence_ref,
        repair_plan_json,
        reason,
        actor,
        approved,
    )?;

    ensure_metadata_path_is_not_directory(database_path)?;
    if !database_path.exists() {
        return Err(DbmError::Diverged(
            "cannot reconcile a run because the SQLite target does not exist".to_owned(),
        ));
    }
    let mut connection = open_mutable(config)?;
    let transaction = begin_immediate(&mut connection)?;
    let snapshot = read_snapshot_from_connection(&transaction, &config.config.namespace)?;
    let run = snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .ok_or_else(|| {
            DbmError::Configuration(format!("run {run_id} was not found in this namespace"))
        })?;
    if run.direction == "reconcile" || run.state == "reconciled" {
        return Err(DbmError::Safety(format!(
            "run {run_id} has already been reconciled"
        )));
    }
    let active_lock = snapshot
        .lock
        .as_ref()
        .filter(|lock| lock.state == "observed_active");
    let lock_matches_run = active_lock.is_some_and(|lock| lock.run_id.as_deref() == Some(run_id));
    let unresolved_run = matches!(run.state.as_str(), "planned" | "started" | "indeterminate");
    let terminal_run_with_held_lock =
        matches!(run.state.as_str(), "succeeded" | "failed") && lock_matches_run;
    if !unresolved_run && !terminal_run_with_held_lock {
        return Err(DbmError::Safety(format!(
            "run {run_id} is {}, not an unresolved run or a run with a held diagnostic lock",
            run.state
        )));
    }
    if active_lock.is_some() && !lock_matches_run {
        return Err(DbmError::Lock(
            "active database lock belongs to a different run; inspect doctor before reconciling"
                .to_owned(),
        ));
    }

    let migrations = discover(config)?;
    let migration = migrations
        .iter()
        .find(|migration| migration.identity.version == run.migration_id)
        .ok_or_else(|| {
            DbmError::Diverged(format!(
                "source for run {} is missing; reconciliation cannot verify its identity",
                run.run_id
            ))
        })?;
    if migration.checksum != run.checksum_at_execution {
        return Err(DbmError::Diverged(format!(
            "source checksum for run {} changed; reconciliation is blocked",
            run.run_id
        )));
    }
    validate_repair_plan(
        repair_plan_json,
        migration,
        &run.direction,
        operation_result,
    )?;
    let applied = snapshot
        .applied
        .iter()
        .find(|applied| applied.version == run.migration_id);
    if let Some(applied) = applied
        && let Some(reason) = applied_mismatch(migration, applied)
    {
        return Err(DbmError::Diverged(format!(
            "current applied state for run {} diverges: {reason}",
            run.run_id
        )));
    }
    let observed_result = match (run.direction.as_str(), applied.is_some()) {
        ("up", true) | ("down", false) => "operation_applied",
        ("up", false) | ("down", true) => "operation_not_applied",
        _ => {
            return Err(DbmError::Diverged(format!(
                "run {} has unsupported direction {}",
                run.run_id, run.direction
            )));
        }
    };
    if observed_result != operation_result {
        return Err(DbmError::Safety(format!(
            "requested result {operation_result} conflicts with the SQLite state, which proves {observed_result}"
        )));
    }

    let now = utc_timestamp()?;
    let reconciliation_run_id = Uuid::new_v4().to_string();
    reconcile_run_record(
        &transaction,
        ReconciliationData {
            run_id,
            reconcile_run_id: &reconciliation_run_id,
            result: operation_result,
            evidence_ref,
            repair_plan: repair_plan_json,
            reason,
            actor,
            now: &now,
        },
    )?;
    release_reconciled_lock(&transaction, run_id, &now)?;
    transaction.commit()?;

    Ok(ReconcileReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "doctor reconcile",
        status: "reconciled",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        run_id: run_id.to_owned(),
        reconciliation_run_id,
        operation_result: operation_result.to_owned(),
        evidence_ref: evidence_ref.to_owned(),
    })
}

fn validate_reconcile_request_fields(
    operation_result: &str,
    evidence_ref: &str,
    repair_plan_json: &str,
    reason: &str,
    actor: &str,
    approved: bool,
) -> Result<()> {
    if !approved {
        return Err(DbmError::Safety(
            "reconciliation requires the explicit --approve flag".to_owned(),
        ));
    }
    if !matches!(
        operation_result,
        "operation_applied" | "operation_not_applied"
    ) {
        return Err(DbmError::Configuration(
            "reconciliation result must be operation_applied or operation_not_applied".to_owned(),
        ));
    }
    if !is_raw_evidence_checksum(evidence_ref) {
        return Err(DbmError::Configuration(
            "evidence reference must be a SHA-256 raw-v1 checksum".to_owned(),
        ));
    }
    if reason.trim().is_empty() || reason.len() > 1_000 {
        return Err(DbmError::Configuration(
            "reconciliation reason must contain 1 to 1000 bytes".to_owned(),
        ));
    }
    if actor.trim().is_empty() || actor.len() > 128 {
        return Err(DbmError::Configuration(
            "reconciliation actor must contain 1 to 128 bytes".to_owned(),
        ));
    }
    if repair_plan_json.trim().is_empty() || repair_plan_json.len() > 65_536 {
        return Err(DbmError::Configuration(
            "repair plan must be non-empty and no larger than 64 KiB".to_owned(),
        ));
    }
    Ok(())
}

pub async fn reconcile_async(
    config: &LoadedConfig,
    request: ReconcileRequest<'_>,
) -> Result<ReconcileReport> {
    match config.config.target.driver.as_str() {
        "sqlite" => {
            let config = config.clone();
            let run_id = request.run_id.to_owned();
            let operation_result = request.operation_result.to_owned();
            let evidence_ref = request.evidence_ref.to_owned();
            let repair_plan_json = request.repair_plan_json.to_owned();
            let reason = request.reason.to_owned();
            let actor = request.actor.to_owned();
            let approved = request.approved;
            run_blocking(move || {
                reconcile(
                    &config,
                    ReconcileRequest {
                        run_id: &run_id,
                        operation_result: &operation_result,
                        evidence_ref: &evidence_ref,
                        repair_plan_json: &repair_plan_json,
                        reason: &reason,
                        actor: &actor,
                        approved,
                    },
                )
            })
            .await
        }
        "postgres" => reconcile_postgres_async(config, request).await,
        "mysql" => reconcile_mysql_async(config, request).await,
        driver => Err(DbmError::Unsupported(format!(
            "reconciliation is not supported for database driver '{driver}'"
        ))),
    }
}

async fn reconcile_postgres_async(
    config: &LoadedConfig,
    request: ReconcileRequest<'_>,
) -> Result<ReconcileReport> {
    validate_reconcile_request_fields(
        request.operation_result,
        request.evidence_ref,
        request.repair_plan_json,
        request.reason,
        request.actor,
        request.approved,
    )?;
    require_mutation_environment(config)?;
    let migrations = discover_on_worker(config).await?;
    let adapter = database::adapter_for(config)?;
    let mut session = adapter.connect(config, SessionIntent::ReadWrite).await?;
    let info = session.info();
    if info.dialect != "postgres" || info.server_version.is_empty() {
        let _ = session.close().await;
        return Err(DbmError::Driver {
            driver: "PostgreSQL adapter",
            message: "connected session returned invalid server identity".to_owned(),
        });
    }

    let result = async {
        session
            .acquire_lock(Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
        session.begin().await?;
        let snapshot = session.read_snapshot(&config.config.namespace).await?;
        if snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION) {
            return Err(DbmError::Unsupported(format!(
                "PostgreSQL reconciliation requires metadata format {}; found {:?}",
                crate::storage::METADATA_FORMAT_VERSION,
                snapshot.metadata_format_version
            )));
        }
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == request.run_id)
            .ok_or_else(|| {
                DbmError::Configuration(format!(
                    "run {} was not found in this namespace",
                    request.run_id
                ))
            })?;
        if run.direction == "reconcile" || run.state == "reconciled" {
            return Err(DbmError::Safety(format!(
                "run {} has already been reconciled",
                request.run_id
            )));
        }
        let active_lock = snapshot
            .lock
            .as_ref()
            .filter(|lock| lock.state == "observed_active");
        let lock_matches_run = active_lock.is_some_and(|lock| {
            lock.run_id.as_deref() == Some(request.run_id)
        });
        let unresolved_run = matches!(run.state.as_str(), "planned" | "started" | "indeterminate");
        let terminal_run_with_held_lock =
            matches!(run.state.as_str(), "succeeded" | "failed") && lock_matches_run;
        if !unresolved_run && !terminal_run_with_held_lock {
            return Err(DbmError::Safety(format!(
                "run {} is {}, not an unresolved run or a run with a held diagnostic lock",
                request.run_id, run.state
            )));
        }
        if !lock_matches_run {
            return Err(DbmError::Lock(
                "PostgreSQL reconciliation requires an active diagnostic lock for the selected run"
                    .to_owned(),
            ));
        }
        let migration = migrations
            .iter()
            .find(|migration| migration.identity.version == run.migration_id)
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "source for run {} is missing; reconciliation cannot verify its identity",
                    run.run_id
                ))
            })?;
        if migration.checksum != run.checksum_at_execution {
            return Err(DbmError::Diverged(format!(
                "source checksum for run {} changed; reconciliation is blocked",
                run.run_id
            )));
        }
        if migration.dialect != crate::migration::SqlDialect::Postgres
            || migration.transaction != TransactionPolicy::Never
        {
            return Err(DbmError::Unsupported(
                "PostgreSQL catalog reconciliation currently supports only explicitly non-transactional concurrent-index migrations"
                    .to_owned(),
            ));
        }
        let up_statements = split_sql_statements_for_dialect(
            &migration.up_sql,
            crate::migration::SqlDialect::Postgres,
        );
        if up_statements.len() != 1 {
            return Err(DbmError::Diverged(
                "PostgreSQL reconciliation requires exactly one index statement in migration up"
                    .to_owned(),
            ));
        }
        validate_repair_plan(
            request.repair_plan_json,
            migration,
            &run.direction,
            request.operation_result,
        )?;
        let recorded_steps: Vec<&StepRecord> = snapshot
            .steps
            .iter()
            .filter(|step| step.run_id == run.run_id)
            .collect();
        if recorded_steps.len() > 1
            || recorded_steps.iter().any(|step| {
                step.step_index != 0
                    || step.transaction_class != "non_transactional"
                    || step.statement_hash != sql_hash(up_statements[0])
            })
        {
            return Err(DbmError::Diverged(
                "PostgreSQL run journal does not match the single non-transactional migration step"
                    .to_owned(),
            ));
        }
        let index_exists = session.inspect_postgres_index(up_statements[0]).await?;
        let observed_result = match (run.direction.as_str(), index_exists) {
            ("up", true) | ("down", false) => "operation_applied",
            ("up", false) | ("down", true) => "operation_not_applied",
            _ => {
                return Err(DbmError::Diverged(format!(
                    "run {} has unsupported direction {}",
                    run.run_id, run.direction
                )));
            }
        };
        if observed_result != request.operation_result {
            return Err(DbmError::Safety(format!(
                "requested result {} conflicts with the PostgreSQL catalog, which proves {observed_result}",
                request.operation_result
            )));
        }

        let applied = snapshot
            .applied
            .iter()
            .find(|applied| applied.version == run.migration_id);
        if let Some(applied) = applied
            && let Some(reason) = applied_mismatch(migration, applied)
        {
            return Err(DbmError::Diverged(format!(
                "current applied state for run {} diverges: {reason}",
                run.run_id
            )));
        }
        let should_be_applied = match (run.direction.as_str(), request.operation_result) {
            ("up", "operation_applied") | ("down", "operation_not_applied") => true,
            ("up", "operation_not_applied") | ("down", "operation_applied") => false,
            _ => {
                return Err(DbmError::Safety(
                    "PostgreSQL reconciliation request does not match the run direction".to_owned(),
                ));
            }
        };
        match (should_be_applied, applied.is_some()) {
            (true, false) => session
                .insert_applied(migration, &utc_timestamp()?)
                .await?,
            (false, true) => session.delete_applied(migration).await?,
            (true, true) | (false, false) => {}
        }

        let now = utc_timestamp()?;
        let reconciliation_run_id = Uuid::new_v4().to_string();
        session
            .reconcile_run(ReconciliationData {
                run_id: request.run_id,
                reconcile_run_id: &reconciliation_run_id,
                result: request.operation_result,
                evidence_ref: request.evidence_ref,
                repair_plan: request.repair_plan_json,
                reason: request.reason,
                actor: request.actor,
                now: &now,
            })
            .await?;
        session
            .release_reconciled_lock(request.run_id, &now)
            .await?;
        session.commit().await?;
        session.release_lock().await?;
        Ok(ReconcileReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor reconcile",
            status: "reconciled",
            target: config.target_description(),
            namespace: config.config.namespace.clone(),
            run_id: request.run_id.to_owned(),
            reconciliation_run_id,
            operation_result: request.operation_result.to_owned(),
            evidence_ref: request.evidence_ref.to_owned(),
        })
    }
    .await;
    let close = session.close().await;
    match (result, close) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(DbmError::ReconciliationRequired(format!(
            "reconciliation committed but the PostgreSQL session did not close cleanly: {error}"
        ))),
        (Ok(report), Ok(())) => Ok(report),
    }
}

async fn reconcile_mysql_async(
    config: &LoadedConfig,
    request: ReconcileRequest<'_>,
) -> Result<ReconcileReport> {
    validate_reconcile_request_fields(
        request.operation_result,
        request.evidence_ref,
        request.repair_plan_json,
        request.reason,
        request.actor,
        request.approved,
    )?;
    require_mutation_environment(config)?;
    let migrations = discover_on_worker(config).await?;
    let adapter = database::adapter_for(config)?;
    let mut session = adapter.connect(config, SessionIntent::ReadWrite).await?;
    let info = session.info();
    if info.dialect != "mysql" || info.server_version.is_empty() {
        let _ = session.close().await;
        return Err(DbmError::Driver {
            driver: "MySQL/MariaDB adapter",
            message: "connected session returned invalid server identity".to_owned(),
        });
    }

    let result = async {
        session
            .acquire_lock(Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
        session.begin().await?;
        let snapshot = session.read_snapshot(&config.config.namespace).await?;
        if snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION) {
            return Err(DbmError::Unsupported(format!(
                "MySQL reconciliation requires metadata format {}; found {:?}",
                crate::storage::METADATA_FORMAT_VERSION,
                snapshot.metadata_format_version
            )));
        }
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == request.run_id)
            .ok_or_else(|| {
                DbmError::Configuration(format!(
                    "run {} was not found in this namespace",
                    request.run_id
                ))
            })?;
        if run.direction == "reconcile" || run.state == "reconciled" {
            return Err(DbmError::Safety(format!(
                "run {} has already been reconciled",
                request.run_id
            )));
        }
        let active_lock = snapshot
            .lock
            .as_ref()
            .filter(|lock| lock.state == "observed_active");
        let lock_matches_run = active_lock.is_some_and(|lock| {
            lock.run_id.as_deref() == Some(request.run_id)
                && lock.lock_strategy == "mysql_get_lock_session"
        });
        let unresolved_run = matches!(run.state.as_str(), "planned" | "started" | "indeterminate");
        let terminal_run_with_held_lock =
            matches!(run.state.as_str(), "succeeded" | "failed") && lock_matches_run;
        if !unresolved_run && !terminal_run_with_held_lock {
            return Err(DbmError::Safety(format!(
                "run {} is {}, not an unresolved run or a run with a held diagnostic lock",
                request.run_id, run.state
            )));
        }
        if !lock_matches_run {
            return Err(DbmError::Lock(
                "MySQL reconciliation requires an active diagnostic lock for the selected run"
                    .to_owned(),
            ));
        }
        let migration = migrations
            .iter()
            .find(|migration| migration.identity.version == run.migration_id)
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "source for run {} is missing; reconciliation cannot verify its identity",
                    run.run_id
                ))
            })?;
        if migration.checksum != run.checksum_at_execution {
            return Err(DbmError::Diverged(format!(
                "source checksum for run {} changed; reconciliation is blocked",
                run.run_id
            )));
        }
        if migration.dialect != crate::migration::SqlDialect::Mysql
            || migration.transaction != TransactionPolicy::Never
        {
            return Err(DbmError::Unsupported(
                "MySQL reconciliation requires an explicitly non-transactional MySQL/MariaDB migration"
                    .to_owned(),
            ));
        }
        let direction_sql = match run.direction.as_str() {
            "up" => migration.up_sql.as_str(),
            "down" => migration.down_sql.as_deref().ok_or_else(|| {
                DbmError::Safety(format!(
                    "run {} has no down migration source to reconcile",
                    run.run_id
                ))
            })?,
            other => {
                return Err(DbmError::Safety(format!(
                    "run direction {other} cannot be reconciled as a migration"
                )));
            }
        };
        let statements = split_sql_statements_for_dialect(
            direction_sql,
            crate::migration::SqlDialect::Mysql,
        );
        if statements.is_empty() {
            return Err(DbmError::Diverged(
                "MySQL reconciliation source contains no SQL statements".to_owned(),
            ));
        }
        for statement in &statements {
            crate::migration::validate_mysql_migration_statement(statement)?;
        }
        validate_repair_plan(
            request.repair_plan_json,
            migration,
            &run.direction,
            request.operation_result,
        )?;
        let recorded_steps: Vec<&StepRecord> = snapshot
            .steps
            .iter()
            .filter(|step| step.run_id == run.run_id)
            .collect();
        if recorded_steps.len() > statements.len()
            || recorded_steps.iter().any(|step| {
                let index = usize::try_from(step.step_index).ok();
                index.is_none_or(|index| {
                    step.direction != run.direction
                        || step.transaction_class != "non_transactional"
                        || statements
                            .get(index)
                            .is_none_or(|statement| step.statement_hash != sql_hash(statement))
                })
            })
        {
            return Err(DbmError::Diverged(
                "MySQL run journal does not match the non-transactional migration source"
                    .to_owned(),
            ));
        }

        let applied = snapshot
            .applied
            .iter()
            .find(|applied| applied.version == run.migration_id);
        if let Some(applied) = applied
            && let Some(reason) = applied_mismatch(migration, applied)
        {
            return Err(DbmError::Diverged(format!(
                "current applied state for run {} diverges: {reason}",
                run.run_id
            )));
        }
        let should_be_applied = match (run.direction.as_str(), request.operation_result) {
            ("up", "operation_applied") | ("down", "operation_not_applied") => true,
            ("up", "operation_not_applied") | ("down", "operation_applied") => false,
            _ => {
                return Err(DbmError::Safety(
                    "MySQL reconciliation request does not match the run direction".to_owned(),
                ));
            }
        };
        match (should_be_applied, applied.is_some()) {
            (true, false) => session
                .insert_applied(migration, &utc_timestamp()?)
                .await?,
            (false, true) => session.delete_applied(migration).await?,
            (true, true) | (false, false) => {}
        }

        let now = utc_timestamp()?;
        let reconciliation_run_id = Uuid::new_v4().to_string();
        session
            .reconcile_run(ReconciliationData {
                run_id: request.run_id,
                reconcile_run_id: &reconciliation_run_id,
                result: request.operation_result,
                evidence_ref: request.evidence_ref,
                repair_plan: request.repair_plan_json,
                reason: request.reason,
                actor: request.actor,
                now: &now,
            })
            .await?;
        session
            .release_reconciled_lock(request.run_id, &now)
            .await?;
        session.commit().await?;
        session.release_lock().await?;
        Ok(ReconcileReport {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor reconcile",
            status: "reconciled",
            target: config.target_description(),
            namespace: config.config.namespace.clone(),
            run_id: request.run_id.to_owned(),
            reconciliation_run_id,
            operation_result: request.operation_result.to_owned(),
            evidence_ref: request.evidence_ref.to_owned(),
        })
    }
    .await;
    let close = session.close().await;
    match (result, close) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(DbmError::ReconciliationRequired(format!(
            "reconciliation committed but the MySQL session did not close cleanly: {error}"
        ))),
        (Ok(report), Ok(())) => Ok(report),
    }
}

fn validate_repair_plan(
    repair_plan_json: &str,
    migration: &Migration,
    direction: &str,
    operation_result: &str,
) -> Result<()> {
    let plan: RepairPlan = serde_json::from_str(repair_plan_json).map_err(|error| {
        DbmError::Configuration(format!(
            "repair plan must be valid structured JSON: {error}"
        ))
    })?;
    if plan.schema_version != "1.0" {
        return Err(DbmError::Configuration(format!(
            "unsupported repair-plan schema version {}; expected 1.0",
            plan.schema_version
        )));
    }
    if plan.summary.trim().is_empty() || plan.summary.len() > 2_048 {
        return Err(DbmError::Configuration(
            "repair-plan summary must contain 1 to 2048 bytes".to_owned(),
        ));
    }

    let sql = match direction {
        "up" => migration.up_sql.as_str(),
        "down" => migration.down_sql.as_deref().ok_or_else(|| {
            DbmError::Safety(format!(
                "run for migration {} has no down source to validate its repair plan",
                migration.identity.version
            ))
        })?,
        other => {
            return Err(DbmError::Safety(format!(
                "run direction {other} cannot be reconciled as a migration"
            )));
        }
    };
    let statements = split_sql_statements_for_dialect(sql, migration.dialect);
    if statements.len() != plan.steps.len() {
        return Err(DbmError::Safety(format!(
            "repair plan has {} step(s), but migration {} {direction} contains {} statement(s)",
            plan.steps.len(),
            migration.identity.version,
            statements.len()
        )));
    }

    let expected_state = match operation_result {
        "operation_applied" => RepairPlanStepState::ConfirmedApplied,
        "operation_not_applied" => RepairPlanStepState::ConfirmedNotApplied,
        other => {
            return Err(DbmError::Configuration(format!(
                "unsupported repair-plan operation result {other}"
            )));
        }
    };
    for (index, (statement, step)) in statements.iter().zip(&plan.steps).enumerate() {
        let expected_hash = sql_hash(statement);
        if step.step_index != index || step.statement_hash != expected_hash {
            return Err(DbmError::Safety(format!(
                "repair-plan step {} does not match statement index/hash for migration {} {direction}",
                step.step_index, migration.identity.version
            )));
        }
        if step.inspection.trim().is_empty() || step.inspection.len() > 1_000 {
            return Err(DbmError::Configuration(format!(
                "repair-plan step {index} inspection must contain 1 to 1000 bytes"
            )));
        }
        if step.state == RepairPlanStepState::Indeterminate {
            return Err(DbmError::Safety(format!(
                "repair-plan step {index} remains indeterminate; inspect it before reconciling"
            )));
        }
        if step.state != expected_state {
            return Err(DbmError::Safety(format!(
                "repair-plan step {index} conflicts with the requested operation result {operation_result}"
            )));
        }
    }
    Ok(())
}

fn is_raw_evidence_checksum(evidence_ref: &str) -> bool {
    let Some(digest) = evidence_ref.strip_prefix("sha256:raw:v1:") else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_metadata_version(connection: &Connection) -> Result<Option<i64>> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master \
         WHERE type = 'table' AND name = 'schema_migration_meta')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let version = connection
        .query_row(
            "SELECT metadata_format_version FROM main.schema_migration_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(version)
}

pub fn up(config: &LoadedConfig) -> Result<ExecutionReport> {
    let database_path = config.sqlite_database_path()?;
    require_mutation_environment(config)?;
    ensure_metadata_path_is_not_directory(database_path)?;

    let migrations = discover(config)?;
    let snapshot = read_snapshot(config)?;
    let initial_plan = plan_from_snapshot(config, &migrations, &snapshot)?;
    if initial_plan.operations.is_empty() {
        return Ok(execution_report(
            config,
            "up",
            "no_changes",
            None,
            Vec::new(),
        ));
    }

    let manifest = manifest_hash(&migrations);
    let operation_plan_hash = initial_plan.plan_hash.clone();
    let by_identity: BTreeMap<(&str, &str), &Migration> = migrations
        .iter()
        .map(|migration| {
            (
                (
                    migration_kind_label(migration.kind),
                    migration.identity.version.as_str(),
                ),
                migration,
            )
        })
        .collect();
    let batch_id = Uuid::new_v4().to_string();
    let mut connection = open_mutable(config)?;
    let mut applied = Vec::new();

    for operation in initial_plan.operations {
        let operation_kind = migration_kind_label(operation.kind);
        let migration = by_identity
            .get(&(operation_kind, operation.migration_id.as_str()))
            .copied()
            .ok_or_else(|| {
                DbmError::Execution(format!(
                    "planned migration {} disappeared from the source manifest",
                    operation.migration_id
                ))
            })?;
        if run_migration_with_journal(
            config,
            &mut connection,
            MigrationOperation {
                migration,
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &operation_plan_hash,
                direction: if migration.kind == crate::migration::MigrationKind::Repeatable {
                    "repeat"
                } else {
                    "up"
                },
            },
        )? {
            applied.push(migration.identity.version.clone());
        }
    }

    Ok(execution_report(
        config,
        "up",
        if applied.is_empty() {
            "no_changes"
        } else {
            "succeeded"
        },
        Some(batch_id),
        applied,
    ))
}

pub fn down(config: &LoadedConfig, steps: usize) -> Result<ExecutionReport> {
    let database_path = config.sqlite_database_path()?;
    require_mutation_environment(config)?;
    ensure_metadata_path_is_not_directory(database_path)?;
    if steps == 0 {
        return Err(DbmError::Configuration(
            "down steps must be greater than zero".to_owned(),
        ));
    }

    let migrations = discover(config)?;
    let snapshot = read_snapshot(config)?;
    let entries = build_status_entries(&migrations, &snapshot)?;
    reject_invalid_status(&entries)?;

    let by_identity: BTreeMap<(&str, &str), &Migration> = migrations
        .iter()
        .map(|migration| {
            (
                (
                    migration_kind_label(migration.kind),
                    migration.identity.version.as_str(),
                ),
                migration,
            )
        })
        .collect();
    let mut applied: Vec<AppliedMigration> = snapshot
        .applied
        .iter()
        .filter(|migration| migration.kind == "versioned")
        .cloned()
        .collect();
    applied.sort_by_key(|migration| Reverse(migration.apply_order));
    if applied.is_empty() {
        return Ok(execution_report(
            config,
            "down",
            "no_changes",
            None,
            Vec::new(),
        ));
    }
    if steps > applied.len() {
        return Err(DbmError::Configuration(format!(
            "requested {steps} rollback step(s), but only {} migration(s) are applied",
            applied.len()
        )));
    }

    let selected: Vec<&AppliedMigration> = applied.iter().take(steps).collect();
    for applied_migration in &selected {
        let migration = by_identity
            .get(&("versioned", applied_migration.version.as_str()))
            .copied()
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "source file for applied migration {} is missing",
                    applied_migration.version
                ))
            })?;
        if migration.down_sql.is_none() {
            return Err(DbmError::Safety(format!(
                "migration {} is irreversible because it has no down section",
                migration.identity.version
            )));
        }
    }

    let manifest = manifest_hash(&migrations);
    let operation_plan_hash = plan_hash(config, &migrations, &snapshot);
    let batch_id = Uuid::new_v4().to_string();
    let mut connection = open_mutable(config)?;
    let mut reverted = Vec::new();

    for applied_migration in applied.into_iter().take(steps) {
        let migration = by_identity
            .get(&("versioned", applied_migration.version.as_str()))
            .copied()
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "source file for applied migration {} is missing",
                    applied_migration.version
                ))
            })?;
        if migration.down_sql.is_none() {
            return Err(DbmError::Safety(format!(
                "migration {} is irreversible because it has no down section",
                migration.identity.version
            )));
        }
        if run_migration_with_journal(
            config,
            &mut connection,
            MigrationOperation {
                migration,
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &operation_plan_hash,
                direction: "down",
            },
        )? {
            reverted.push(migration.identity.version.clone());
        }
    }

    Ok(execution_report(
        config,
        "down",
        if reverted.is_empty() {
            "no_changes"
        } else {
            "succeeded"
        },
        Some(batch_id),
        reverted,
    ))
}

async fn execute_with_adapter(
    config: &LoadedConfig,
    direction: &'static str,
    down_steps: usize,
    registry: Option<&MigrationRegistry>,
) -> Result<ExecutionReport> {
    require_mutation_environment(config)?;
    if config.config.target.driver == "sqlite" {
        ensure_metadata_path_is_not_directory(config.sqlite_database_path()?)?;
    }
    if direction == "down" && down_steps == 0 {
        return Err(DbmError::Configuration(
            "down steps must be greater than zero".to_owned(),
        ));
    }

    let migrations = discover_on_worker_with_registry(config, registry).await?;
    let snapshot = read_adapter_snapshot(config).await?;
    if matches!(config.config.target.driver.as_str(), "postgres" | "mysql")
        && snapshot.initialized
        && snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
    {
        return Err(DbmError::Unsupported(format!(
            "{} mutations require metadata format {}; an explicit metadata upgrade is not available; found {:?}",
            config.config.target.driver,
            crate::storage::METADATA_FORMAT_VERSION,
            snapshot.metadata_format_version
        )));
    }
    if direction == "down"
        && snapshot
            .legacy_applied
            .iter()
            .any(|migration| migration.verification_status != "adopted")
    {
        return Err(DbmError::Safety(
            "rollback is blocked while imported legacy migrations remain unadopted; verify the import and explicitly adopt each current legacy migration first"
                .to_owned(),
        ));
    }
    let mut operations = Vec::new();
    let operation_plan_hash;
    if direction == "up" {
        let plan = plan_from_snapshot(config, &migrations, &snapshot)?;
        operation_plan_hash = plan.plan_hash;
        let by_identity: BTreeMap<(&str, &str), &Migration> = migrations
            .iter()
            .map(|migration| {
                (
                    (
                        migration_kind_label(migration.kind),
                        migration.identity.version.as_str(),
                    ),
                    migration,
                )
            })
            .collect();
        for operation in plan.operations {
            let migration = by_identity
                .get(&(
                    migration_kind_label(operation.kind),
                    operation.migration_id.as_str(),
                ))
                .copied()
                .ok_or_else(|| {
                    DbmError::Execution(format!(
                        "planned migration {} disappeared from the source manifest",
                        operation.migration_id
                    ))
                })?;
            operations.push(migration);
        }
    } else {
        let entries = build_status_entries(&migrations, &snapshot)?;
        reject_invalid_status(&entries)?;
        let mut applied: Vec<AppliedMigration> = snapshot
            .applied
            .iter()
            .filter(|migration| migration.kind == "versioned")
            .cloned()
            .collect();
        applied.sort_by_key(|migration| Reverse(migration.apply_order));
        if applied.is_empty() {
            return Ok(execution_report(
                config,
                direction,
                "no_changes",
                None,
                Vec::new(),
            ));
        }
        if down_steps > applied.len() {
            return Err(DbmError::Configuration(format!(
                "requested {down_steps} rollback step(s), but only {} migration(s) are applied",
                applied.len()
            )));
        }
        let by_identity: BTreeMap<(&str, &str), &Migration> = migrations
            .iter()
            .map(|migration| {
                (
                    (
                        migration_kind_label(migration.kind),
                        migration.identity.version.as_str(),
                    ),
                    migration,
                )
            })
            .collect();
        for applied_migration in applied.iter().take(down_steps) {
            let migration = by_identity
                .get(&("versioned", applied_migration.version.as_str()))
                .copied()
                .ok_or_else(|| {
                    DbmError::Diverged(format!(
                        "source file for applied migration {} is missing",
                        applied_migration.version
                    ))
                })?;
            if migration.down_sql.is_none() {
                return Err(DbmError::Safety(format!(
                    "migration {} is irreversible because it has no down section",
                    migration.identity.version
                )));
            }
            operations.push(migration);
        }
        operation_plan_hash = plan_hash(config, &migrations, &snapshot);
    }

    if operations.is_empty() {
        return Ok(execution_report(
            config,
            direction,
            "no_changes",
            None,
            Vec::new(),
        ));
    }
    for migration in &operations {
        let dialect_matches = matches!(
            (config.config.target.driver.as_str(), migration.dialect),
            ("sqlite", crate::migration::SqlDialect::Sqlite)
                | ("postgres", crate::migration::SqlDialect::Postgres)
                | ("mysql", crate::migration::SqlDialect::Mysql)
        );
        if !dialect_matches {
            return Err(DbmError::Safety(format!(
                "migration {} uses a dialect that does not match the target",
                migration.identity.version
            )));
        }
        let sql = if direction == "down" {
            migration.down_sql.as_deref().ok_or_else(|| {
                DbmError::Safety(format!(
                    "migration {} is irreversible because it has no down section",
                    migration.identity.version
                ))
            })?
        } else {
            migration.up_sql.as_str()
        };
        let statements = split_sql_statements_for_dialect(sql, migration.dialect);
        if statements.is_empty() {
            return Err(DbmError::Execution(format!(
                "migration {} {direction} contains no SQL statements",
                migration.identity.version
            )));
        }
        for statement in statements {
            if migration.transaction == TransactionPolicy::Never {
                match migration.dialect {
                    crate::migration::SqlDialect::Postgres => {
                        crate::migration::validate_postgres_nontransactional_statement(statement)?;
                    }
                    crate::migration::SqlDialect::Mysql => {
                        crate::migration::validate_mysql_migration_statement(statement)?;
                    }
                    crate::migration::SqlDialect::Sqlite => {
                        return Err(DbmError::Unsupported(
                            "non-transactional execution is unavailable for SQLite".to_owned(),
                        ));
                    }
                }
            } else if migration.dialect == crate::migration::SqlDialect::Postgres {
                crate::migration::validate_postgres_migration_statement(statement)?;
            }
        }
        if migration.transaction == TransactionPolicy::Never
            && snapshot.initialized
            && snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
        {
            return Err(DbmError::Unsupported(format!(
                "migration {} requires metadata format {}; upgrade PostgreSQL metadata explicitly before running non-transactional SQL",
                migration.identity.version,
                crate::storage::METADATA_FORMAT_VERSION
            )));
        }
    }

    let manifest = manifest_hash(&migrations);
    let batch_id = Uuid::new_v4().to_string();
    let adapter = database::adapter_for(config)?;
    let mut session = adapter.connect(config, SessionIntent::ReadWrite).await?;
    let info = session.info();
    if info.dialect != config.config.target.driver || info.server_version.is_empty() {
        let _ = session.close().await;
        return Err(DbmError::Driver {
            driver: "database adapter",
            message: "connected session returned invalid server identity".to_owned(),
        });
    }
    if let Some(registry) = registry
        && let Err(error) = registry.validate_database_info(info)
    {
        let _ = session.close().await;
        return Err(error);
    }

    let mut completed = Vec::new();
    let execution = async {
        for migration in operations {
            if run_adapter_migration(
                config,
                session.as_mut(),
                &migrations,
                registry,
                MigrationOperation {
                    migration,
                    initial_manifest: &manifest,
                    batch_id: &batch_id,
                    plan_hash: &operation_plan_hash,
                    direction: if direction == "up"
                        && migration.kind == crate::migration::MigrationKind::Repeatable
                    {
                        "repeat"
                    } else {
                        direction
                    },
                },
            )
            .await?
            {
                completed.push(migration.identity.version.clone());
            }
        }
        Ok::<(), DbmError>(())
    }
    .await;
    let close = session.close().await;
    match (execution, close) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(DbmError::ReconciliationRequired(format!(
            "migration execution finished but the database session did not close cleanly: {error}"
        ))),
        (Ok(()), Ok(())) => Ok(execution_report(
            config,
            direction,
            if completed.is_empty() {
                "no_changes"
            } else {
                "succeeded"
            },
            Some(batch_id),
            completed,
        )),
    }
}

async fn run_adapter_migration(
    config: &LoadedConfig,
    session: &mut dyn database::DatabaseSession,
    initial_migrations: &[Migration],
    registry: Option<&MigrationRegistry>,
    operation: MigrationOperation<'_>,
) -> Result<bool> {
    let MigrationOperation {
        migration,
        batch_id,
        plan_hash,
        direction,
        ..
    } = operation;
    let run_id = Uuid::new_v4().to_string();
    let owner_id = Uuid::new_v4().to_string();
    let uses_native_lock = matches!(config.config.target.driver.as_str(), "postgres" | "mysql");
    if uses_native_lock {
        session
            .acquire_lock(Duration::from_secs(
                config.config.execution.busy_timeout_seconds,
            ))
            .await?;
    }

    session.begin().await?;
    let created_at = utc_timestamp()?;
    let preparation = async {
        session.initialize_metadata(&created_at).await?;
        let snapshot = session.read_snapshot(&config.config.namespace).await?;
        ensure_snapshot_is_writable(&snapshot, None)?;
        if uses_native_lock
            && snapshot.initialized
            && snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION)
        {
            return Err(DbmError::Unsupported(format!(
                "database mutations require metadata format {}; found {:?}",
                crate::storage::METADATA_FORMAT_VERSION,
                snapshot.metadata_format_version
            )));
        }
        ensure_adapter_operation_is_current(initial_migrations, migration, direction, &snapshot)?;
        session
            .insert_planned_run(
                migration,
                PlannedRunData {
                    run_id: &run_id,
                    batch_id,
                    direction,
                    created_at: &created_at,
                    plan_hash,
                },
            )
            .await?;
        session
            .record_lock(LockData {
                owner_id: &owner_id,
                run_id: &run_id,
                now: &created_at,
            })
            .await?;
        Ok::<(), DbmError>(())
    }
    .await;
    if let Err(error) = preparation {
        let rollback = session.rollback().await;
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(DbmError::ReconciliationRequired(format!(
                "preparing migration {} failed and its transaction could not be closed: {rollback_error}",
                migration.identity.version
            ))),
        };
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "creation of run {run_id} and its lock has an uncertain commit result: {error}"
        ))
    })?;

    let started_at = utc_timestamp().map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} was planned but its started time could not be created: {error}"
        ))
    })?;
    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} was planned but could not begin its started-state transaction: {error}"
        ))
    })?;
    if let Err(error) = session.mark_run_started(&run_id, &started_at).await {
        let rollback = session.rollback().await;
        return Err(DbmError::ReconciliationRequired(format!(
            "run {run_id} could not be durably marked started (write error: {error}; rollback: {rollback:?})"
        )));
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "starting run {run_id} has an uncertain commit result: {error}"
        ))
    })?;
    let started = Instant::now();

    if migration.transaction == TransactionPolicy::Never {
        execute_adapter_nontransactional_migration(
            config, session, registry, operation, &run_id, &owner_id, started,
        )
        .await?;
        release_adapter_lock(session, uses_native_lock, &owner_id).await?;
        return Ok(true);
    }

    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} is started but its migration transaction could not begin: {error}"
        ))
    })?;
    let mut steps = Vec::new();
    let migration_result = execute_adapter_migration_in_transaction(
        config,
        session,
        registry,
        operation,
        AdapterMigrationRun {
            run_id: &run_id,
            owner_id: &owner_id,
            started,
            steps: &mut steps,
        },
    )
    .await;
    match migration_result {
        Ok(()) => {
            session.commit().await.map_err(|error| {
                DbmError::ReconciliationRequired(format!(
                    "commit result is uncertain for migration {}; inspect with doctor before retrying: {error}",
                    migration.identity.version
                ))
            })?;
        }
        Err(error) if matches!(error, DbmError::ReconciliationRequired(_)) => return Err(error),
        Err(error) => {
            session.rollback().await.map_err(|rollback_error| {
                DbmError::ReconciliationRequired(format!(
                    "migration {} failed and rollback could not be confirmed: {rollback_error}",
                    migration.identity.version
                ))
            })?;
            persist_adapter_failed_run(session, &run_id, direction, started, &steps).await?;
            release_adapter_lock(session, uses_native_lock, &owner_id).await?;
            return Err(error);
        }
    }

    release_adapter_lock(session, uses_native_lock, &owner_id).await?;
    Ok(true)
}

async fn execute_adapter_nontransactional_migration(
    config: &LoadedConfig,
    session: &mut dyn database::DatabaseSession,
    registry: Option<&MigrationRegistry>,
    operation: MigrationOperation<'_>,
    run_id: &str,
    owner_id: &str,
    started: Instant,
) -> Result<()> {
    let MigrationOperation {
        migration,
        initial_manifest,
        plan_hash,
        direction,
        ..
    } = operation;
    let step_direction = if direction == "down" { "down" } else { "up" };
    if !matches!(config.config.target.driver.as_str(), "postgres" | "mysql")
        || !matches!(
            (config.config.target.driver.as_str(), migration.dialect),
            ("postgres", crate::migration::SqlDialect::Postgres)
                | ("mysql", crate::migration::SqlDialect::Mysql)
        )
        || migration.transaction != TransactionPolicy::Never
    {
        return Err(DbmError::Unsupported(
            "non-transactional execution requires an explicitly marked PostgreSQL or MySQL migration"
                .to_owned(),
        ));
    }

    let current_migrations = discover_on_worker_with_registry(config, registry).await?;
    if manifest_hash(&current_migrations) != initial_manifest {
        return Err(DbmError::ReconciliationRequired(format!(
            "migration sources changed after run {run_id} was started; inspect before retrying"
        )));
    }
    let snapshot = session.read_snapshot(&config.config.namespace).await?;
    ensure_snapshot_is_writable(&snapshot, Some((run_id, owner_id)))?;
    if snapshot.metadata_format_version != Some(crate::storage::METADATA_FORMAT_VERSION) {
        return Err(DbmError::ReconciliationRequired(format!(
            "run {run_id} requires metadata format {} for its non-transactional journal",
            crate::storage::METADATA_FORMAT_VERSION
        )));
    }
    ensure_adapter_operation_is_current(&current_migrations, migration, direction, &snapshot)?;
    let current_migration = current_migrations
        .iter()
        .find(|current| current.migration_key() == migration.migration_key())
        .ok_or_else(|| {
            DbmError::ReconciliationRequired(format!(
                "migration source {} disappeared after run {run_id} was started",
                migration.identity.version
            ))
        })?;
    let sql = if direction == "down" {
        current_migration.down_sql.as_deref().ok_or_else(|| {
            DbmError::ReconciliationRequired(format!(
                "migration {} has no down section after run {run_id} was started",
                migration.identity.version
            ))
        })?
    } else {
        current_migration.up_sql.as_str()
    };
    let statements = split_sql_statements_for_dialect(sql, current_migration.dialect);
    if statements.is_empty() {
        return Err(DbmError::ReconciliationRequired(format!(
            "migration {} has no SQL steps after run {run_id} was started",
            migration.identity.version
        )));
    }
    for statement in &statements {
        let validation = match current_migration.dialect {
            crate::migration::SqlDialect::Postgres => {
                crate::migration::validate_postgres_nontransactional_statement(statement)
            }
            crate::migration::SqlDialect::Mysql => {
                crate::migration::validate_mysql_migration_statement(statement)
            }
            crate::migration::SqlDialect::Sqlite => Err(DbmError::Unsupported(
                "SQLite does not support non-transactional statements".to_owned(),
            )),
        };
        validation.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "non-transactional run {run_id} failed preflight before executing SQL: {error}"
            ))
        })?;
    }

    for (index, statement) in statements.iter().enumerate() {
        let latest_migrations = discover_on_worker_with_registry(config, registry)
            .await
            .map_err(|error| {
                DbmError::ReconciliationRequired(format!(
                    "could not revalidate sources during non-transactional run {run_id}: {error}"
                ))
            })?;
        if manifest_hash(&latest_migrations) != initial_manifest {
            return Err(DbmError::ReconciliationRequired(format!(
                "migration sources changed while non-transactional run {run_id} was executing"
            )));
        }

        let step_index = i64::try_from(index).map_err(|_| {
            DbmError::ReconciliationRequired(format!(
                "run {run_id} exceeds the journal step index range"
            ))
        })?;
        let statement_hash = sql_hash(statement);
        let step_key = step_key(plan_hash, step_direction, step_index, &statement_hash);
        let step_started_at = utc_timestamp().map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "could not timestamp non-transactional step {step_index} for run {run_id}: {error}"
            ))
        })?;
        let attempt = StepAttempt {
            step_index,
            step_key,
            statement_hash,
            statement_kind: statement_kind(statement),
            transaction_class: "non_transactional",
            state: "intent_recorded".to_owned(),
            started_at: step_started_at,
            acknowledged_at: None,
            error_code: None,
            evidence: None,
        };

        session.begin().await.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "could not persist intent for non-transactional step {step_index} of run {run_id}: {error}"
            ))
        })?;
        if let Err(error) = session
            .insert_step(StepData {
                run_id,
                step_index,
                step_key: &attempt.step_key,
                direction: step_direction,
                statement_hash: &attempt.statement_hash,
                statement_kind: &attempt.statement_kind,
                transaction_class: attempt.transaction_class,
                state: "intent_recorded",
                started_at: &attempt.started_at,
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            })
            .await
        {
            let rollback = session.rollback().await;
            return Err(DbmError::ReconciliationRequired(format!(
                "could not persist intent for non-transactional step {step_index} of run {run_id}: {error}; rollback: {rollback:?}"
            )));
        }
        session.commit().await.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "intent commit for non-transactional step {step_index} of run {run_id} is uncertain: {error}"
            ))
        })?;

        if let Err(error) = session.execute_nontransactional_statement(statement).await {
            if matches!(error, DbmError::ReconciliationRequired(_)) {
                return Err(error);
            }
            persist_adapter_indeterminate_step(
                session,
                run_id,
                step_index,
                started,
                "NON_TRANSACTIONAL_STATEMENT_RESULT_UNKNOWN",
                "database rejected or may have partially completed a non-transactional statement; inspect its effects before reconciliation",
            )
            .await?;
            return Err(DbmError::ReconciliationRequired(format!(
                "non-transactional step {step_index} of run {run_id} requires explicit inspection and reconciliation: {error}"
            )));
        }

        let acknowledged_at = utc_timestamp().map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "statement {step_index} for run {run_id} completed but could not be timestamped for acknowledgement: {error}"
            ))
        })?;
        session.begin().await.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "statement {step_index} for run {run_id} completed but its ACK transaction could not begin: {error}"
            ))
        })?;
        if let Err(error) = session
            .acknowledge_step(run_id, step_index, &acknowledged_at)
            .await
        {
            let rollback = session.rollback().await;
            return Err(DbmError::ReconciliationRequired(format!(
                "statement {step_index} for run {run_id} completed but its journal ACK failed: {error}; rollback: {rollback:?}"
            )));
        }
        session.commit().await.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "statement {step_index} for run {run_id} completed but its ACK commit is uncertain: {error}"
            ))
        })?;
    }

    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "all statements in run {run_id} were acknowledged but the final metadata transaction could not begin: {error}"
        ))
    })?;
    let finalization = async {
        if direction != "down" {
            session
                .insert_applied(current_migration, &utc_timestamp()?)
                .await?;
        } else {
            session.delete_applied(current_migration).await?;
        }
        let ended_at = utc_timestamp()?;
        session
            .finish_run(FinishedRunData {
                run_id,
                state: "succeeded",
                ended_at: &ended_at,
                duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                error_code: None,
                error_summary_redacted: None,
            })
            .await
    }
    .await;
    if let Err(error) = finalization {
        let rollback = session.rollback().await;
        return Err(DbmError::ReconciliationRequired(format!(
            "all statements in run {run_id} were acknowledged but migration state could not be finalized: {error}; rollback: {rollback:?}"
        )));
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "final metadata commit for non-transactional run {run_id} is uncertain: {error}"
        ))
    })
}

async fn persist_adapter_indeterminate_step(
    session: &mut dyn database::DatabaseSession,
    run_id: &str,
    step_index: i64,
    started: Instant,
    error_code: &str,
    evidence: &str,
) -> Result<()> {
    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} has an unresolved non-transactional statement and its journal could not be updated: {error}"
        ))
    })?;
    let ended_at = utc_timestamp().map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} has an unresolved non-transactional statement and its terminal time is unavailable: {error}"
        ))
    })?;
    let update = async {
        session
            .mark_step_indeterminate(run_id, step_index, error_code, evidence)
            .await?;
        session
            .finish_run(FinishedRunData {
                run_id,
                state: "indeterminate",
                ended_at: &ended_at,
                duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                error_code: Some(error_code),
                error_summary_redacted: Some(
                    "non-transactional SQL outcome requires catalog inspection and explicit reconciliation",
                ),
            })
            .await
    }
    .await;
    if let Err(error) = update {
        let rollback = session.rollback().await;
        return Err(DbmError::ReconciliationRequired(format!(
            "run {run_id} has an unresolved non-transactional statement and its journal update failed: {error}; rollback: {rollback:?}"
        )));
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "indeterminate journal commit for non-transactional run {run_id} is uncertain: {error}"
        ))
    })
}

async fn execute_adapter_migration_in_transaction(
    config: &LoadedConfig,
    session: &mut dyn database::DatabaseSession,
    registry: Option<&MigrationRegistry>,
    operation: MigrationOperation<'_>,
    run: AdapterMigrationRun<'_>,
) -> Result<()> {
    let AdapterMigrationRun {
        run_id,
        owner_id,
        started,
        steps,
    } = run;
    let MigrationOperation {
        migration,
        initial_manifest,
        plan_hash,
        direction,
        ..
    } = operation;
    let step_direction = if direction == "down" { "down" } else { "up" };
    let discovered_migrations = discover_on_worker_with_registry(config, registry).await?;
    if manifest_hash(&discovered_migrations) != initial_manifest {
        return Err(DbmError::Safety(
            "migration sources changed after planning; rerun plan before applying".to_owned(),
        ));
    }
    let snapshot = session.read_snapshot(&config.config.namespace).await?;
    ensure_snapshot_is_writable(&snapshot, Some((run_id, owner_id)))?;
    ensure_adapter_operation_is_current(&discovered_migrations, migration, direction, &snapshot)?;
    let current_migration = discovered_migrations
        .iter()
        .find(|current| current.migration_key() == migration.migration_key())
        .ok_or_else(|| {
            DbmError::Diverged(format!(
                "migration source {} disappeared during execution",
                migration.identity.version
            ))
        })?;
    if let Some(applied) = snapshot.applied.iter().find(|applied| {
        applied.kind == migration_kind_label(migration.kind)
            && applied.version == migration.identity.version
    }) && direction != "down"
    {
        if migration.kind == crate::migration::MigrationKind::Repeatable
            && applied.checksum != current_migration.checksum
        {
            // Changed content is the expected repeatable re-execution case.
        } else {
            if let Some(reason) = applied_mismatch(current_migration, applied) {
                return Err(DbmError::Diverged(format!(
                    "migration {} changed while another executor applied it: {reason}",
                    migration.identity.version
                )));
            }
            return Err(DbmError::Safety(format!(
                "migration {} became applied after planning; inspect status before retrying",
                migration.identity.version
            )));
        }
    }

    let sql = if direction == "down" {
        current_migration.down_sql.as_deref().ok_or_else(|| {
            DbmError::Safety(format!(
                "migration {} is irreversible because it has no down section",
                migration.identity.version
            ))
        })?
    } else {
        current_migration.up_sql.as_str()
    };
    let statements = split_sql_statements_for_dialect(sql, current_migration.dialect);
    if statements.is_empty() {
        return Err(DbmError::Execution(format!(
            "migration {} {direction} contains no SQL statements",
            migration.identity.version
        )));
    }

    let rust_migration =
        registry.and_then(|registry| registry.rust_migration(&current_migration.identity));
    if let Some(rust_migration) = rust_migration {
        let expected_statements = statements
            .iter()
            .map(|statement| (*statement).to_owned())
            .collect();
        let mut context = MigrationContext::new(
            session,
            current_migration,
            run_id,
            plan_hash,
            step_direction,
            expected_statements,
            steps,
        );
        let result = if direction != "down" {
            rust_migration.up(&mut context).await
        } else {
            rust_migration.down(&mut context).await
        };
        result?;
        context.ensure_plan_consumed()?;
    } else {
        for (index, statement) in statements.iter().enumerate() {
            let step_index = i64::try_from(index).map_err(|_| {
                DbmError::Execution(
                    "migration contains more SQL steps than the journal supports".to_owned(),
                )
            })?;
            let statement_hash = sql_hash(statement);
            let mut attempt = StepAttempt {
                step_index,
                step_key: step_key(plan_hash, step_direction, step_index, &statement_hash),
                statement_hash,
                statement_kind: statement_kind(statement),
                transaction_class: "transactional",
                state: "intent_recorded".to_owned(),
                started_at: utc_timestamp()?,
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            };
            session
                .insert_step(StepData {
                    run_id,
                    step_index,
                    step_key: &attempt.step_key,
                    direction: step_direction,
                    statement_hash: &attempt.statement_hash,
                    statement_kind: &attempt.statement_kind,
                    transaction_class: attempt.transaction_class,
                    state: "intent_recorded",
                    started_at: &attempt.started_at,
                    acknowledged_at: None,
                    error_code: None,
                    evidence: None,
                })
                .await?;
            let step_position = steps.len();
            steps.push(attempt.clone());

            let statement_result =
                if current_migration.dialect == crate::migration::SqlDialect::Postgres {
                    crate::migration::validate_postgres_migration_statement(statement)
                } else {
                    Ok(())
                };
            if let Err(error) = statement_result {
                attempt.state = "failed".to_owned();
                attempt.error_code = Some("MIGRATION_STATEMENT_REJECTED".to_owned());
                attempt.evidence =
                    Some("runner rejected the statement before sending it".to_owned());
                steps[step_position] = attempt;
                return Err(error);
            }
            if let Err(error) = session.execute_statement(statement).await {
                if matches!(error, DbmError::ReconciliationRequired(_)) {
                    return Err(error);
                }
                attempt.state = "failed".to_owned();
                attempt.error_code = Some("MIGRATION_STATEMENT_FAILED".to_owned());
                attempt.evidence = Some(
                    "driver rejected the statement; managed transaction will roll back".to_owned(),
                );
                steps[step_position] = attempt;
                return Err(error);
            }

            let acknowledged_at = match utc_timestamp() {
                Ok(timestamp) => timestamp,
                Err(error) => {
                    attempt.state = "failed".to_owned();
                    attempt.error_code = Some("STEP_ACKNOWLEDGEMENT_FAILED".to_owned());
                    attempt.evidence = Some(
                    "statement ran in the managed transaction but no acknowledgement time was available; transaction will roll back"
                        .to_owned(),
                );
                    steps[step_position] = attempt;
                    return Err(error);
                }
            };
            if let Err(error) = session
                .acknowledge_step(run_id, step_index, &acknowledged_at)
                .await
            {
                if matches!(error, DbmError::ReconciliationRequired(_)) {
                    return Err(error);
                }
                attempt.state = "failed".to_owned();
                attempt.error_code = Some("STEP_ACKNOWLEDGEMENT_FAILED".to_owned());
                attempt.evidence = Some(
                "statement ran in the managed transaction but journal acknowledgement failed; transaction will roll back"
                    .to_owned(),
            );
                steps[step_position] = attempt;
                return Err(error);
            }
            attempt.state = "acknowledged".to_owned();
            attempt.acknowledged_at = Some(acknowledged_at);
            steps[step_position] = attempt;
        }
    }

    if direction != "down" {
        session
            .insert_applied(current_migration, &utc_timestamp()?)
            .await?;
    } else {
        session.delete_applied(current_migration).await?;
    }
    let ended_at = utc_timestamp()?;
    session
        .finish_run(FinishedRunData {
            run_id,
            state: "succeeded",
            ended_at: &ended_at,
            duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
            error_code: None,
            error_summary_redacted: None,
        })
        .await
}

async fn persist_adapter_failed_run(
    session: &mut dyn database::DatabaseSession,
    run_id: &str,
    direction: &str,
    started: Instant,
    steps: &[StepAttempt],
) -> Result<()> {
    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} failed but its terminal state could not be persisted: {error}"
        ))
    })?;
    let ended_at = utc_timestamp()?;
    let persistence = async {
        session
            .finish_run(FinishedRunData {
                run_id,
                state: "failed",
                ended_at: &ended_at,
                duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                error_code: Some("MIGRATION_FAILED"),
                error_summary_redacted: Some(
                    "managed transaction rolled back; no migration state was committed",
                ),
            })
            .await?;
        for step in steps {
            let evidence = if step.state == "acknowledged" {
                Some("statement acknowledged inside a transaction that was rolled back")
            } else {
                step.evidence.as_deref()
            };
            session
                .insert_step(StepData {
                    run_id,
                    step_index: step.step_index,
                    step_key: &step.step_key,
                    direction,
                    statement_hash: &step.statement_hash,
                    statement_kind: &step.statement_kind,
                    transaction_class: step.transaction_class,
                    state: &step.state,
                    started_at: &step.started_at,
                    acknowledged_at: step.acknowledged_at.as_deref(),
                    error_code: step.error_code.as_deref(),
                    evidence,
                })
                .await?;
        }
        Ok::<(), DbmError>(())
    }
    .await;
    if let Err(error) = persistence {
        let rollback = session.rollback().await;
        return Err(DbmError::ReconciliationRequired(format!(
            "run {run_id} failed and its failure journal could not be committed: {error}; rollback: {rollback:?}"
        )));
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "failed-run journal for {run_id} has an uncertain commit result: {error}"
        ))
    })
}

async fn release_adapter_lock(
    session: &mut dyn database::DatabaseSession,
    uses_native_lock: bool,
    owner_id: &str,
) -> Result<()> {
    session.begin().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "migration finished but its recorded lock could not be released: {error}"
        ))
    })?;
    let released_at = utc_timestamp()?;
    if let Err(error) = session.release_recorded_lock(owner_id, &released_at).await {
        let rollback = session.rollback().await;
        return Err(DbmError::ReconciliationRequired(format!(
            "migration finished but its recorded lock could not be released: {error}; rollback: {rollback:?}"
        )));
    }
    session.commit().await.map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "release of recorded migration lock has an uncertain commit result: {error}"
        ))
    })?;
    if uses_native_lock {
        session.release_lock().await.map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "recorded native database lock was released but the session lock could not be confirmed released: {error}"
            ))
        })?;
    }
    Ok(())
}

fn ensure_snapshot_is_writable(
    snapshot: &MetadataSnapshot,
    own_run: Option<(&str, &str)>,
) -> Result<()> {
    for run in &snapshot.runs {
        if matches!(run.state.as_str(), "planned" | "started" | "indeterminate")
            && own_run.is_none_or(|(run_id, _)| run.run_id != run_id)
        {
            return Err(DbmError::Safety(format!(
                "run {} remains {}; inspect with doctor and reconcile explicitly before writing",
                run.run_id, run.state
            )));
        }
    }
    if let Some(lock) = &snapshot.lock {
        let owned_by_current_run = own_run.is_some_and(|(run_id, owner_id)| {
            lock.run_id.as_deref() == Some(run_id)
                && lock.owner_id == owner_id
                && lock.state == "observed_active"
        });
        if lock.state != "released" && !owned_by_current_run {
            return Err(DbmError::Safety(
                "migration lock metadata is not released; inspect with doctor and reconcile explicitly before writing"
                    .to_owned(),
            ));
        }
        if own_run.is_some() && !owned_by_current_run {
            return Err(DbmError::ReconciliationRequired(
                "current migration run no longer owns its recorded lock".to_owned(),
            ));
        }
    } else if own_run.is_some() {
        return Err(DbmError::ReconciliationRequired(
            "current migration run has no recorded lock metadata".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_adapter_operation_is_current(
    migrations: &[Migration],
    migration: &Migration,
    direction: &str,
    snapshot: &MetadataSnapshot,
) -> Result<()> {
    let entries = build_status_entries(migrations, snapshot)?;
    reject_invalid_status(&entries)?;
    if direction != "down" {
        let first_pending = migrations.iter().find(|candidate| {
            entries.iter().any(|entry| {
                entry.kind == migration_kind_label(candidate.kind)
                    && entry.migration_id == candidate.identity.version
                    && entry.state == "pending"
            })
        });
        if first_pending
            .is_none_or(|candidate| candidate.migration_key() != migration.migration_key())
        {
            return Err(DbmError::Safety(format!(
                "execution order changed after planning; expected {} {}, found {:?}; rerun plan",
                migration_kind_label(migration.kind),
                migration.identity.version,
                first_pending.map(|candidate| candidate.identity.version.as_str())
            )));
        }
    } else {
        let latest = snapshot
            .applied
            .iter()
            .filter(|applied| applied.kind == "versioned")
            .max_by_key(|applied| applied.apply_order)
            .ok_or_else(|| {
                DbmError::Diverged("no applied migration remains to roll back".to_owned())
            })?;
        if latest.kind != migration_kind_label(migration.kind)
            || latest.version != migration.identity.version
        {
            return Err(DbmError::Safety(format!(
                "latest applied migration changed during rollback; expected {}, found {}; retry after checking status",
                migration.identity.version, latest.version
            )));
        }
    }
    Ok(())
}

pub async fn up_async(config: &LoadedConfig) -> Result<ExecutionReport> {
    execute_with_adapter(config, "up", 0, None).await
}

pub async fn down_async(config: &LoadedConfig, steps: usize) -> Result<ExecutionReport> {
    execute_with_adapter(config, "down", steps, None).await
}

/// Plans filesystem and compiled migrations together without executing SQL.
pub async fn plan_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<PlanReport> {
    let migrations = discover_on_worker_with_registry(config, Some(registry)).await?;
    let (snapshot, info) = read_adapter_snapshot_with_info(config).await?;
    registry.validate_database_info(&info)?;
    let mut report = plan_from_snapshot(config, &migrations, &snapshot)?;
    if config.config.target.driver == "postgres" {
        report.warnings.push(
            "PostgreSQL planning is read-only and does not initialize metadata; up/down writes require metadata format 5 and a non-production target"
                .to_owned(),
        );
    }
    if config.config.target.driver == "mysql" {
        report.warnings.push(
            "MySQL/MariaDB planning is read-only; initial migration execution requires explicit transaction=never and metadata format 5"
                .to_owned(),
        );
    }
    Ok(report)
}

pub(crate) async fn runner_preflight(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
    action: RunnerAction,
) -> Result<RunnerPreflight> {
    let migrations = discover_on_worker_with_registry(config, Some(registry)).await?;
    if action == RunnerAction::Validate {
        let snapshot = MetadataSnapshot::default();
        let mut report = plan_from_snapshot(config, &migrations, &snapshot)?;
        report.plan_hash = runner_operation_hash(&report.plan_hash, "validate", 0);
        return Ok(RunnerPreflight {
            manifest_fingerprint: manifest_hash(&migrations),
            plan_hash: report.plan_hash.clone(),
            target_fingerprint: configured_target_fingerprint(config),
            report,
        });
    }
    let (snapshot, info) = read_adapter_snapshot_with_info(config).await?;
    registry.validate_database_info(&info)?;
    let mut report = plan_from_snapshot(config, &migrations, &snapshot)?;

    let (direction, steps) = match action {
        RunnerAction::Validate => ("validate", 0),
        RunnerAction::Plan => ("plan", 0),
        RunnerAction::Status => ("status", 0),
        RunnerAction::History => ("history", 0),
        RunnerAction::Doctor => ("doctor", 0),
        RunnerAction::Up => ("up", 0),
        RunnerAction::Down { steps } => {
            if steps == 0 {
                return Err(DbmError::Configuration(
                    "down steps must be greater than zero".to_owned(),
                ));
            }
            let entries = build_status_entries(&migrations, &snapshot)?;
            reject_invalid_status(&entries)?;
            let mut applied: Vec<AppliedMigration> = snapshot
                .applied
                .iter()
                .filter(|migration| migration.kind == "versioned")
                .cloned()
                .collect();
            applied.sort_by_key(|migration| Reverse(migration.apply_order));
            if applied.is_empty() {
                report.operations.clear();
                report.status = "no_changes";
                ("down", steps)
            } else if steps > applied.len() {
                return Err(DbmError::Configuration(format!(
                    "requested {steps} rollback step(s), but only {} migration(s) are applied",
                    applied.len()
                )));
            } else {
                let by_version: BTreeMap<&str, &Migration> = migrations
                    .iter()
                    .filter(|migration| {
                        migration.kind == crate::migration::MigrationKind::Versioned
                    })
                    .map(|migration| (migration.identity.version.as_str(), migration))
                    .collect();
                let mut operations = Vec::with_capacity(steps);
                for applied_migration in applied.iter().take(steps) {
                    let migration = by_version
                        .get(applied_migration.version.as_str())
                        .copied()
                        .ok_or_else(|| {
                            DbmError::Diverged(format!(
                                "source for applied migration {} is missing",
                                applied_migration.version
                            ))
                        })?;
                    if migration.down_sql.is_none() {
                        return Err(DbmError::Safety(format!(
                            "migration {} is irreversible because it has no down section",
                            migration.identity.version
                        )));
                    }
                    operations.push(PlanOperation {
                        migration_id: migration.identity.version.clone(),
                        kind: migration.kind,
                        filename: migration.filename.clone(),
                        direction: "down",
                        transaction: match migration.transaction {
                            TransactionPolicy::Auto | TransactionPolicy::Always => "supported",
                            TransactionPolicy::Never => "unsupported",
                        },
                        risk_level: "unknown",
                        capability_status: "supported",
                    });
                }
                report.operations = operations;
                report.status = "ready";
                ("down", steps)
            }
        }
    };

    if !matches!(action, RunnerAction::Plan) {
        report.plan_hash = runner_operation_hash(&report.plan_hash, direction, steps);
    }
    Ok(RunnerPreflight {
        manifest_fingerprint: manifest_hash(&migrations),
        plan_hash: report.plan_hash.clone(),
        target_fingerprint: target_fingerprint(config, &info),
        report,
    })
}

fn runner_operation_hash(base_hash: &str, direction: &str, steps: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-runner-operation-v1\0");
    let step_count = steps.to_string();
    for value in [base_hash, direction, step_count.as_str()] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn target_fingerprint(config: &LoadedConfig, info: &database::DatabaseInfo) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-target-identity-v1\0");
    for value in [
        config.target_description(),
        info.dialect.to_owned(),
        info.server_version.clone(),
        info.target_identity.clone().unwrap_or_default(),
        info.database.clone().unwrap_or_default(),
        info.schema.clone().unwrap_or_default(),
        config.config.namespace.clone(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn configured_target_fingerprint(config: &LoadedConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-configured-target-v1\0");
    for value in [
        config.config.target.driver.clone(),
        config.target_description(),
        config.config.namespace.clone(),
        match config.config.target.environment {
            Environment::Development => "development",
            Environment::Test => "test",
            Environment::Production => "production",
        }
        .to_owned(),
        config.config.target.url_env.clone().unwrap_or_default(),
        config
            .config
            .target
            .tls_ca_cert_env
            .clone()
            .unwrap_or_default(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

/// Executes the complete filesystem/registry plan through the shared engine.
/// Applications must call this explicitly; loading the library never migrates.
pub async fn up_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<ExecutionReport> {
    execute_with_adapter(config, "up", 0, Some(registry)).await
}

/// Rolls back the complete selected filesystem/registry plan through the shared engine.
pub async fn down_with_registry_async(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
    steps: usize,
) -> Result<ExecutionReport> {
    execute_with_adapter(config, "down", steps, Some(registry)).await
}

fn plan_from_snapshot(
    config: &LoadedConfig,
    migrations: &[Migration],
    snapshot: &MetadataSnapshot,
) -> Result<PlanReport> {
    let entries = build_status_entries(migrations, snapshot)?;
    reject_invalid_status(&entries)?;
    let operations: Vec<PlanOperation> = migrations
        .iter()
        .filter(|migration| {
            entries.iter().any(|entry| {
                entry.migration_id == migration.identity.version
                    && entry.kind == migration_kind_label(migration.kind)
                    && entry.state == "pending"
            })
        })
        .map(|migration| PlanOperation {
            migration_id: migration.identity.version.clone(),
            kind: migration.kind,
            filename: migration.filename.clone(),
            direction: if migration.kind == crate::migration::MigrationKind::Repeatable {
                "repeat"
            } else {
                "up"
            },
            transaction: match migration.transaction {
                TransactionPolicy::Auto | TransactionPolicy::Always => "supported",
                TransactionPolicy::Never => "unsupported",
            },
            risk_level: "unknown",
            capability_status: if config.config.target.driver == "postgres" {
                "unsupported"
            } else {
                "supported"
            },
        })
        .collect();

    let plan_hash = plan_hash(config, migrations, snapshot);
    Ok(PlanReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: "plan",
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: if operations.is_empty() {
            "no_changes"
        } else {
            "ready"
        },
        plan_hash,
        read_only: true,
        operations,
        warnings: Vec::new(),
        errors: Vec::new(),
    })
}

fn build_status_entries(
    migrations: &[Migration],
    snapshot: &MetadataSnapshot,
) -> Result<Vec<StatusEntry>> {
    let mut applied_by_version = BTreeMap::<(String, String), AppliedMigration>::new();
    for applied in &snapshot.applied {
        if applied_by_version
            .insert(
                (applied.kind.clone(), applied.version.clone()),
                applied.clone(),
            )
            .is_some()
        {
            return Err(DbmError::Diverged(format!(
                "metadata contains duplicate {} migration identity {}",
                applied.kind, applied.version
            )));
        }
    }

    let max_applied_version = applied_by_version
        .keys()
        .filter(|(kind, _)| kind == "versioned")
        .map(|(_, version)| version.clone())
        .max();
    let mut entries = Vec::new();
    for migration in migrations {
        let version = migration.identity.version.clone();
        let kind = migration_kind_label(migration.kind).to_owned();
        match applied_by_version.remove(&(kind.clone(), version.clone())) {
            Some(applied) => {
                let reason = applied_mismatch(migration, &applied);
                let checksum_changed = migration.kind
                    == crate::migration::MigrationKind::Repeatable
                    && applied.checksum != migration.checksum;
                entries.push(StatusEntry {
                    migration_id: version,
                    kind,
                    filename: migration.filename.clone(),
                    state: if checksum_changed {
                        "pending".to_owned()
                    } else if reason.is_some() {
                        "diverged".to_owned()
                    } else {
                        "applied".to_owned()
                    },
                    reason: if checksum_changed {
                        Some("repeatable content checksum changed; it will be reapplied".to_owned())
                    } else {
                        reason
                    },
                });
            }
            None => {
                let out_of_order = migration.kind == crate::migration::MigrationKind::Versioned
                    && max_applied_version
                        .as_ref()
                        .is_some_and(|max| version < *max);
                entries.push(StatusEntry {
                    migration_id: version,
                    kind,
                    filename: migration.filename.clone(),
                    state: if out_of_order {
                        "blocked".to_owned()
                    } else {
                        "pending".to_owned()
                    },
                    reason: out_of_order.then(|| {
                        "pending migration is older than an already-applied migration".to_owned()
                    }),
                });
            }
        }
    }

    for ((kind, version), applied) in applied_by_version {
        entries.push(StatusEntry {
            migration_id: version,
            kind,
            filename: applied.filename,
            state: "diverged".to_owned(),
            reason: Some("applied migration source file is missing".to_owned()),
        });
    }

    for legacy in &snapshot.legacy_applied {
        entries.push(StatusEntry {
            migration_id: format!("legacy:{}:{}", legacy.origin_tool, legacy.original_version),
            kind: "legacy".to_owned(),
            filename: legacy
                .filename
                .clone()
                .unwrap_or_else(|| format!("{}:{}", legacy.origin_tool, legacy.original_version)),
            state: if legacy.verification_status == "adopted" {
                "legacy_adopted".to_owned()
            } else {
                "legacy_unadopted".to_owned()
            },
            reason: (legacy.verification_status != "adopted").then(|| {
                "legacy state is imported but not adopted; rollback remains blocked".to_owned()
            }),
        });
    }

    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.migration_id.cmp(&right.migration_id))
    });
    Ok(entries)
}

pub(crate) fn applied_mismatch(
    migration: &Migration,
    applied: &AppliedMigration,
) -> Option<String> {
    if applied.kind != migration_kind_label(migration.kind) {
        return Some("stored migration kind does not match its descriptor".to_owned());
    }
    let expected_key = migration.migration_key();
    if applied.migration_key != expected_key {
        return Some("stored migration identity key does not match its descriptor".to_owned());
    }
    if migration.kind == crate::migration::MigrationKind::Versioned
        && applied.filename != migration.filename
    {
        return Some("filename changed after application".to_owned());
    }
    if migration.kind == crate::migration::MigrationKind::Versioned
        && applied.source_path != migration.source_path
    {
        return Some("source path changed after application".to_owned());
    }
    if migration.kind == crate::migration::MigrationKind::Versioned
        && applied.description != migration.description
    {
        return Some("description changed after application".to_owned());
    }
    if migration.kind == crate::migration::MigrationKind::Versioned
        && applied.checksum != migration.checksum
    {
        return Some("content checksum changed after application".to_owned());
    }
    None
}

fn migration_kind_label(kind: crate::migration::MigrationKind) -> &'static str {
    match kind {
        crate::migration::MigrationKind::Versioned => "versioned",
        crate::migration::MigrationKind::Repeatable => "repeatable",
    }
}

fn reject_invalid_status(entries: &[StatusEntry]) -> Result<()> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.state == "diverged" || entry.state == "blocked")
    {
        return Err(DbmError::Diverged(format!(
            "migration {} is {}: {}",
            entry.migration_id,
            entry.state,
            entry.reason.as_deref().unwrap_or("unspecified mismatch")
        )));
    }
    Ok(())
}

fn overall_status(entries: &[StatusEntry]) -> String {
    if entries.iter().any(|entry| entry.state == "diverged") {
        return "diverged".to_owned();
    }
    if entries.iter().any(|entry| entry.state == "blocked") {
        return "blocked".to_owned();
    }
    if entries.iter().any(|entry| entry.state == "pending") {
        return "pending".to_owned();
    }
    if entries
        .iter()
        .any(|entry| entry.state == "legacy_unadopted")
    {
        return "legacy_adoption_required".to_owned();
    }
    if entries.iter().any(|entry| entry.state == "legacy_adopted") {
        return "legacy_adopted".to_owned();
    }
    "up_to_date".to_owned()
}

#[derive(Clone)]
struct StepAttempt {
    step_index: i64,
    step_key: String,
    statement_hash: String,
    statement_kind: String,
    transaction_class: &'static str,
    state: String,
    started_at: String,
    acknowledged_at: Option<String>,
    error_code: Option<String>,
    evidence: Option<String>,
}

/// A journal-aware execution context exposed to trusted Rust migrations.
///
/// Statements can only be executed through this context and must appear in the
/// migration's previously hashed plan in order. This is not a sandbox: Rust
/// code still has the process's ordinary filesystem and network permissions.
pub struct MigrationContext<'a> {
    session: &'a mut dyn database::DatabaseSession,
    migration: &'a Migration,
    run_id: &'a str,
    plan_hash: &'a str,
    direction: &'a str,
    expected_statements: Vec<String>,
    next_statement: usize,
    steps: &'a mut Vec<StepAttempt>,
}

impl MigrationContext<'_> {
    /// Executes the next declared SQL statement and records its journal step.
    pub async fn execute_next(&mut self) -> Result<()> {
        let statement = self
            .expected_statements
            .get(self.next_statement)
            .cloned()
            .ok_or_else(|| {
                DbmError::Safety(
                    "Rust migration attempted to execute beyond its declared plan".to_owned(),
                )
            })?;
        self.execute(&statement).await
    }

    /// Executes a SQL statement only when it exactly matches the next hashed plan step.
    pub async fn execute(&mut self, statement: &str) -> Result<()> {
        let parsed = split_sql_statements_for_dialect(statement, self.migration.dialect);
        if parsed.len() != 1 {
            return Err(DbmError::InvalidMigration {
                path: self.migration.source_path.clone(),
                message: "Rust migration context accepts exactly one SQL statement per step"
                    .to_owned(),
            });
        }
        let expected = self
            .expected_statements
            .get(self.next_statement)
            .cloned()
            .ok_or_else(|| {
                DbmError::Safety(
                    "Rust migration attempted to execute beyond its declared plan".to_owned(),
                )
            })?;
        if normalize_plan_statement(parsed[0]) != normalize_plan_statement(&expected) {
            return Err(DbmError::Safety(format!(
                "Rust migration statement {} does not match its hashed plan",
                self.next_statement + 1
            )));
        }

        let step_index = i64::try_from(self.next_statement).map_err(|_| {
            DbmError::Execution(
                "migration contains more steps than the journal supports".to_owned(),
            )
        })?;
        let statement = expected.as_str();
        let statement_hash = sql_hash(statement);
        let mut attempt = StepAttempt {
            step_index,
            step_key: step_key(self.plan_hash, self.direction, step_index, &statement_hash),
            statement_hash,
            statement_kind: statement_kind(statement),
            transaction_class: "transactional",
            state: "intent_recorded".to_owned(),
            started_at: utc_timestamp()?,
            acknowledged_at: None,
            error_code: None,
            evidence: None,
        };
        self.session
            .insert_step(StepData {
                run_id: self.run_id,
                step_index,
                step_key: &attempt.step_key,
                direction: self.direction,
                statement_hash: &attempt.statement_hash,
                statement_kind: &attempt.statement_kind,
                transaction_class: attempt.transaction_class,
                state: "intent_recorded",
                started_at: &attempt.started_at,
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            })
            .await?;
        let step_position = self.steps.len();
        self.steps.push(attempt.clone());

        if self.migration.dialect == crate::migration::SqlDialect::Postgres
            && let Err(error) = crate::migration::validate_postgres_migration_statement(statement)
        {
            attempt.state = "failed".to_owned();
            attempt.error_code = Some("MIGRATION_STATEMENT_REJECTED".to_owned());
            attempt.evidence = Some("runner rejected the statement before sending it".to_owned());
            self.steps[step_position] = attempt;
            return Err(error);
        }
        if let Err(error) = self.session.execute_statement(statement).await {
            if matches!(error, DbmError::ReconciliationRequired(_)) {
                return Err(error);
            }
            attempt.state = "failed".to_owned();
            attempt.error_code = Some("MIGRATION_STATEMENT_FAILED".to_owned());
            attempt.evidence = Some(
                "driver rejected the statement; managed transaction will roll back".to_owned(),
            );
            self.steps[step_position] = attempt;
            return Err(error);
        }

        let acknowledged_at = match utc_timestamp() {
            Ok(timestamp) => timestamp,
            Err(error) => {
                attempt.state = "failed".to_owned();
                attempt.error_code = Some("STEP_ACKNOWLEDGEMENT_FAILED".to_owned());
                attempt.evidence = Some(
                    "statement ran in the managed transaction but no acknowledgement time was available; transaction will roll back"
                        .to_owned(),
                );
                self.steps[step_position] = attempt;
                return Err(error);
            }
        };
        if let Err(error) = self
            .session
            .acknowledge_step(self.run_id, step_index, &acknowledged_at)
            .await
        {
            if matches!(error, DbmError::ReconciliationRequired(_)) {
                return Err(error);
            }
            attempt.state = "failed".to_owned();
            attempt.error_code = Some("STEP_ACKNOWLEDGEMENT_FAILED".to_owned());
            attempt.evidence = Some(
                "statement ran in the managed transaction but journal acknowledgement failed; transaction will roll back"
                    .to_owned(),
            );
            self.steps[step_position] = attempt;
            return Err(error);
        }
        attempt.state = "acknowledged".to_owned();
        attempt.acknowledged_at = Some(acknowledged_at);
        self.steps[step_position] = attempt;
        self.next_statement += 1;
        Ok(())
    }

    /// Executes every remaining planned step in order.
    pub async fn execute_all(&mut self) -> Result<()> {
        while self.next_statement < self.expected_statements.len() {
            self.execute_next().await?;
        }
        Ok(())
    }

    fn ensure_plan_consumed(&self) -> Result<()> {
        if self.next_statement != self.expected_statements.len() {
            return Err(DbmError::Execution(format!(
                "Rust migration completed after {} of {} declared step(s)",
                self.next_statement,
                self.expected_statements.len()
            )));
        }
        Ok(())
    }
}

fn normalize_plan_statement(statement: &str) -> &str {
    let trimmed = statement.trim();
    trimmed
        .strip_suffix(';')
        .map(str::trim_end)
        .unwrap_or(trimmed)
}

impl<'a> MigrationContext<'a> {
    fn new(
        session: &'a mut dyn database::DatabaseSession,
        migration: &'a Migration,
        run_id: &'a str,
        plan_hash: &'a str,
        direction: &'a str,
        expected_statements: Vec<String>,
        steps: &'a mut Vec<StepAttempt>,
    ) -> Self {
        Self {
            session,
            migration,
            run_id,
            plan_hash,
            direction,
            expected_statements,
            next_statement: 0,
            steps,
        }
    }
}

#[derive(Clone, Copy)]
struct MigrationOperation<'a> {
    migration: &'a Migration,
    initial_manifest: &'a str,
    batch_id: &'a str,
    plan_hash: &'a str,
    direction: &'a str,
}

struct AdapterMigrationRun<'a> {
    run_id: &'a str,
    owner_id: &'a str,
    started: Instant,
    steps: &'a mut Vec<StepAttempt>,
}

struct StepExecution<'a> {
    migration: &'a Migration,
    plan_hash: &'a str,
    run_id: &'a str,
    started: Instant,
    direction: &'a str,
    sql: &'a str,
}

fn run_migration_with_journal(
    config: &LoadedConfig,
    connection: &mut Connection,
    operation: MigrationOperation<'_>,
) -> Result<bool> {
    let migration = operation.migration;
    let run_id = Uuid::new_v4().to_string();
    let owner_id = Uuid::new_v4().to_string();
    let created_at = utc_timestamp()?;
    let preparation = begin_immediate(connection)?;
    ensure_schema(&preparation, &created_at)?;
    ensure_no_unresolved_runs(&preparation)?;
    acquire_lock(
        &preparation,
        LockData {
            owner_id: &owner_id,
            run_id: &run_id,
            now: &created_at,
        },
    )?;
    insert_planned_run(
        &preparation,
        migration,
        PlannedRunData {
            run_id: &run_id,
            batch_id: operation.batch_id,
            direction: operation.direction,
            created_at: &created_at,
            plan_hash: operation.plan_hash,
        },
    )?;
    preparation.commit().map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "creation of run {run_id} and its diagnostic lock has an uncertain commit result: {error}"
        ))
    })?;

    let started_at = utc_timestamp()?;
    let start_transaction = begin_immediate(connection).map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} was planned but could not acquire the SQLite writer lock to start: {error}"
        ))
    })?;
    mark_run_started(&start_transaction, &run_id, &started_at).map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "run {run_id} could not be durably marked started: {error}"
        ))
    })?;
    start_transaction.commit().map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "starting run {run_id} has an uncertain commit result: {error}"
        ))
    })?;
    let started = Instant::now();

    let result = if operation.direction != "down" {
        apply_one(config, connection, operation, &run_id, &owner_id, started)
    } else {
        revert_one(config, connection, operation, &run_id, &owner_id, started)
    };

    if matches!(&result, Err(DbmError::ReconciliationRequired(_))) {
        return result;
    }
    if let Err(error) = &result {
        let snapshot = read_snapshot_from_connection(connection, &config.config.namespace)?;
        let run_state = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.state.as_str());
        if run_state == Some("reconciled") {
            return result;
        }
        if !matches!(run_state, Some("succeeded" | "failed" | "reconciled")) {
            return Err(DbmError::ReconciliationRequired(format!(
                "run {run_id} remains unresolved after {error}; inspect with doctor before retrying"
            )));
        }
    }
    let release_result = release_owned_lock(connection, &owner_id);
    match (result, release_result) {
        (Ok(applied), Ok(())) => Ok(applied),
        (Err(error), Ok(())) => Err(error),
        (_, Err(error)) => Err(DbmError::ReconciliationRequired(format!(
            "run {run_id} finished but its diagnostic lock could not be confirmed released: {error}"
        ))),
    }
}

fn apply_one(
    config: &LoadedConfig,
    connection: &mut Connection,
    planned: MigrationOperation<'_>,
    run_id: &str,
    owner_id: &str,
    started: Instant,
) -> Result<bool> {
    let expected = planned.migration;
    let transaction = begin_immediate(connection)?;
    let mut steps = Vec::new();
    let execution = (|| {
        ensure_run_lock_ownership(&transaction, run_id, owner_id)?;
        let current_migrations = discover(config)?;
        if manifest_hash(&current_migrations) != planned.initial_manifest {
            return Err(DbmError::Safety(
                "migration sources changed after planning; rerun plan before applying".to_owned(),
            ));
        }

        let snapshot = read_snapshot_from_connection(&transaction, &config.config.namespace)?;
        let entries = build_status_entries(&current_migrations, &snapshot)?;
        reject_invalid_status(&entries)?;
        if let Some(applied) = snapshot.applied.iter().find(|applied| {
            applied.kind == migration_kind_label(expected.kind)
                && applied.version == expected.identity.version
        }) {
            let source = current_migrations
                .iter()
                .find(|migration| migration.migration_key() == expected.migration_key())
                .ok_or_else(|| {
                    DbmError::Diverged(format!(
                        "migration source {} disappeared during execution",
                        expected.identity.version
                    ))
                })?;
            if expected.kind == crate::migration::MigrationKind::Repeatable
                && applied.checksum != source.checksum
            {
                // A changed repeatable is the intended rerun condition.
            } else {
                if let Some(reason) = applied_mismatch(source, applied) {
                    return Err(DbmError::Diverged(format!(
                        "migration {} changed while another executor applied it: {reason}",
                        expected.identity.version
                    )));
                }
                return Err(DbmError::Safety(format!(
                    "migration {} became current after planning; rerun plan before retrying",
                    expected.identity.version
                )));
            }
        }

        let first_pending = current_migrations.iter().find(|candidate| {
            entries.iter().any(|entry| {
                entry.kind == migration_kind_label(candidate.kind)
                    && entry.migration_id == candidate.identity.version
                    && entry.state == "pending"
            })
        });
        if first_pending
            .is_none_or(|candidate| candidate.migration_key() != expected.migration_key())
        {
            return Err(DbmError::Safety(format!(
                "execution order changed after planning; expected {} {}, found {:?}; rerun plan",
                migration_kind_label(expected.kind),
                expected.identity.version,
                first_pending.map(|migration| migration.identity.version.as_str())
            )));
        }

        let migration = current_migrations
            .iter()
            .find(|migration| migration.migration_key() == expected.migration_key())
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "migration source {} disappeared during execution",
                    expected.identity.version
                ))
            })?;
        execute_one_in_transaction(
            &transaction,
            StepExecution {
                migration,
                plan_hash: planned.plan_hash,
                run_id,
                started,
                direction: "up",
                sql: &migration.up_sql,
            },
            &mut steps,
        )?;
        Ok(true)
    })();

    match execution {
        Ok(true) => transaction.commit().map(|()| true).map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "commit result is uncertain for migration {}; inspect with doctor before retrying: {error}",
                expected.identity.version
            ))
        }),
        Ok(false) => {
            transaction.rollback().map_err(|error| {
                DbmError::ReconciliationRequired(format!(
                    "could not close the concurrent run for migration {}: {error}",
                    expected.identity.version
                ))
            })?;
            persist_failed_run(
                connection,
                run_id,
                started,
                planned.direction,
                "CONCURRENTLY_APPLIED",
                "another executor applied this migration before the transaction began",
                &steps,
            )?;
            Ok(false)
        }
        Err(error) => {
            transaction.rollback().map_err(|rollback_error| {
                DbmError::ReconciliationRequired(format!(
                    "migration {} failed and rollback could not be confirmed: {rollback_error}",
                    expected.identity.version
                ))
            })?;
            persist_failed_run(
                connection,
                run_id,
                started,
                planned.direction,
                "MIGRATION_FAILED",
                "managed SQLite transaction rolled back; no migration state was committed",
                &steps,
            )?;
            Err(error)
        }
    }
}

fn revert_one(
    config: &LoadedConfig,
    connection: &mut Connection,
    planned: MigrationOperation<'_>,
    run_id: &str,
    owner_id: &str,
    started: Instant,
) -> Result<bool> {
    let expected = planned.migration;
    let transaction = begin_immediate(connection)?;
    let mut steps = Vec::new();
    let execution = (|| {
        ensure_run_lock_ownership(&transaction, run_id, owner_id)?;
        let current_migrations = discover(config)?;
        if manifest_hash(&current_migrations) != planned.initial_manifest {
            return Err(DbmError::Safety(
                "migration sources changed after planning; rerun status before rollback".to_owned(),
            ));
        }

        let snapshot = read_snapshot_from_connection(&transaction, &config.config.namespace)?;
        let entries = build_status_entries(&current_migrations, &snapshot)?;
        reject_invalid_status(&entries)?;
        let latest = snapshot
            .applied
            .iter()
            .filter(|migration| migration.kind == "versioned")
            .max_by_key(|migration| migration.apply_order)
            .ok_or_else(|| {
                DbmError::Diverged("no applied migration remains to roll back".to_owned())
            })?;
        if latest.kind != "versioned" || latest.version != expected.identity.version {
            return Err(DbmError::Safety(format!(
                "latest applied migration changed during rollback; expected {}, found {}; retry after checking status",
                expected.identity.version, latest.version
            )));
        }

        let migration = current_migrations
            .iter()
            .find(|migration| migration.migration_key() == expected.migration_key())
            .ok_or_else(|| {
                DbmError::Diverged(format!(
                    "migration source {} disappeared during rollback",
                    expected.identity.version
                ))
            })?;
        let down_sql = migration.down_sql.as_deref().ok_or_else(|| {
            DbmError::Safety(format!(
                "migration {} is irreversible because it has no down section",
                migration.identity.version
            ))
        })?;
        execute_one_in_transaction(
            &transaction,
            StepExecution {
                migration,
                plan_hash: planned.plan_hash,
                run_id,
                started,
                direction: "down",
                sql: down_sql,
            },
            &mut steps,
        )?;
        Ok(true)
    })();

    match execution {
        Ok(true) => transaction.commit().map(|()| true).map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "commit result is uncertain for rollback of migration {}; inspect with doctor before retrying: {error}",
                expected.identity.version
            ))
        }),
        Ok(false) => transaction.rollback().map(|()| false).map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "could not close the concurrent rollback run for migration {}: {error}",
                expected.identity.version
            ))
        }),
        Err(error) => {
            transaction.rollback().map_err(|rollback_error| {
                DbmError::ReconciliationRequired(format!(
                    "rollback for migration {} failed and transaction rollback could not be confirmed: {rollback_error}",
                    expected.identity.version
                ))
            })?;
            persist_failed_run(
                connection,
                run_id,
                started,
                "down",
                "MIGRATION_FAILED",
                "managed SQLite transaction rolled back; migration remains applied",
                &steps,
            )?;
            Err(error)
        }
    }
}

fn execute_one_in_transaction(
    transaction: &Transaction<'_>,
    execution: StepExecution<'_>,
    steps: &mut Vec<StepAttempt>,
) -> Result<()> {
    let StepExecution {
        migration,
        plan_hash,
        run_id,
        started,
        direction,
        sql,
    } = execution;
    let statements = split_sql_statements_for_dialect(sql, migration.dialect);
    if statements.is_empty() {
        return Err(DbmError::Execution(format!(
            "migration {} {direction} contains no SQL statements",
            migration.identity.version
        )));
    }

    for (index, statement) in statements.iter().enumerate() {
        let step_index = i64::try_from(index).unwrap_or(i64::MAX);
        let statement_hash = sql_hash(statement);
        let step_key = step_key(plan_hash, direction, step_index, &statement_hash);
        let step_started_at = utc_timestamp()?;
        let mut attempt = StepAttempt {
            step_index,
            step_key,
            statement_hash,
            statement_kind: statement_kind(statement),
            transaction_class: "transactional",
            state: "intent_recorded".to_owned(),
            started_at: step_started_at.clone(),
            acknowledged_at: None,
            error_code: None,
            evidence: None,
        };
        insert_step(
            transaction,
            StepData {
                run_id,
                step_index,
                step_key: &attempt.step_key,
                direction,
                statement_hash: &attempt.statement_hash,
                statement_kind: &attempt.statement_kind,
                transaction_class: attempt.transaction_class,
                state: "intent_recorded",
                started_at: &attempt.started_at,
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            },
        )?;

        if let Err(error) = execute_guarded(transaction, statement) {
            attempt.state = "failed".to_owned();
            attempt.error_code = Some(sqlite_error_code(&error));
            attempt.evidence =
                Some("SQLite returned an error; managed transaction will roll back".to_owned());
            steps.push(attempt);
            return Err(DbmError::Execution(format!(
                "migration {} {direction} failed at SQL step {}: {error}",
                migration.identity.version,
                index + 1
            )));
        }

        let acknowledged_at = utc_timestamp()?;
        acknowledge_step(transaction, run_id, step_index, &acknowledged_at)?;
        attempt.state = "acknowledged".to_owned();
        attempt.acknowledged_at = Some(acknowledged_at);
        steps.push(attempt);
    }

    if direction != "down" {
        insert_applied(transaction, migration, &utc_timestamp()?)?;
    } else {
        delete_applied(transaction, migration)?;
    }

    let ended_at = utc_timestamp()?;
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    finish_run(
        transaction,
        FinishedRunData {
            run_id,
            state: "succeeded",
            ended_at: &ended_at,
            duration_ms,
            error_code: None,
            error_summary_redacted: None,
        },
    )?;
    Ok(())
}

fn persist_failed_run(
    connection: &mut Connection,
    run_id: &str,
    started: Instant,
    direction: &str,
    error_code: &str,
    error_summary: &str,
    steps: &[StepAttempt],
) -> Result<()> {
    let transaction = begin_immediate(connection)?;
    let ended_at = utc_timestamp()?;
    finish_run(
        &transaction,
        FinishedRunData {
            run_id,
            state: "failed",
            ended_at: &ended_at,
            duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
            error_code: Some(error_code),
            error_summary_redacted: Some(error_summary),
        },
    )?;
    for step in steps {
        let evidence = if step.state == "acknowledged" {
            Some("statement acknowledged inside a transaction that was rolled back")
        } else {
            step.evidence.as_deref()
        };
        insert_step(
            &transaction,
            StepData {
                run_id,
                step_index: step.step_index,
                step_key: &step.step_key,
                direction,
                statement_hash: &step.statement_hash,
                statement_kind: &step.statement_kind,
                transaction_class: step.transaction_class,
                state: &step.state,
                started_at: &step.started_at,
                acknowledged_at: step.acknowledged_at.as_deref(),
                error_code: step.error_code.as_deref(),
                evidence,
            },
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn release_owned_lock(connection: &mut Connection, owner_id: &str) -> Result<()> {
    let now = utc_timestamp()?;
    let transaction = begin_immediate(connection)?;
    release_lock(&transaction, owner_id, &now)?;
    match transaction.commit() {
        Ok(()) => Ok(()),
        Err(error) => {
            let state = connection
                .query_row(
                    "SELECT state FROM main.schema_migration_locks \
                     WHERE scope = 'database' AND owner_id = ?1",
                    [owner_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if state.as_deref() == Some("released") {
                return Ok(());
            }
            Err(DbmError::ReconciliationRequired(format!(
                "release of the diagnostic lock has an uncertain commit result; inspect with doctor before retrying: {error}"
            )))
        }
    }
}

fn sql_hash(sql: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-sql-step-v1\0");
    hasher.update(sql.as_bytes());
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn step_key(plan_hash: &str, direction: &str, index: i64, statement_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-step-key-v1\0");
    for value in [plan_hash, direction, statement_hash] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(index.to_be_bytes());
    hex_digest(hasher.finalize().as_slice())
}

fn statement_kind(sql: &str) -> String {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after_line_comment) = rest.strip_prefix("--") {
            rest = after_line_comment
                .find('\n')
                .map(|newline| &after_line_comment[newline + 1..])
                .unwrap_or_default()
                .trim_start();
            continue;
        }
        if let Some(after_block_start) = rest.strip_prefix("/*") {
            rest = after_block_start
                .find("*/")
                .map(|end| &after_block_start[end + 2..])
                .unwrap_or_default()
                .trim_start();
            continue;
        }
        break;
    }
    let keyword: String = rest
        .chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .take(32)
        .map(|character| character.to_ascii_uppercase())
        .collect();
    if keyword.is_empty() {
        "OPAQUE".to_owned()
    } else {
        keyword
    }
}

fn sqlite_error_code(error: &rusqlite::Error) -> String {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => {
            format!("SQLITE_{}", failure.extended_code)
        }
        _ => "SQLITE_EXECUTION_ERROR".to_owned(),
    }
}

fn execute_guarded(transaction: &Transaction<'_>, sql: &str) -> rusqlite::Result<()> {
    transaction.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
        if forbidden_action(context.action) {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))?;
    let result = transaction.execute_batch(sql);
    let clear_result = transaction.authorizer(Some(|_: rusqlite::hooks::AuthContext<'_>| {
        Authorization::Allow
    }));
    clear_result?;
    result
}

fn forbidden_action(action: AuthAction<'_>) -> bool {
    match action {
        AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Pragma { .. } => true,
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::CreateTable { table_name }
        | AuthAction::CreateTempTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::DropTempTable { table_name }
        | AuthAction::CreateIndex { table_name, .. }
        | AuthAction::CreateTempIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::DropTempIndex { table_name, .. }
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::CreateTempTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. }
        | AuthAction::DropTempTrigger { table_name, .. }
        | AuthAction::AlterTable { table_name, .. } => is_metadata_table(table_name),
        AuthAction::Update { table_name, .. } => is_metadata_table(table_name),
        _ => false,
    }
}

fn is_metadata_table(table_name: &str) -> bool {
    [
        "schema_migration_meta",
        "schema_migrations",
        "schema_migration_runs",
        "schema_migration_steps",
        "schema_migration_locks",
    ]
    .iter()
    .any(|name| table_name.eq_ignore_ascii_case(name))
}

fn begin_immediate(connection: &mut Connection) -> Result<Transaction<'_>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    DbmError::Lock(error.to_string())
                }
                _ => DbmError::Database(rusqlite::Error::SqliteFailure(failure, None)),
            },
            other => DbmError::Database(other),
        })
}

pub(crate) fn require_mutation_environment(config: &LoadedConfig) -> Result<()> {
    match config.config.target.environment {
        Environment::Development | Environment::Test => Ok(()),
        Environment::Production => Err(DbmError::Safety(
            "production targets are intentionally disabled in this increment".to_owned(),
        )),
    }
}

fn manifest_hash(migrations: &[Migration]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-source-manifest-v2\0");
    for migration in migrations {
        hasher.update(migration_kind_label(migration.kind).as_bytes());
        hasher.update([0]);
        for value in [
            migration.identity.namespace.as_str(),
            migration.identity.version.as_str(),
            migration.filename.as_str(),
            migration.source_path.as_str(),
            migration.checksum.as_str(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn plan_hash(
    config: &LoadedConfig,
    migrations: &[Migration],
    snapshot: &MetadataSnapshot,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-plan-v1\0");
    hasher.update(config.config.namespace.as_bytes());
    hasher.update(config.target_description().as_bytes());
    hasher.update(manifest_hash(migrations).as_bytes());
    let mut applied = snapshot.applied.clone();
    applied.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.version.cmp(&right.version))
    });
    for migration in applied {
        for value in [
            migration.kind.as_str(),
            migration.migration_key.as_str(),
            migration.version.as_str(),
            migration.filename.as_str(),
            migration.source_path.as_str(),
            migration.checksum.as_str(),
            migration.applied_at.as_str(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    let mut imports = snapshot.legacy_imports.clone();
    imports.sort_by(|left, right| {
        left.import_id
            .cmp(&right.import_id)
            .then_with(|| left.snapshot_digest.cmp(&right.snapshot_digest))
    });
    for import in imports {
        for value in [
            import.import_id.as_str(),
            import.origin_tool.as_str(),
            import.source_identity.as_str(),
            import.snapshot_digest.as_str(),
            import.plan_digest.as_str(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    let mut legacy_applied = snapshot.legacy_applied.clone();
    legacy_applied.sort_by(|left, right| {
        left.migration_key
            .cmp(&right.migration_key)
            .then_with(|| left.import_id.cmp(&right.import_id))
    });
    for migration in legacy_applied {
        for value in [
            migration.migration_key.as_str(),
            migration.import_id.as_str(),
            migration.origin_tool.as_str(),
            migration.source_identity.as_str(),
            migration.origin_identity.as_str(),
            migration.original_version.as_str(),
            migration.verification_status.as_str(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn execution_report(
    config: &LoadedConfig,
    command: &'static str,
    status: &str,
    batch_id: Option<String>,
    migrations: Vec<String>,
) -> ExecutionReport {
    ExecutionReport {
        schema_version: JSON_SCHEMA_VERSION,
        command,
        target: config.target_description(),
        namespace: config.config.namespace.clone(),
        status: status.to_owned(),
        batch_id,
        migrations,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        MigrationOperation, ReconcileReport, ReconcileRequest, build_doctor_steps, doctor,
        doctor_async, down, down_async, history, history_async, manifest_hash, plan, plan_async,
        plan_from_snapshot, plan_hash, reconcile as engine_reconcile, reconcile_async,
        run_adapter_migration, split_sql_statements_for_dialect, sql_hash, status, status_async,
        up, up_async, validate,
    };
    use crate::config::{self, LoadedConfig};
    use crate::database::{self, SessionIntent};
    use crate::error::DbmError;
    use crate::storage::read_snapshot;

    fn setup() -> (tempfile::TempDir, LoadedConfig) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("dbm.toml");
        config::initialize(&config_path).expect("initialize project");
        let loaded = config::load(&config_path).expect("load config");
        (directory, loaded)
    }

    fn write_migration(config: &LoadedConfig, name: &str, sql: &str) {
        fs::write(config.source_dirs[0].path.join(name), sql).expect("write migration");
    }

    async fn set_test_role_search_path(database_url: &str, schema: &str, enabled: bool) {
        let (client, connection) = tokio_postgres::connect(database_url, tokio_postgres::NoTls)
            .await
            .expect("connect to disposable PostgreSQL to set test role search path");
        let connection_task = tokio::spawn(connection);
        let statement = if enabled {
            client
                .query_one(
                    "SELECT format(\
                         'ALTER ROLE %I IN DATABASE %I SET search_path TO %I, public', \
                         current_user, current_database(), $1::text)",
                    &[&schema],
                )
                .await
                .expect("build safely quoted test role search-path statement")
                .get::<_, String>(0)
        } else {
            client
                .query_one(
                    "SELECT format(\
                         'ALTER ROLE %I IN DATABASE %I RESET search_path', \
                         current_user, current_database())",
                    &[],
                )
                .await
                .expect("build test role search-path reset statement")
                .get::<_, String>(0)
        };
        client
            .batch_execute(&statement)
            .await
            .expect("update disposable PostgreSQL test role search path");
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
    }

    fn repair_plan_json(
        config: &LoadedConfig,
        migration_version: &str,
        direction: &str,
        operation_result: &str,
    ) -> String {
        let migrations = crate::source::discover(config).expect("discover migration sources");
        let migration = migrations
            .iter()
            .find(|migration| migration.identity.version == migration_version)
            .expect("find migration for repair plan");
        let sql = match direction {
            "up" => migration.up_sql.as_str(),
            "down" => migration.down_sql.as_deref().expect("down SQL exists"),
            _ => panic!("test direction is supported"),
        };
        let state = match operation_result {
            "operation_applied" => "confirmed_applied",
            "operation_not_applied" => "confirmed_not_applied",
            _ => panic!("test operation result is supported"),
        };
        let steps: Vec<serde_json::Value> =
            split_sql_statements_for_dialect(sql, migration.dialect)
                .iter()
                .enumerate()
                .map(|(step_index, statement)| {
                    serde_json::json!({
                        "step_index": step_index,
                        "statement_hash": sql_hash(statement),
                        "state": state,
                        "inspection": "checked the transactional SQLite outcome",
                    })
                })
                .collect();
        serde_json::json!({
            "schema_version": "1.0",
            "summary": "reconcile the atomic SQLite migration outcome",
            "steps": steps,
        })
        .to_string()
    }

    fn reconcile_attempt(
        config: &LoadedConfig,
        run_id: &str,
        operation_result: &str,
        repair_plan_json: &str,
        approved: bool,
    ) -> crate::Result<ReconcileReport> {
        let evidence_ref = crate::identity::raw_checksum(b"synthetic operator evidence");
        engine_reconcile(
            config,
            ReconcileRequest {
                run_id,
                operation_result,
                evidence_ref: &evidence_ref,
                repair_plan_json,
                reason: "verified transactional state",
                actor: "operator",
                approved,
            },
        )
    }

    #[test]
    fn repeatables_are_audited_checksum_driven_and_excluded_from_down() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_create_versioned_probe.sql",
            "-- dbm:up\nCREATE TABLE versioned_probe (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE versioned_probe;\n",
        );
        let repeatable_path = config.source_dirs[0].path.join("R__refresh_probe.sql");
        fs::write(
            &repeatable_path,
            "CREATE TABLE IF NOT EXISTS repeatable_probe (value TEXT NOT NULL);\nDELETE FROM repeatable_probe;\nINSERT INTO repeatable_probe VALUES ('v1');\n",
        )
        .expect("write repeatable migration");

        let initial_plan = plan(&config).expect("plan repeatable");
        assert_eq!(initial_plan.operations.len(), 2);
        assert_eq!(
            initial_plan.operations[0].kind,
            crate::migration::MigrationKind::Versioned
        );
        assert_eq!(
            initial_plan.operations[1].kind,
            crate::migration::MigrationKind::Repeatable
        );
        assert_eq!(initial_plan.operations[1].migration_id, "refresh_probe");
        assert_eq!(initial_plan.operations[1].direction, "repeat");

        let first_apply = up(&config).expect("apply versioned and repeatable migrations");
        assert_eq!(
            first_apply.migrations,
            vec!["20261021162643981234", "refresh_probe",]
        );
        let first_history = history(&config).expect("read first history");
        assert_eq!(first_history.runs.len(), 2);
        assert_eq!(first_history.runs[0].direction, "up");
        assert_eq!(first_history.runs[1].direction, "repeat");

        let unchanged_apply = up(&config).expect("unchanged repeatable is not rerun");
        assert_eq!(unchanged_apply.status, "no_changes");
        assert_eq!(
            history(&config).expect("read unchanged history").runs.len(),
            2
        );

        fs::write(
            &repeatable_path,
            "CREATE TABLE IF NOT EXISTS repeatable_probe (value TEXT NOT NULL);\nDELETE FROM repeatable_probe;\nINSERT INTO repeatable_probe VALUES ('v2');\n",
        )
        .expect("change repeatable content");
        let changed_status = status(&config).expect("status detects repeatable checksum change");
        assert_eq!(changed_status.status, "pending");
        assert!(changed_status.migrations.iter().any(|entry| {
            entry.kind == "repeatable"
                && entry.migration_id == "refresh_probe"
                && entry.state == "pending"
        }));
        up(&config).expect("reapply changed repeatable");
        let changed_history = history(&config).expect("read changed history");
        assert_eq!(changed_history.runs.len(), 3);
        assert_eq!(changed_history.runs[2].direction, "repeat");

        let connection = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open test database");
        let stored_value: String = connection
            .query_row("SELECT value FROM repeatable_probe", [], |row| row.get(0))
            .expect("read repeatable result");
        assert_eq!(stored_value, "v2");

        let rollback = down(&config, 1).expect("rollback only the versioned migration");
        assert_eq!(rollback.migrations, vec!["20261021162643981234"]);
        let final_history = history(&config).expect("read final history");
        assert_eq!(final_history.runs.len(), 4);
        assert_eq!(final_history.runs[3].direction, "down");
        assert_eq!(final_history.runs[3].migration_id, "20261021162643981234");
    }

    #[test]
    fn plan_and_status_are_read_only_and_up_down_preserve_history() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_create_items.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE items;\n",
        );

        let planned = plan(&config).expect("plan succeeds");
        let observed = status(&config).expect("status succeeds");
        assert_eq!(planned.operations.len(), 1);
        assert!(planned.read_only);
        assert_eq!(observed.status, "pending");
        assert!(
            !config
                .sqlite_database_path()
                .expect("SQLite database path")
                .exists()
        );

        let applied = up(&config).expect("up succeeds");
        assert_eq!(applied.migrations, vec!["20261021162643981234"]);
        assert_eq!(
            status(&config).expect("status after up").status,
            "up_to_date"
        );
        assert_eq!(history(&config).expect("history after up").runs.len(), 1);

        let reverted = down(&config, 1).expect("down succeeds");
        assert_eq!(reverted.migrations, vec!["20261021162643981234"]);
        assert_eq!(history(&config).expect("history after down").runs.len(), 2);
        assert_eq!(
            status(&config).expect("status after down").status,
            "pending"
        );
    }

    #[tokio::test]
    async fn async_sqlite_adapter_read_paths_match_the_sync_engine_without_creating_metadata() {
        let (directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_create_items.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE items;\n",
        );
        let database_path = config
            .sqlite_database_path()
            .expect("SQLite database path")
            .to_path_buf();

        let sync_plan = plan(&config).expect("synchronous plan succeeds");
        let async_plan = plan_async(&config).await.expect("async plan succeeds");
        assert_eq!(async_plan.plan_hash, sync_plan.plan_hash);
        assert_eq!(async_plan.operations.len(), 1);
        assert!(!database_path.exists());

        let async_status = status_async(&config).await.expect("async status succeeds");
        assert_eq!(async_status.status, "pending");
        let async_history = history_async(&config)
            .await
            .expect("async history succeeds");
        assert!(async_history.runs.is_empty());
        let async_doctor = doctor_async(&config).await.expect("async doctor succeeds");
        assert_eq!(async_doctor.status, "not_initialized");
        assert!(!database_path.exists());

        let applied = up_async(&config).await.expect("async SQLite up succeeds");
        assert_eq!(applied.status, "succeeded");
        let applied_status = status_async(&config)
            .await
            .expect("async status reads applied metadata");
        assert_eq!(applied_status.status, "up_to_date");
        let applied_history = history_async(&config)
            .await
            .expect("async history reads applied run");
        assert_eq!(applied_history.runs.len(), 1);

        let reverted = down_async(&config, 1)
            .await
            .expect("async SQLite down succeeds");
        assert_eq!(reverted.status, "succeeded");
        let reverted_history = history_async(&config)
            .await
            .expect("async history preserves rollback event");
        assert_eq!(reverted_history.runs.len(), 2);
        assert!(directory.path().join("migrations/versioned").is_dir());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_adapter_journals_failed_transaction_and_releases_its_lock() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_partial_failure.sql",
            "-- dbm:up\nCREATE TABLE should_rollback (id INTEGER);\nINSERT INTO missing_table VALUES (1);\n",
        );

        assert!(matches!(
            up_async(&config).await,
            Err(DbmError::Execution(_))
        ));
        let snapshot = read_snapshot(&config).expect("read async failure journal");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "failed");
        assert_eq!(snapshot.steps.len(), 2);
        assert_eq!(snapshot.steps[0].state, "acknowledged");
        assert_eq!(snapshot.steps[1].state, "failed");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("released")
        );
        let connection = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open target after async failure");
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='should_rollback'",
                [],
                |row| row.get(0),
            )
            .expect("query rolled-back table");
        assert_eq!(table_count, 0);
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_adapter_runs_up_failure_and_down_through_shared_fsm() {
        use std::net::IpAddr;

        use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

        let (directory, sqlite_config) = setup();
        let database_url = std::env::var("DBM_TEST_DATABASE_URL")
            .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
        let parsed = database_url
            .parse::<PostgresConfig>()
            .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
        assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
        assert!(parsed.get_hosts().iter().all(|host| {
            match host {
                Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
                Host::Tcp(host) => host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()),
                Host::Unix(_) => true,
            }
        }));
        assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));

        let mut config = sqlite_config;
        config.config.target.driver = "postgres".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        config.config.target.allow_insecure_transport = true;
        config.config.target.environment = crate::config::Environment::Test;
        config.database_path = None;
        write_migration(
            &config,
            "20261024120000000000_create_probe.sql",
            "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE transactional_probe (id integer PRIMARY KEY);\n-- dbm:down\nDROP TABLE transactional_probe;\n",
        );
        write_migration(
            &config,
            "20261024120000000001_create_concurrent_index.sql",
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY transactional_probe_id_idx ON transactional_probe (id);\n-- dbm:down\nDROP INDEX CONCURRENTLY transactional_probe_id_idx;\n",
        );
        write_migration(
            &config,
            "20261024120000000002_failed_probe.sql",
            "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE failed_probe (id integer);\nSELECT 1 / 0;\n-- dbm:down\nDROP TABLE failed_probe;\n",
        );
        let migrations = crate::source::discover(&config).expect("discover PostgreSQL sources");
        let manifest = manifest_hash(&migrations);
        let adapter = database::adapter_for(&config).expect("resolve PostgreSQL adapter");
        let mut session = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect to disposable PostgreSQL");
        let schema = format!("dbm_fsm_test_{}", uuid::Uuid::new_v4().simple());
        session
            .begin()
            .await
            .expect("begin isolated PostgreSQL schema creation");
        session
            .execute_statement(&format!("CREATE SCHEMA {schema}"))
            .await
            .expect("create isolated PostgreSQL schema");
        session
            .commit()
            .await
            .expect("commit isolated PostgreSQL schema creation");
        session
            .begin()
            .await
            .expect("begin isolated PostgreSQL search-path setup");
        session
            .execute_statement(&format!("SET search_path TO {schema}"))
            .await
            .expect("scope migration session to isolated schema");
        session
            .commit()
            .await
            .expect("commit isolated PostgreSQL search-path setup");
        let initial_snapshot = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read initial PostgreSQL state");
        let initial_plan = plan_from_snapshot(&config, &migrations, &initial_snapshot)
            .expect("plan PostgreSQL migrations");
        let batch_id = uuid::Uuid::new_v4().to_string();

        run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[0],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &initial_plan.plan_hash,
                direction: "up",
            },
        )
        .await
        .expect("apply PostgreSQL migration through shared FSM");

        run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[1],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &initial_plan.plan_hash,
                direction: "up",
            },
        )
        .await
        .expect("apply concurrent-index migration outside a transaction");

        let failed = run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[2],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &initial_plan.plan_hash,
                direction: "up",
            },
        )
        .await
        .expect_err("PostgreSQL SQL failure rolls back transactionally");
        assert!(matches!(failed, DbmError::Execution(_)));
        session
            .begin()
            .await
            .expect("begin PostgreSQL rollback verification");
        session
            .execute_statement("CREATE TABLE failed_probe (id integer)")
            .await
            .expect("failed migration did not leave its first DDL applied");
        session
            .execute_statement("DROP TABLE failed_probe")
            .await
            .expect("remove rollback verification table");
        session
            .commit()
            .await
            .expect("commit PostgreSQL rollback verification");

        let after_failure = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read PostgreSQL failure journal");
        assert_eq!(after_failure.metadata_format_version, Some(4));
        assert_eq!(after_failure.applied.len(), 2);
        assert_eq!(after_failure.runs.len(), 3);
        assert_eq!(after_failure.runs[0].state, "succeeded");
        assert_eq!(after_failure.runs[1].state, "succeeded");
        assert_eq!(after_failure.runs[2].state, "failed");
        assert_eq!(after_failure.steps.len(), 4);
        assert_eq!(after_failure.steps[1].state, "acknowledged");
        assert_eq!(
            after_failure.steps[1].transaction_class,
            "non_transactional"
        );
        assert_eq!(after_failure.steps[2].state, "acknowledged");
        assert_eq!(after_failure.steps[3].state, "failed");
        assert_eq!(
            after_failure.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("released")
        );

        let rollback_plan_hash = plan_hash(&config, &migrations, &after_failure);
        run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[1],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &rollback_plan_hash,
                direction: "down",
            },
        )
        .await
        .expect("drop concurrent index outside a transaction");
        run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[0],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &rollback_plan_hash,
                direction: "down",
            },
        )
        .await
        .expect("roll back PostgreSQL table through shared FSM");
        let final_snapshot = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read final PostgreSQL state");
        assert!(final_snapshot.applied.is_empty());
        assert_eq!(final_snapshot.runs.len(), 5);
        assert_eq!(final_snapshot.runs[3].direction, "down");
        assert_eq!(final_snapshot.runs[3].state, "succeeded");
        assert_eq!(final_snapshot.runs[4].direction, "down");
        assert_eq!(final_snapshot.runs[4].state, "succeeded");

        session
            .close()
            .await
            .expect("close PostgreSQL migration session");
        let mut cleanup = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect for isolated schema cleanup");
        cleanup
            .begin()
            .await
            .expect("begin isolated PostgreSQL schema cleanup");
        cleanup
            .execute_statement(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .expect("drop isolated PostgreSQL schema");
        cleanup
            .commit()
            .await
            .expect("commit isolated PostgreSQL schema cleanup");
        cleanup
            .close()
            .await
            .expect("close PostgreSQL cleanup session");
        assert!(directory.path().join("migrations/versioned").is_dir());
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_session_loss_during_fsm_ddl_is_not_retried() {
        use std::net::IpAddr;

        use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

        let (_directory, sqlite_config) = setup();
        let database_url = std::env::var("DBM_TEST_DATABASE_URL")
            .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
        let parsed = database_url
            .parse::<PostgresConfig>()
            .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
        assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
        assert!(parsed.get_hosts().iter().all(|host| {
            match host {
                Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
                Host::Tcp(host) => host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()),
                Host::Unix(_) => true,
            }
        }));
        assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));

        let mut config = sqlite_config;
        config.config.target.driver = "postgres".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        config.config.target.allow_insecure_transport = true;
        config.config.target.environment = crate::config::Environment::Test;
        config.database_path = None;

        let schema = format!("dbm_fsm_loss_{}", uuid::Uuid::new_v4().simple());
        let table = format!("fsm_loss_probe_{}", uuid::Uuid::new_v4().simple());
        let event_trigger = format!("dbm_fsm_loss_{}", uuid::Uuid::new_v4().simple());
        let function = format!("pause_fsm_{}", uuid::Uuid::new_v4().simple());
        write_migration(
            &config,
            "20261024120000000000_session_loss_probe.sql",
            &format!(
                "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE {schema}.{table} (id integer PRIMARY KEY);\n-- dbm:down\nDROP TABLE {schema}.{table};\n"
            ),
        );
        let migrations = crate::source::discover(&config).expect("discover PostgreSQL source");
        let manifest = manifest_hash(&migrations);
        let adapter = database::adapter_for(&config).expect("resolve PostgreSQL adapter");

        let mut session = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect migration session to disposable PostgreSQL");
        session
            .begin()
            .await
            .expect("begin schema setup transaction");
        session
            .execute_statement(&format!("CREATE SCHEMA {schema}"))
            .await
            .expect("create isolated PostgreSQL schema");
        session.commit().await.expect("commit schema setup");
        session
            .begin()
            .await
            .expect("begin migration search-path setup");
        session
            .execute_statement(&format!("SET search_path TO {schema}, public"))
            .await
            .expect("set isolated migration search path");
        session
            .commit()
            .await
            .expect("commit migration search path");
        session
            .begin()
            .await
            .expect("begin event-trigger setup transaction");
        session
            .execute_statement(&format!(
                "CREATE FUNCTION {schema}.{function}() RETURNS event_trigger \
                 LANGUAGE plpgsql AS $$ BEGIN \
                 IF position('{table}' IN current_query()) > 0 THEN \
                     PERFORM pg_sleep(30); \
                 END IF; END $$"
            ))
            .await
            .expect("create a scoped DDL pause hook");
        session
            .execute_statement(&format!(
                "CREATE EVENT TRIGGER {event_trigger} ON ddl_command_start \
                 WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION {schema}.{function}()"
            ))
            .await
            .expect("install the scoped DDL pause hook");
        session.commit().await.expect("commit event-trigger setup");

        let backend_pid = session
            .info()
            .session_id
            .as_deref()
            .expect("pinned migration backend PID")
            .parse::<i32>()
            .expect("valid migration backend PID");
        let mut observer = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect observer session");
        observer
            .begin()
            .await
            .expect("begin observer search-path setup");
        observer
            .execute_statement(&format!("SET search_path TO {schema}, public"))
            .await
            .expect("set observer search path");
        observer
            .commit()
            .await
            .expect("commit observer search path");

        let watcher_url = database_url.clone();
        let watcher_table = table.clone();
        let watcher = tokio::spawn(async move {
            let (client, connection) = tokio_postgres::connect(&watcher_url, tokio_postgres::NoTls)
                .await
                .map_err(|_| "could not connect the PostgreSQL fault-injection observer")?;
            let connection_task = tokio::spawn(connection);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut terminated = false;
            while tokio::time::Instant::now() < deadline {
                let active: Option<i32> = client
                    .query_opt(
                        "SELECT pid FROM pg_stat_activity \
                         WHERE pid = $1 AND state = 'active' \
                           AND position($2 IN query) > 0",
                        &[&backend_pid, &watcher_table],
                    )
                    .await
                    .map_err(|_| "could not inspect PostgreSQL activity")?
                    .map(|row| row.get(0));
                if active.is_some() {
                    let did_terminate: bool = client
                        .query_one("SELECT pg_terminate_backend($1)", &[&backend_pid])
                        .await
                        .map_err(|_| "could not terminate the migration backend")?
                        .get(0);
                    terminated = did_terminate;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            connection_task.abort();
            if terminated {
                Ok::<(), &'static str>(())
            } else {
                Err("the migration DDL did not reach the fault-injection pause")
            }
        });

        let initial_snapshot = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read initial PostgreSQL metadata");
        let initial_plan = plan_from_snapshot(&config, &migrations, &initial_snapshot)
            .expect("plan session-loss migration");
        let batch_id = uuid::Uuid::new_v4().to_string();
        let execution = run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[0],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &initial_plan.plan_hash,
                direction: "up",
            },
        )
        .await;
        let watcher_result = watcher.await.expect("join PostgreSQL fault injector");
        let _ = session.close().await;

        let observed = observer
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read durable metadata after migration-session loss");
        observer
            .begin()
            .await
            .expect("begin fault-injection cleanup transaction");
        observer
            .execute_statement(&format!("DROP EVENT TRIGGER {event_trigger}"))
            .await
            .expect("remove scoped DDL pause hook");
        observer
            .execute_statement(&format!("DROP FUNCTION {schema}.{function}()"))
            .await
            .expect("remove scoped DDL pause function");
        observer
            .execute_statement(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .expect("remove isolated PostgreSQL schema");
        observer
            .commit()
            .await
            .expect("commit fault-injection cleanup");
        observer
            .close()
            .await
            .expect("close PostgreSQL fault-injection observer");

        assert!(
            watcher_result.is_ok(),
            "fault injector failed: {watcher_result:?}"
        );
        assert!(matches!(
            execution,
            Err(DbmError::ReconciliationRequired(_))
        ));
        assert_eq!(observed.applied.len(), 0);
        assert_eq!(observed.runs.len(), 1);
        assert_eq!(observed.runs[0].state, "started");
        assert!(
            observed.steps.is_empty(),
            "uncommitted transactional steps must roll back"
        );
        assert_eq!(
            observed.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("observed_active")
        );
    }

    #[tokio::test]
    #[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
    async fn local_postgres_nontransactional_failure_is_journaled_indeterminate() {
        use std::net::IpAddr;

        use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

        let (_directory, sqlite_config) = setup();
        let database_url = std::env::var("DBM_TEST_DATABASE_URL")
            .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
        let parsed = database_url
            .parse::<PostgresConfig>()
            .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
        assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
        assert!(parsed.get_hosts().iter().all(|host| {
            match host {
                Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
                Host::Tcp(host) => host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()),
                Host::Unix(_) => true,
            }
        }));
        assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));

        let mut config = sqlite_config;
        config.config.target.driver = "postgres".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        config.config.target.allow_insecure_transport = true;
        config.config.target.environment = crate::config::Environment::Test;
        config.database_path = None;
        write_migration(
            &config,
            "20261024120000000000_indeterminate_index.sql",
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY missing_relation_idx ON missing_relation (id);\n-- dbm:down\nDROP INDEX CONCURRENTLY missing_relation_idx;\n",
        );

        let migrations = crate::source::discover(&config).expect("discover PostgreSQL sources");
        let manifest = manifest_hash(&migrations);
        let adapter = database::adapter_for(&config).expect("resolve PostgreSQL adapter");
        let mut session = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect to disposable PostgreSQL");
        let schema = format!("dbm_indeterminate_test_{}", uuid::Uuid::new_v4().simple());
        session
            .begin()
            .await
            .expect("begin isolated PostgreSQL schema creation");
        session
            .execute_statement(&format!("CREATE SCHEMA {schema}"))
            .await
            .expect("create isolated PostgreSQL schema");
        session
            .commit()
            .await
            .expect("commit isolated PostgreSQL schema creation");
        session
            .begin()
            .await
            .expect("begin isolated PostgreSQL search-path setup");
        session
            .execute_statement(&format!("SET search_path TO {schema}"))
            .await
            .expect("set isolated PostgreSQL schema search path");
        session
            .commit()
            .await
            .expect("commit isolated PostgreSQL search-path setup");
        let initial_snapshot = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read initial PostgreSQL state");
        let initial_plan = plan_from_snapshot(&config, &migrations, &initial_snapshot)
            .expect("plan non-transactional PostgreSQL migration");
        let batch_id = uuid::Uuid::new_v4().to_string();
        let error = run_adapter_migration(
            &config,
            session.as_mut(),
            &migrations,
            None,
            MigrationOperation {
                migration: &migrations[0],
                initial_manifest: &manifest,
                batch_id: &batch_id,
                plan_hash: &initial_plan.plan_hash,
                direction: "up",
            },
        )
        .await
        .expect_err("failed concurrent DDL needs explicit reconciliation");
        assert!(matches!(error, DbmError::ReconciliationRequired(_)));

        let snapshot = session
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read indeterminate PostgreSQL journal");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "indeterminate");
        assert_eq!(snapshot.steps.len(), 1);
        assert_eq!(snapshot.steps[0].state, "indeterminate");
        assert_eq!(snapshot.steps[0].transaction_class, "non_transactional");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("observed_active")
        );
        let diagnosis = build_doctor_steps(
            &snapshot.runs[0],
            &migrations[0],
            &snapshot.steps,
            "known_not_applied",
        );
        assert_eq!(diagnosis[0].state, "indeterminate");
        assert_eq!(diagnosis[0].transaction_class, "non_transactional");
        let run_id = snapshot.runs[0].run_id.clone();

        session
            .close()
            .await
            .expect("close indeterminate migration session");
        set_test_role_search_path(&database_url, &schema, true).await;
        let doctor_report = doctor_async(&config)
            .await
            .expect("diagnose PostgreSQL non-transactional outcome");
        let indeterminate_finding = doctor_report
            .findings
            .iter()
            .find(|finding| {
                finding.run_id.as_deref() == Some(run_id.as_str())
                    && finding.code == "INDETERMINATE"
            })
            .expect("doctor finding for the interrupted PostgreSQL run");
        assert!(
            indeterminate_finding
                .message
                .contains("cannot be inferred from the applied-migration row")
        );
        let index_statement =
            split_sql_statements_for_dialect(&migrations[0].up_sql, migrations[0].dialect)[0];
        let repair_plan = serde_json::json!({
            "schema_version": "1.0",
            "summary": "the PostgreSQL catalog contains the valid index requested by the migration",
            "steps": [{
                "step_index": 0,
                "statement_hash": sql_hash(index_statement),
                "state": "confirmed_applied",
                "inspection": "pg_catalog confirms a valid btree index on the expected table and columns",
            }],
        })
        .to_string();
        let evidence_ref = crate::identity::raw_checksum(
            b"disposable PostgreSQL catalog inspection: requested index definition is present",
        );
        let conflict = reconcile_async(
            &config,
            ReconcileRequest {
                run_id: &run_id,
                operation_result: "operation_applied",
                evidence_ref: &evidence_ref,
                repair_plan_json: &repair_plan,
                reason: "verify catalog state conflicts are rejected before repair",
                actor: "integration-test",
                approved: true,
            },
        )
        .await
        .expect_err("catalog absence must reject a requested applied outcome");
        assert!(matches!(conflict, DbmError::Safety(_)));

        let mut catalog_setup = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect to prepare a matching index for reconciliation");
        catalog_setup
            .acquire_lock(std::time::Duration::from_secs(2))
            .await
            .expect("acquire the isolated PostgreSQL schema lock");
        catalog_setup
            .begin()
            .await
            .expect("begin matching table setup transaction");
        catalog_setup
            .execute_statement("CREATE TABLE missing_relation (id integer)")
            .await
            .expect("create the table required by the migration index");
        catalog_setup
            .commit()
            .await
            .expect("commit matching table setup");
        catalog_setup
            .execute_nontransactional_statement(index_statement)
            .await
            .expect("create the exact index definition for catalog reconciliation");
        catalog_setup
            .close()
            .await
            .expect("release the isolated PostgreSQL schema lock");
        let reconciled = reconcile_async(
            &config,
            ReconcileRequest {
                run_id: &run_id,
                operation_result: "operation_applied",
                evidence_ref: &evidence_ref,
                repair_plan_json: &repair_plan,
                reason: "verified the failed concurrent index is absent",
                actor: "integration-test",
                approved: true,
            },
        )
        .await
        .expect("reconcile the PostgreSQL catalog-proven absent index");
        assert_eq!(reconciled.status, "reconciled");
        let history = history_async(&config)
            .await
            .expect("read PostgreSQL reconciliation audit");
        assert_eq!(history.runs.len(), 2);
        assert_eq!(history.runs[0].state, "reconciled");
        assert_eq!(history.runs[1].direction, "reconcile");
        assert_eq!(history.runs[1].state, "succeeded");
        let mut verification = adapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect to verify reconciled applied migration");
        let final_snapshot = verification
            .read_snapshot(&config.config.namespace)
            .await
            .expect("read reconciled applied migration row");
        assert_eq!(final_snapshot.applied.len(), 1);
        assert_eq!(
            final_snapshot.applied[0].version,
            migrations[0].identity.version
        );
        verification
            .close()
            .await
            .expect("close reconciled migration verification session");
        set_test_role_search_path(&database_url, &schema, false).await;
        let mut cleanup = adapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect for isolated schema cleanup");
        cleanup
            .begin()
            .await
            .expect("begin isolated PostgreSQL schema cleanup");
        cleanup
            .execute_statement(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .expect("drop isolated PostgreSQL schema");
        cleanup
            .commit()
            .await
            .expect("commit isolated PostgreSQL schema cleanup");
        cleanup
            .close()
            .await
            .expect("close PostgreSQL cleanup session");
    }

    #[test]
    fn postgres_config_validates_sources_but_cannot_enter_sqlite_execution() {
        let (_directory, sqlite_config) = setup();
        write_migration(
            &sqlite_config,
            "20261021162643981234_create_pg_index.sql",
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);\n",
        );
        let mut postgres_config = sqlite_config.clone();
        postgres_config.config.target.driver = "postgres".to_owned();
        postgres_config.config.target.database = None;
        postgres_config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        postgres_config.database_path = None;

        assert_eq!(
            validate(&postgres_config)
                .expect("validate PostgreSQL sources")
                .status,
            "valid"
        );
        assert!(matches!(
            plan(&postgres_config),
            Err(DbmError::Unsupported(_))
        ));
        assert!(matches!(
            up(&postgres_config),
            Err(DbmError::Unsupported(_))
        ));
        assert!(postgres_config.database_path.is_none());
    }

    #[test]
    fn failed_migration_rolls_back_all_statements_and_state() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_partial_failure.sql",
            "-- dbm:up\nCREATE TABLE should_rollback (id INTEGER);\nINSERT INTO missing_table VALUES (1);\n",
        );

        let error = up(&config).expect_err("invalid SQL execution fails");
        assert!(matches!(error, DbmError::Execution(_)));

        let connection = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open target");
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='should_rollback'",
                [],
                |row| row.get(0),
            )
            .expect("query table");
        assert_eq!(table_count, 0);
        assert_eq!(
            status(&config).expect("status after failure").status,
            "pending"
        );
        let snapshot = read_snapshot(&config).expect("read failed run journal");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "failed");
        assert_eq!(snapshot.steps.len(), 2);
        assert_eq!(snapshot.steps[0].state, "acknowledged");
        assert_eq!(snapshot.steps[1].state, "failed");
        let diagnosis = doctor(&config).expect("doctor after failure");
        assert_eq!(diagnosis.status, "known_failures");
        assert_eq!(diagnosis.steps.len(), 2);
        assert!(
            diagnosis
                .steps
                .iter()
                .all(|step| step.state == "confirmed_not_applied")
        );
    }

    #[test]
    fn doctor_is_read_only_and_reconciliation_requires_proven_state_and_approval() {
        let (_directory, config) = setup();
        let untouched = doctor(&config).expect("doctor on missing target");
        assert_eq!(untouched.status, "not_initialized");
        assert!(untouched.read_only);
        assert!(
            !config
                .sqlite_database_path()
                .expect("SQLite database path")
                .exists()
        );

        write_migration(
            &config,
            "20261021162643981234_create_items.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\nINSERT INTO items VALUES (1);\n-- dbm:down\nDROP TABLE items;\n",
        );
        up(&config).expect("apply transactionally journaled migration");
        let snapshot = read_snapshot(&config).expect("read journal");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "succeeded");
        assert_eq!(snapshot.steps.len(), 2);
        assert!(
            snapshot
                .steps
                .iter()
                .all(|step| step.state == "acknowledged")
        );
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("released")
        );

        write_migration(
            &config,
            "20261021162743981234_add_name.sql",
            "-- dbm:up\nALTER TABLE items ADD COLUMN name TEXT;\n-- dbm:down\nALTER TABLE items DROP COLUMN name;\n",
        );
        let run_id = snapshot.runs[0].run_id.clone();
        let connection = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open target");
        connection
            .execute(
                "UPDATE schema_migration_runs SET state = 'started', ended_at = NULL, duration_ms = NULL WHERE run_id = ?1",
                [&run_id],
            )
            .expect("simulate process loss after start");
        connection
            .execute(
                "UPDATE schema_migration_locks SET state = 'observed_active', run_id = ?1 WHERE scope = 'database'",
                [&run_id],
            )
            .expect("simulate abandoned diagnostic lock");
        drop(connection);

        let diagnosis = doctor(&config).expect("diagnose abandoned run");
        assert_eq!(diagnosis.status, "lock_held");
        assert!(diagnosis.read_only);
        assert!(matches!(up(&config), Err(DbmError::Safety(_))));
        let applied_plan =
            repair_plan_json(&config, "20261021162643981234", "up", "operation_applied");
        let not_applied_plan = repair_plan_json(
            &config,
            "20261021162643981234",
            "up",
            "operation_not_applied",
        );
        let mut invalid_plan: serde_json::Value =
            serde_json::from_str(&applied_plan).expect("parse generated repair plan");
        invalid_plan["steps"][0]["statement_hash"] =
            serde_json::json!("sha256:wrong-statement-hash");
        let invalid_plan = invalid_plan.to_string();
        assert!(matches!(
            reconcile_attempt(&config, &run_id, "operation_applied", &invalid_plan, true,),
            Err(DbmError::Safety(_))
        ));
        assert!(matches!(
            reconcile_attempt(
                &config,
                &run_id,
                "operation_not_applied",
                &not_applied_plan,
                true,
            ),
            Err(DbmError::Safety(_))
        ));
        assert!(matches!(
            reconcile_attempt(&config, &run_id, "operation_applied", &applied_plan, false,),
            Err(DbmError::Safety(_))
        ));

        let reconciled =
            reconcile_attempt(&config, &run_id, "operation_applied", &applied_plan, true)
                .expect("reconcile using matching evidence and explicit approval");
        assert_eq!(reconciled.status, "reconciled");
        let after_reconciliation = read_snapshot(&config).expect("read reconciliation history");
        assert_eq!(after_reconciliation.runs.len(), 2);
        assert_eq!(after_reconciliation.runs[0].state, "reconciled");
        assert_eq!(
            after_reconciliation.runs[0].reconciliation_plan.as_deref(),
            Some(applied_plan.as_str())
        );
        assert_eq!(
            after_reconciliation.runs[1].reconciliation_plan.as_deref(),
            Some(applied_plan.as_str())
        );
        assert_eq!(
            after_reconciliation.runs[0]
                .reconciled_from_state
                .as_deref(),
            Some("started")
        );
        assert_eq!(
            after_reconciliation
                .lock
                .as_ref()
                .map(|lock| lock.state.as_str()),
            Some("released")
        );
        assert_eq!(
            doctor(&config).expect("doctor after reconciliation").status,
            "healthy"
        );

        let applied = up(&config).expect("new migration allowed after reconciliation");
        assert_eq!(applied.migrations, vec!["20261021162743981234"]);
    }

    #[test]
    fn applied_file_edit_rename_and_missing_file_are_divergences() {
        let (_directory, config) = setup();
        let original = config.source_dirs[0]
            .path
            .join("20261021162643981234_create_items.sql");
        fs::write(
            &original,
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\n-- dbm:down\nDROP TABLE items;\n",
        )
        .expect("write migration");
        up(&config).expect("apply migration");

        fs::write(
            &original,
            "-- dbm:up\nCREATE TABLE items (id INTEGER, name TEXT);\n-- dbm:down\nDROP TABLE items;\n",
        )
        .expect("edit migration");
        assert_eq!(
            status(&config).expect("status after edit").status,
            "diverged"
        );
        assert!(matches!(plan(&config), Err(DbmError::Diverged(_))));

        fs::rename(
            &original,
            config.source_dirs[0]
                .path
                .join("20261021162643981234_renamed.sql"),
        )
        .expect("rename migration");
        assert_eq!(
            status(&config).expect("status after rename").status,
            "diverged"
        );
    }

    #[test]
    fn blocks_control_statements_and_writes_to_internal_tables() {
        for (index, sql) in [
            "-- dbm:up\nCREATE TABLE item (id INTEGER); COMMIT;\n",
            "-- dbm:up\nCREATE TABLE schema_migrations (id INTEGER);\n",
            "-- dbm:up\nPRAGMA journal_mode = WAL;\n",
        ]
        .into_iter()
        .enumerate()
        {
            let (_directory, config) = setup();
            write_migration(
                &config,
                &format!("2026102116264398123{index}_blocked.sql"),
                sql,
            );
            assert!(up(&config).is_err());
            assert_eq!(
                status(&config).expect("status after block").status,
                "pending"
            );
        }
    }

    #[test]
    fn sqlite_immediate_writer_lock_fails_without_retrying_the_migration() {
        let (_directory, mut config) = setup();
        config.config.execution.busy_timeout_seconds = 0;
        write_migration(
            &config,
            "20261021162643981234_create_items.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\n",
        );

        let mut other_writer = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open lock database");
        let transaction = other_writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("acquire writer lock");
        let error = up(&config).expect_err("second writer cannot acquire lock");
        assert!(matches!(error, DbmError::Lock(_)));
        transaction.rollback().expect("release writer lock");
        assert_eq!(
            status(&config).expect("status after lock").status,
            "pending"
        );
    }

    #[test]
    fn sqlite_trigger_body_with_internal_semicolons_executes_as_one_statement() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_create_audited_items.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\nCREATE TABLE audit (item_id INTEGER);\nCREATE TRIGGER items_audit AFTER INSERT ON items BEGIN\n    INSERT INTO audit (item_id) VALUES (NEW.id);\n    UPDATE items SET id = id WHERE id = NEW.id;\nEND;\n-- dbm:down\nDROP TRIGGER items_audit;\nDROP TABLE audit;\nDROP TABLE items;\n",
        );
        up(&config).expect("apply trigger migration");

        let connection = rusqlite::Connection::open(
            config.sqlite_database_path().expect("SQLite database path"),
        )
        .expect("open target");
        connection
            .execute("INSERT INTO items (id) VALUES (42)", [])
            .expect("trigger runs");
        let audit_id: i64 = connection
            .query_row("SELECT item_id FROM audit", [], |row| row.get(0))
            .expect("read audit row");
        assert_eq!(audit_id, 42);
        drop(connection);

        down(&config, 1).expect("drop trigger and tables");
    }

    #[test]
    fn out_of_order_and_production_writes_fail_closed() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_first.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\n",
        );
        up(&config).expect("apply first");
        write_migration(
            &config,
            "20261021162543981234_older.sql",
            "-- dbm:up\nSELECT 1;\n",
        );
        assert!(matches!(plan(&config), Err(DbmError::Diverged(_))));

        let mut production_config = config.clone();
        production_config.config.target.environment = crate::config::Environment::Production;
        assert!(matches!(up(&production_config), Err(DbmError::Safety(_))));
    }

    #[test]
    fn down_preflights_all_selected_migrations_before_reverting_any() {
        let (_directory, config) = setup();
        write_migration(
            &config,
            "20261021162643981234_irreversible.sql",
            "-- dbm:up\nCREATE TABLE items (id INTEGER);\n",
        );
        write_migration(
            &config,
            "20261021162743981234_reversible.sql",
            "-- dbm:up\nALTER TABLE items ADD COLUMN name TEXT;\n-- dbm:down\nALTER TABLE items DROP COLUMN name;\n",
        );
        up(&config).expect("apply both migrations");

        assert!(matches!(down(&config, 2), Err(DbmError::Safety(_))));
        assert_eq!(
            status(&config)
                .expect("status after blocked rollback")
                .status,
            "up_to_date"
        );
        assert_eq!(
            history(&config)
                .expect("history after blocked rollback")
                .runs
                .len(),
            2
        );
    }
}
