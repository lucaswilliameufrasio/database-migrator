use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{DbmError, Result};

pub const IMPORT_SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum LegacyTool {
    Knex,
    Goose,
    Sqlx,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyChecksum {
    pub algorithm: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyEvent {
    pub event_id: String,
    pub original_version: String,
    pub state: LegacyEventState,
    pub filename: Option<String>,
    pub batch_id: Option<String>,
    pub applied_at: Option<String>,
    pub checksum: Option<LegacyChecksum>,
    pub execution_time_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyEventState {
    Applied,
    Reverted,
    Failed,
    Sentinel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Partial,
    Unverifiable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportedMigration {
    pub original_identity: String,
    pub original_version: String,
    pub filename: Option<String>,
    pub batch_id: Option<String>,
    pub applied_at: Option<String>,
    pub checksum: Option<LegacyChecksum>,
    pub execution_time_ms: Option<i64>,
    pub verification_status: VerificationStatus,
    pub rollback_blocked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportBlocker {
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportSnapshot {
    pub schema_version: u32,
    pub tool: LegacyTool,
    /// A digest of the database identity; raw connection details are never retained.
    pub source_identity: String,
    pub events: Vec<LegacyEvent>,
    pub current_migrations: Vec<ImportedMigration>,
    pub blockers: Vec<ImportBlocker>,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportPlan {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub source: LegacyImportSnapshot,
    pub local_migrations: Vec<NativeMigrationFingerprint>,
    pub blockers: Vec<ImportBlocker>,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportApplyReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub import_id: String,
    pub digest: String,
    pub event_count: usize,
    pub current_migration_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportVerifyReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub import_id: String,
    pub snapshot_digest: String,
    pub plan_digest: String,
    pub event_count: usize,
    pub current_migration_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyImportAdoptReport {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub import_id: String,
    pub legacy_identity: String,
    pub native_version: String,
    pub adoption_id: String,
}

pub struct LegacyAdoptRequest<'a> {
    pub tool: LegacyTool,
    pub import_id: &'a str,
    pub legacy_identity: &'a str,
    pub native_version: &'a str,
    pub evidence_ref: &'a str,
    pub reason: &'a str,
    pub actor: &'a str,
    pub approved: bool,
    pub approved_target: Option<&'a str>,
}

pub struct LegacyImportWrite<'a> {
    pub namespace: &'a str,
    pub import_id: &'a str,
    pub tool: LegacyTool,
    pub source: &'a LegacyImportSnapshot,
    pub plan_digest: &'a str,
    pub imported_at: &'a str,
}

pub struct LegacyAdoptionWrite<'a> {
    pub adoption_id: &'a str,
    pub namespace: &'a str,
    pub import_id: &'a str,
    pub legacy_migration_key: &'a str,
    pub native_migration_key: &'a str,
    pub native_version: &'a str,
    pub native_checksum: &'a str,
    pub evidence_ref: &'a str,
    pub actor: &'a str,
    pub reason: &'a str,
    pub adopted_at: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnexRow {
    pub id: i64,
    pub name: String,
    pub batch: i64,
    pub migration_time: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnexLock {
    pub is_locked: bool,
    pub index: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GooseRow {
    pub id: i64,
    pub version_id: i64,
    pub is_applied: bool,
    pub timestamp: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlxRow {
    pub version: i64,
    pub description: String,
    pub installed_on: Option<String>,
    pub success: bool,
    pub checksum: Vec<u8>,
    pub execution_time_ms: i64,
}

pub fn tool_name(tool: LegacyTool) -> &'static str {
    match tool {
        LegacyTool::Knex => "knex",
        LegacyTool::Goose => "goose",
        LegacyTool::Sqlx => "sqlx",
    }
}

pub fn event_state_name(state: LegacyEventState) -> &'static str {
    match state {
        LegacyEventState::Applied => "applied",
        LegacyEventState::Reverted => "reverted",
        LegacyEventState::Failed => "failed",
        LegacyEventState::Sentinel => "sentinel",
    }
}

pub fn verification_status_name(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Partial => "partial",
        VerificationStatus::Unverifiable => "unverifiable",
    }
}

pub fn parse_legacy_bool(value: &str, tool: &str, field: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" => Ok(true),
        "0" | "false" | "f" | "no" => Ok(false),
        _ => Err(DbmError::InvalidMigration {
            path: format!("legacy:{tool}:{field}"),
            message: "source boolean field has an unsupported representation".to_owned(),
        }),
    }
}

pub fn inspect_knex(
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

pub fn inspect_goose(
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

pub fn inspect_sqlx(source_identity: &str, mut rows: Vec<SqlxRow>) -> Result<LegacyImportSnapshot> {
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
pub struct NativeMigrationFingerprint {
    pub namespace: String,
    pub version: String,
    pub filename: String,
    pub checksum: String,
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

fn invalid_legacy_row(tool: &str, event_id: &str, reason: &str) -> DbmError {
    DbmError::InvalidMigration {
        path: format!("legacy:{tool}:{event_id}"),
        message: reason.to_owned(),
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
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
