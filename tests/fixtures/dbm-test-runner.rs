use std::future::Future;
use std::pin::Pin;
use std::process;
use std::thread;
use std::time::Duration;

use database_migrator::dsl::{ColumnType, SchemaBuilder, SchemaDialect, SchemaMigration};
use database_migrator::engine::MigrationContext;
use database_migrator::registry::{
    MigrationMetadata, MigrationPlan, MigrationRegistry, RustMigration,
};
use database_migrator::{MigrationIdentity, Result, SqlDialect, TransactionPolicy, runner};

struct FixtureRustMigration;

impl RustMigration for FixtureRustMigration {
    fn plan(&self, dialect: SqlDialect) -> Result<MigrationPlan> {
        MigrationPlan::new(
            dialect,
            vec!["CREATE TABLE runner_rust (id INTEGER PRIMARY KEY)".to_owned()],
            Some(vec!["DROP TABLE runner_rust".to_owned()]),
        )
    }

    fn up<'a>(
        &'a self,
        context: &'a mut MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { context.execute_next().await })
    }

    fn down<'a>(
        &'a self,
        context: &'a mut MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { context.execute_next().await })
    }
}

fn metadata(namespace: &str, version: &str, name: &str) -> Result<MigrationMetadata> {
    MigrationMetadata::new(
        MigrationIdentity {
            namespace: namespace.to_owned(),
            version: version.to_owned(),
        },
        name,
        format!("{version}_{name}.rs"),
        format!("src/db_migrations/{version}_{name}.rs"),
        TransactionPolicy::Auto,
    )
}

fn registry() -> Result<MigrationRegistry> {
    let mut registry = MigrationRegistry::new();
    for namespace in ["default", "tenant_a"] {
        let mut up = SchemaBuilder::new();
        up.create_table("runner_dsl", |table| {
            table.column("id", ColumnType::Integer)?.primary_key();
            Ok(())
        })?;
        let mut down = SchemaBuilder::new();
        down.drop_table("runner_dsl", false)?;
        let schema = SchemaMigration::new(up.finish()?, down.finish()?)?;

        registry.register_sql(
            metadata(namespace, "20261024000000000002", "runner_embedded")?,
            SqlDialect::Sqlite,
            "CREATE TABLE runner_embedded (id INTEGER PRIMARY KEY)",
            Some("DROP TABLE runner_embedded".to_owned()),
        )?;
        registry.register_dsl(
            metadata(namespace, "20261024000000000003", "runner_dsl")?,
            SchemaDialect::Sqlite,
            schema,
        )?;
        registry.register_rust(
            metadata(namespace, "20261024000000000004", "runner_rust")?,
            FixtureRustMigration,
        )?;
    }
    Ok(registry)
}

#[tokio::main]
async fn main() {
    let mode = std::env::var("DBM_RUNNER_FIXTURE_MODE").unwrap_or_default();
    if mode == "sleep" {
        thread::sleep(Duration::from_secs(5));
        return;
    }
    if mode == "bad_protocol" {
        println!(
            "{}",
            serde_json::json!({
                "kind": "ready",
                "protocol_version": 999,
                "engine_version": env!("CARGO_PKG_VERSION"),
                "metadata_schema_version": 5,
                "runner_fingerprint": "sha256:raw:v1:fixture",
                "namespace": "default",
                "capabilities": [
                    "filesystem_sql",
                    "embedded_sql",
                    "schema_dsl",
                    "rust_plan",
                    "mixed_plan"
                ],
                "manifest_fingerprint": "fixture-manifest",
                "plan_hash": "fixture-plan",
                "target_fingerprint": "fixture-target",
                "report": {"plan_hash": "fixture-plan"}
            })
        );
        return;
    }

    let registry = match registry() {
        Ok(registry) => registry,
        Err(_) => process::exit(1),
    };
    if mode == "extra_plan_frame" {
        if runner::serve(registry).await.is_err() {
            process::exit(1);
        }
        println!("{{\"kind\":\"unexpected\"}}");
        return;
    }
    if runner::serve(registry).await.is_err() {
        process::exit(1);
    }
}
