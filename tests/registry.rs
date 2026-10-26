use std::fs;
use std::future::Future;
use std::pin::Pin;

use database_migrator::dsl::{ColumnType, SchemaBuilder, SchemaDialect, SchemaMigration};
use database_migrator::engine::{self, MigrationContext};
use database_migrator::registry::{
    MigrationMetadata, MigrationPlan, MigrationRegistry, RustMigration,
};
use database_migrator::{MigrationIdentity, SqlDialect, TransactionPolicy, config};
use rusqlite::Connection;
use tempfile::tempdir;

struct RustTableMigration;

impl RustMigration for RustTableMigration {
    fn plan(&self, dialect: SqlDialect) -> database_migrator::Result<MigrationPlan> {
        MigrationPlan::new(
            dialect,
            vec!["CREATE TABLE rust_migrations (id INTEGER PRIMARY KEY)".to_owned()],
            Some(vec!["DROP TABLE rust_migrations".to_owned()]),
        )
    }

    fn up<'a>(
        &'a self,
        context: &'a mut MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = database_migrator::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            context
                .execute("CREATE TABLE rust_migrations (id INTEGER PRIMARY KEY)")
                .await
        })
    }

    fn down<'a>(
        &'a self,
        context: &'a mut MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = database_migrator::Result<()>> + Send + 'a>> {
        Box::pin(async move { context.execute_next().await })
    }
}

fn metadata(version: &str, name: &str) -> MigrationMetadata {
    MigrationMetadata::new(
        MigrationIdentity {
            namespace: "default".to_owned(),
            version: version.to_owned(),
        },
        name,
        format!("{version}_{name}.rs"),
        format!("src/db_migrations/{version}_{name}.rs"),
        TransactionPolicy::Auto,
    )
    .expect("valid migration metadata")
}

fn registry() -> MigrationRegistry {
    let mut up = SchemaBuilder::new();
    up.create_table("dsl_migrations", |table| {
        table.column("id", ColumnType::Integer)?.primary_key();
        Ok(())
    })
    .expect("build DSL Up");
    let mut down = SchemaBuilder::new();
    down.drop_table("dsl_migrations", false)
        .expect("build DSL Down");
    let dsl = SchemaMigration::new(
        up.finish().expect("seal DSL Up"),
        down.finish().expect("seal DSL Down"),
    )
    .expect("validate DSL migration");

    let mut registry = MigrationRegistry::new();
    registry
        .register_sql(
            metadata("20261024000000000002", "embedded_sql"),
            SqlDialect::Sqlite,
            "CREATE TABLE embedded_migrations (id INTEGER PRIMARY KEY)",
            Some("DROP TABLE embedded_migrations".to_owned()),
        )
        .expect("register embedded SQL");
    registry
        .register_dsl(
            metadata("20261024000000000003", "schema_dsl"),
            SchemaDialect::Sqlite,
            dsl,
        )
        .expect("register DSL migration");
    registry
        .register_rust(
            metadata("20261024000000000004", "rust_plan"),
            RustTableMigration,
        )
        .expect("register Rust migration");
    registry
}

#[tokio::test]
async fn one_engine_executes_filesystem_embedded_dsl_and_rust_plans_together() {
    let directory = tempdir().expect("create isolated project");
    let migrations = directory.path().join("migrations/versioned");
    fs::create_dir_all(&migrations).expect("create SQL migration source");
    fs::write(
        migrations.join("20261024000000000001_filesystem.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE filesystem_migrations (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE filesystem_migrations;\n",
    )
    .expect("write filesystem SQL migration");
    fs::write(
        directory.path().join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n",
    )
    .expect("write isolated config");
    let config = config::load(&directory.path().join("dbm.toml")).expect("load config");
    let registry = registry();
    let embedded_source = directory
        .path()
        .join("src/db_migrations/20261024000000000002_embedded_sql.rs");
    assert!(
        !embedded_source.exists(),
        "embedded migrations must not depend on a deployed source file"
    );

    let plan = engine::plan_with_registry_async(&config, &registry)
        .await
        .expect("plan all source types");
    assert_eq!(plan.operations.len(), 4);
    assert_eq!(plan.status, "ready");
    let repeated_plan = engine::plan_with_registry_async(&config, &registry)
        .await
        .expect("repeat plan for deterministic fingerprints");
    assert_eq!(plan.plan_hash, repeated_plan.plan_hash);

    let applied = engine::up_with_registry_async(&config, &registry)
        .await
        .expect("execute complete mixed plan");
    assert_eq!(applied.status, "succeeded");
    assert_eq!(applied.migrations.len(), 4);

    let connection = Connection::open(directory.path().join("dbm.sqlite"))
        .expect("open isolated SQLite database");
    for table in [
        "filesystem_migrations",
        "embedded_migrations",
        "dsl_migrations",
        "rust_migrations",
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .expect("check migrated table");
        assert!(exists, "expected {table} to be created");
    }
    let rust_step_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             JOIN schema_migration_runs AS run ON run.run_id = step.run_id \
             WHERE run.migration_version = ?1 AND run.direction = 'up' \
               AND step.state = 'acknowledged'",
            ["20261024000000000004"],
            |row| row.get(0),
        )
        .expect("read journaled Rust migration step count");
    assert_eq!(rust_step_count, 1);
    let (migration_key, filename, source_path, checksum): (String, String, String, String) =
        connection
            .query_row(
                "SELECT migration_key, filename, source_path, content_checksum \
                 FROM schema_migrations \
                 WHERE namespace = ?1 AND native_version = ?2",
                ["default", "20261024000000000002"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read embedded migration identity and checksum");
    assert_eq!(migration_key.len(), 64);
    assert!(migration_key.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(filename, "20261024000000000002_embedded_sql.rs");
    assert_eq!(
        source_path,
        "compiled://src/db_migrations/20261024000000000002_embedded_sql.rs"
    );
    assert!(checksum.starts_with("sha256:raw:v1:"));
    drop(connection);

    let reverted = engine::down_with_registry_async(&config, &registry, 4)
        .await
        .expect("roll back complete mixed plan");
    assert_eq!(reverted.status, "succeeded");
    assert_eq!(reverted.migrations.len(), 4);

    let connection = Connection::open(directory.path().join("dbm.sqlite"))
        .expect("reopen isolated SQLite database");
    for table in [
        "filesystem_migrations",
        "embedded_migrations",
        "dsl_migrations",
        "rust_migrations",
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .expect("check rolled-back table");
        assert!(!exists, "expected {table} to be dropped");
    }
}
