use database_migrator_core::config::LoadedConfig;
use database_migrator_core::error::{DbmError, Result};

pub(crate) use database_migrator_core::database::{DatabaseInfo, DatabaseSession, SessionIntent};

pub(crate) fn ensure_adapters() {
    database_migrator_core::database::install_adapter_factory(adapter_for_driver);
}

pub(crate) fn adapter_for(
    config: &LoadedConfig,
) -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    ensure_adapters();
    database_migrator_core::database::adapter_for(config)
}

fn adapter_for_driver(
    config: &LoadedConfig,
) -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    match config.config.target.driver.as_str() {
        "sqlite" => Ok(Box::new(database_migrator_sqlite::SqliteAdapter)),
        "postgres" => Ok(Box::new(database_migrator_postgres::PostgresAdapter)),
        "mysql" => Ok(Box::new(database_migrator_mysql::MySqlAdapter)),
        driver => Err(DbmError::Unsupported(format!(
            "database adapter '{driver}' is not available"
        ))),
    }
}
