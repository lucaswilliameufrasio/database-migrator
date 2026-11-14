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
        "sqlite" => sqlite_adapter(),
        "postgres" => postgres_adapter(),
        "mysql" => mysql_adapter(),
        driver => Err(DbmError::Unsupported(format!(
            "database adapter '{driver}' is not available"
        ))),
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Ok(Box::new(database_migrator_sqlite::SqliteAdapter))
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Err(disabled_adapter("sqlite"))
}

#[cfg(feature = "postgres")]
fn postgres_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Ok(Box::new(database_migrator_postgres::PostgresAdapter))
}

#[cfg(not(feature = "postgres"))]
fn postgres_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Err(disabled_adapter("postgres"))
}

#[cfg(feature = "mysql")]
fn mysql_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Ok(Box::new(database_migrator_mysql::MySqlAdapter))
}

#[cfg(not(feature = "mysql"))]
fn mysql_adapter() -> Result<Box<dyn database_migrator_core::database::DatabaseAdapter>> {
    Err(disabled_adapter("mysql"))
}

#[cfg(not(all(feature = "sqlite", feature = "postgres", feature = "mysql")))]
fn disabled_adapter(feature: &str) -> DbmError {
    DbmError::Unsupported(format!(
        "database adapter '{feature}' requires the {feature} feature"
    ))
}
