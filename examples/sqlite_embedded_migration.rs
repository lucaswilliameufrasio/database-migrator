use std::error::Error;
use std::fs;

use database_migrator::config;
use database_migrator::engine;
use database_migrator::registry::{MigrationMetadata, MigrationRegistry};
use database_migrator::{MigrationIdentity, SqlDialect, TransactionPolicy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("migrations/versioned");
    fs::create_dir_all(&source)?;
    fs::write(
        directory.path().join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n",
    )?;

    let loaded_config = config::load(&directory.path().join("dbm.toml"))?;
    let version = "20261026000000000001";
    let metadata = MigrationMetadata::new(
        MigrationIdentity {
            namespace: "default".to_owned(),
            version: version.to_owned(),
        },
        "create_example_table",
        format!("{version}_create_example_table.sql"),
        format!("embedded/{version}_create_example_table.sql"),
        TransactionPolicy::Auto,
    )?;
    let mut registry = MigrationRegistry::new();
    registry.register_sql(
        metadata,
        SqlDialect::Sqlite,
        "CREATE TABLE example (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        Some("DROP TABLE example".to_owned()),
    )?;

    let applied = engine::up_with_registry_async(&loaded_config, &registry).await?;
    println!("{}: applied {:?}", applied.status, applied.migrations);

    let rolled_back = engine::down_with_registry_async(&loaded_config, &registry, 1).await?;
    println!(
        "{}: rolled back {:?}",
        rolled_back.status, rolled_back.migrations
    );
    Ok(())
}
