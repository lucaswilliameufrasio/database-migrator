use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::config::LoadedConfig;
use crate::error::{DbmError, Result};
use crate::legacy::{LegacyAdoptionWrite, LegacyImportSnapshot, LegacyImportWrite, LegacyTool};
use crate::migration::Migration;
use crate::storage::{
    FinishedRunData, LockData, MetadataSnapshot, PlannedRunData, ReconciliationData, StepData,
};

mod mysql;
mod postgres;
mod sqlite;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionIntent {
    ReadOnly,
    // The shared mutation session is wired into the engine when G4 execution is enabled.
    #[allow(dead_code)]
    ReadWrite,
}

#[derive(Clone, Debug)]
pub(crate) struct DatabaseInfo {
    pub(crate) dialect: &'static str,
    pub(crate) server_version: String,
    pub(crate) database: Option<String>,
    pub(crate) schema: Option<String>,
    pub(crate) target_identity: Option<String>,
    pub(crate) session_id: Option<String>,
}

pub(crate) trait DatabaseAdapter: Send + Sync {
    fn connect<'a>(
        &'a self,
        config: &'a LoadedConfig,
        intent: SessionIntent,
    ) -> BoxFuture<'a, Result<Box<dyn DatabaseSession>>>;
}

#[allow(dead_code)]
pub(crate) trait DatabaseSession: Send {
    fn info(&self) -> &DatabaseInfo;

    fn read_snapshot<'a>(
        &'a mut self,
        namespace: &'a str,
    ) -> BoxFuture<'a, Result<MetadataSnapshot>>;

    fn inspect_legacy_history<'a>(
        &'a mut self,
        _tool: LegacyTool,
        _source_identity: &'a str,
    ) -> BoxFuture<'a, Result<LegacyImportSnapshot>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support legacy history inspection".to_owned(),
            ))
        })
    }

    fn persist_legacy_import<'a>(
        &'a mut self,
        _import: LegacyImportWrite<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support legacy import writes".to_owned(),
            ))
        })
    }

    fn persist_legacy_adoption<'a>(
        &'a mut self,
        _adoption: LegacyAdoptionWrite<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support legacy adoption writes".to_owned(),
            ))
        })
    }

    fn read_legacy_adoption_id<'a>(
        &'a mut self,
        _namespace: &'a str,
        _legacy_migration_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support legacy adoption lookup".to_owned(),
            ))
        })
    }

    fn initialize_metadata<'a>(&'a mut self, initialized_at: &'a str) -> BoxFuture<'a, Result<()>>;

    fn upgrade_metadata_to_current<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support explicit metadata upgrades".to_owned(),
            ))
        })
    }

    fn insert_planned_run<'a>(
        &'a mut self,
        migration: &'a Migration,
        run: PlannedRunData<'a>,
    ) -> BoxFuture<'a, Result<()>>;

    fn mark_run_started<'a>(
        &'a mut self,
        run_id: &'a str,
        started_at: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn insert_step<'a>(&'a mut self, step: StepData<'a>) -> BoxFuture<'a, Result<()>>;

    fn acknowledge_step<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        acknowledged_at: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn mark_step_indeterminate<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        error_code: &'a str,
        evidence: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn reconcile_run<'a>(
        &'a mut self,
        _reconciliation: ReconciliationData<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support reconciliation writes".to_owned(),
            ))
        })
    }

    fn release_reconciled_lock<'a>(
        &'a mut self,
        _run_id: &'a str,
        _released_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support reconciliation lock release".to_owned(),
            ))
        })
    }

    fn inspect_postgres_index<'a>(
        &'a mut self,
        _create_statement: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "this database adapter does not support PostgreSQL catalog inspection".to_owned(),
            ))
        })
    }

    fn insert_applied<'a>(
        &'a mut self,
        migration: &'a Migration,
        applied_at: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn delete_applied<'a>(&'a mut self, migration: &'a Migration) -> BoxFuture<'a, Result<()>>;

    fn finish_run<'a>(&'a mut self, run: FinishedRunData<'a>) -> BoxFuture<'a, Result<()>>;

    fn record_lock<'a>(&'a mut self, lock: LockData<'a>) -> BoxFuture<'a, Result<i64>>;

    fn release_recorded_lock<'a>(
        &'a mut self,
        owner_id: &'a str,
        released_at: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn verify_session<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;

    fn acquire_lock<'a>(&'a mut self, timeout: Duration) -> BoxFuture<'a, Result<()>>;

    fn release_lock<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;

    fn begin<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;

    fn execute_statement<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>>;

    fn execute_nontransactional_statement<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;

    fn rollback<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;

    fn close<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
}

pub(crate) fn adapter_for(config: &LoadedConfig) -> Result<Box<dyn DatabaseAdapter>> {
    match config.config.target.driver.as_str() {
        "sqlite" => Ok(Box::new(sqlite::SqliteAdapter)),
        "postgres" => Ok(Box::new(postgres::PostgresAdapter)),
        "mysql" => Ok(Box::new(mysql::MySqlAdapter)),
        driver => Err(DbmError::Unsupported(format!(
            "database adapter '{driver}' is not available"
        ))),
    }
}
