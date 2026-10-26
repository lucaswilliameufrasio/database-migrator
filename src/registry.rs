use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

use crate::config::LoadedConfig;
use crate::database::DatabaseInfo;
use crate::dsl::{ResultShape, SafetyClass, SchemaDialect, SchemaMigration};
use crate::error::{DbmError, Result};
use crate::identity::{
    MigrationIdentity, raw_checksum, validate_description, validate_namespace, validate_version,
};
use crate::migration::{
    Migration, MigrationFormat, MigrationKind, SqlDialect, TransactionPolicy,
    split_sql_statements_for_dialect,
};

/// Stable metadata for a migration compiled into an application or runner.
#[derive(Clone, Debug, Serialize)]
pub struct MigrationMetadata {
    identity: MigrationIdentity,
    description: String,
    filename: String,
    source_path: String,
    transaction: TransactionPolicy,
}

impl MigrationMetadata {
    /// Creates validated metadata for a virtual/embedded source.
    pub fn new(
        identity: MigrationIdentity,
        description: impl Into<String>,
        filename: impl Into<String>,
        source_path: impl Into<String>,
        transaction: TransactionPolicy,
    ) -> Result<Self> {
        let description = description.into();
        let filename = filename.into();
        let source_path = source_path.into();
        validate_namespace(&identity.namespace)?;
        validate_version(&identity.version)?;
        validate_description(&description)?;
        validate_virtual_name(&filename, "filename")?;
        validate_virtual_name(&source_path, "source path")?;
        Ok(Self {
            identity,
            description,
            filename,
            source_path,
            transaction,
        })
    }

    pub fn identity(&self) -> &MigrationIdentity {
        &self.identity
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn source_path(&self) -> &str {
        &self.source_path
    }
}

/// A deterministic SQL plan emitted by a compiled Rust migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationPlan {
    dialect: SqlDialect,
    up: Vec<String>,
    down: Option<Vec<String>>,
}

impl MigrationPlan {
    pub fn new(dialect: SqlDialect, up: Vec<String>, down: Option<Vec<String>>) -> Result<Self> {
        validate_statement_list(&up, dialect, "up")?;
        if let Some(down) = down.as_ref() {
            validate_statement_list(down, dialect, "down")?;
        }
        Ok(Self { dialect, up, down })
    }

    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    pub fn up(&self) -> &[String] {
        &self.up
    }

    pub fn down(&self) -> Option<&[String]> {
        self.down.as_deref()
    }
}

/// Compiled Rust code that deterministically describes its SQL operations.
///
/// Implementations are trusted native code. The engine journals and validates
/// the SQL returned by `plan`; this trait does not sandbox other effects the
/// implementation may perform.
pub trait RustMigration: Send + Sync {
    fn plan(&self, dialect: SqlDialect) -> Result<MigrationPlan>;

    fn up<'a>(
        &'a self,
        context: &'a mut crate::engine::MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { context.execute_all().await })
    }

    fn down<'a>(
        &'a self,
        context: &'a mut crate::engine::MigrationContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { context.execute_all().await })
    }
}

#[derive(Clone)]
enum RegistryEntry {
    Sql {
        metadata: MigrationMetadata,
        dialect: SqlDialect,
        up: String,
        down: Option<String>,
    },
    Dsl {
        metadata: MigrationMetadata,
        dialect: SchemaDialect,
        migration: SchemaMigration,
    },
    Rust {
        metadata: MigrationMetadata,
        migration: Arc<dyn RustMigration>,
    },
}

/// Compile-time registry for embedded SQL, schema DSL, and Rust migrations.
#[derive(Clone, Default)]
pub struct MigrationRegistry {
    entries: Vec<RegistryEntry>,
}

impl MigrationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers SQL whose source may be embedded with `include_str!`.
    pub fn register_sql(
        &mut self,
        metadata: MigrationMetadata,
        dialect: SqlDialect,
        up: impl Into<String>,
        down: Option<impl Into<String>>,
    ) -> Result<&mut Self> {
        self.ensure_unique(&metadata)?;
        self.entries.push(RegistryEntry::Sql {
            metadata,
            dialect,
            up: up.into(),
            down: down.map(Into::into),
        });
        Ok(self)
    }

    /// Registers an immutable DSL migration with an explicitly selected compiler dialect.
    pub fn register_dsl(
        &mut self,
        metadata: MigrationMetadata,
        dialect: SchemaDialect,
        migration: SchemaMigration,
    ) -> Result<&mut Self> {
        self.ensure_unique(&metadata)?;
        self.entries.push(RegistryEntry::Dsl {
            metadata,
            dialect,
            migration,
        });
        Ok(self)
    }

    /// Registers custom Rust code that returns a deterministic SQL plan.
    pub fn register_rust(
        &mut self,
        metadata: MigrationMetadata,
        migration: impl RustMigration + 'static,
    ) -> Result<&mut Self> {
        self.register_rust_arc(metadata, Arc::new(migration))
    }

    fn register_rust_arc(
        &mut self,
        metadata: MigrationMetadata,
        migration: Arc<dyn RustMigration>,
    ) -> Result<&mut Self> {
        self.ensure_unique(&metadata)?;
        self.entries.push(RegistryEntry::Rust {
            metadata,
            migration,
        });
        Ok(self)
    }

    fn ensure_unique(&self, metadata: &MigrationMetadata) -> Result<()> {
        if self.entries.iter().any(|entry| {
            entry.metadata().identity.namespace == metadata.identity.namespace
                && entry.metadata().identity.version == metadata.identity.version
        }) {
            return Err(DbmError::InvalidMigration {
                path: metadata.source_path.clone(),
                message: format!(
                    "duplicate compiled migration version {} in namespace {}",
                    metadata.identity.version, metadata.identity.namespace
                ),
            });
        }
        Ok(())
    }

    pub(crate) fn materialize(&self, config: &LoadedConfig) -> Result<Vec<Migration>> {
        let target_dialect = match config.config.target.driver.as_str() {
            "sqlite" => SqlDialect::Sqlite,
            "postgres" => SqlDialect::Postgres,
            "mysql" => SqlDialect::Mysql,
            driver => {
                return Err(DbmError::Unsupported(format!(
                    "compiled migrations do not support driver '{driver}'"
                )));
            }
        };
        let mut migrations = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            if entry.metadata().identity.namespace != config.config.namespace {
                continue;
            }
            let migration = match entry {
                RegistryEntry::Sql {
                    metadata,
                    dialect,
                    up,
                    down,
                } => {
                    if *dialect != target_dialect {
                        return Err(dialect_mismatch(metadata, *dialect, target_dialect));
                    }
                    materialize_sql(metadata, *dialect, up, down.as_deref(), "embedded-sql")?
                }
                RegistryEntry::Dsl {
                    metadata,
                    dialect,
                    migration,
                } => {
                    let sql_dialect = schema_to_sql_dialect(*dialect);
                    if sql_dialect != target_dialect {
                        return Err(dialect_mismatch(metadata, sql_dialect, target_dialect));
                    }
                    let up = migration.up().compile(*dialect)?;
                    let down = migration.down().compile(*dialect)?;
                    let up_sql = compiled_sql(up.statements(), metadata)?;
                    let down_sql = compiled_sql(down.statements(), metadata)?;
                    let checksum_material = format!(
                        "dbm-dsl-migration-v1\0{}\0{}\0{}\0{}\0{}",
                        up.ast_fingerprint(),
                        up.sql_fingerprint(),
                        down.ast_fingerprint(),
                        down.sql_fingerprint(),
                        dialect.label()
                    );
                    materialize_sql(
                        metadata,
                        sql_dialect,
                        &up_sql,
                        Some(&down_sql),
                        &checksum_material,
                    )?
                }
                RegistryEntry::Rust {
                    metadata,
                    migration,
                } => {
                    if metadata.transaction == TransactionPolicy::Never {
                        return Err(DbmError::Unsupported(format!(
                            "Rust migration {} cannot use transaction=never until imperative steps have a non-transactional recovery contract",
                            metadata.identity.version
                        )));
                    }
                    let plan = migration.plan(target_dialect)?;
                    if plan.dialect != target_dialect {
                        return Err(dialect_mismatch(metadata, plan.dialect, target_dialect));
                    }
                    let up = join_statements(&plan.up);
                    let down = plan
                        .down
                        .as_ref()
                        .map(|statements| join_statements(statements));
                    let checksum_material = format!(
                        "dbm-rust-migration-v1\0{}\0{}",
                        target_dialect.label(),
                        plan_fingerprint(&plan)
                    );
                    materialize_sql(
                        metadata,
                        target_dialect,
                        &up,
                        down.as_deref(),
                        &checksum_material,
                    )?
                }
            };
            migrations.push(migration);
        }
        Ok(migrations)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn validate_database_info(&self, info: &DatabaseInfo) -> Result<()> {
        for entry in &self.entries {
            let RegistryEntry::Dsl {
                metadata, dialect, ..
            } = entry
            else {
                continue;
            };
            let matches_server = match dialect {
                SchemaDialect::Mysql => info.server_version.starts_with("MySQL "),
                SchemaDialect::MariaDb => info.server_version.starts_with("MariaDB "),
                SchemaDialect::Sqlite | SchemaDialect::Postgres => true,
            };
            if !matches_server {
                return Err(DbmError::Unsupported(format!(
                    "DSL migration {} was compiled for {} but the connected server is {}; select the matching schema dialect",
                    metadata.identity.version,
                    dialect.label(),
                    info.server_version
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn rust_migration(
        &self,
        identity: &MigrationIdentity,
    ) -> Option<Arc<dyn RustMigration>> {
        self.entries.iter().find_map(|entry| match entry {
            RegistryEntry::Rust {
                metadata,
                migration,
            } if metadata.identity == *identity => Some(Arc::clone(migration)),
            RegistryEntry::Sql { .. } | RegistryEntry::Dsl { .. } | RegistryEntry::Rust { .. } => {
                None
            }
        })
    }
}

impl RegistryEntry {
    fn metadata(&self) -> &MigrationMetadata {
        match self {
            Self::Sql { metadata, .. }
            | Self::Dsl { metadata, .. }
            | Self::Rust { metadata, .. } => metadata,
        }
    }
}

fn materialize_sql(
    metadata: &MigrationMetadata,
    dialect: SqlDialect,
    up_sql: &str,
    down_sql: Option<&str>,
    source_material: &str,
) -> Result<Migration> {
    validate_nonempty_sql(up_sql, dialect, "up", &metadata.source_path)?;
    if let Some(down_sql) = down_sql {
        validate_nonempty_sql(down_sql, dialect, "down", &metadata.source_path)?;
    }
    let metadata_bytes = serde_json::to_vec(metadata)?;
    let mut checksum_material = Vec::new();
    checksum_material.extend_from_slice(b"dbm-registered-source-v1\0");
    checksum_material.extend_from_slice(&(metadata_bytes.len() as u64).to_be_bytes());
    checksum_material.extend_from_slice(&metadata_bytes);
    append_fingerprint_part(&mut checksum_material, dialect.label().as_bytes());
    checksum_material.extend_from_slice(&(source_material.len() as u64).to_be_bytes());
    checksum_material.extend_from_slice(source_material.as_bytes());
    checksum_material.extend_from_slice(&(up_sql.len() as u64).to_be_bytes());
    checksum_material.extend_from_slice(up_sql.as_bytes());
    match down_sql {
        Some(sql) => {
            checksum_material.push(1);
            checksum_material.extend_from_slice(&(sql.len() as u64).to_be_bytes());
            checksum_material.extend_from_slice(sql.as_bytes());
        }
        None => checksum_material.push(0),
    }
    Ok(Migration {
        identity: metadata.identity.clone(),
        kind: MigrationKind::Versioned,
        description: metadata.description.clone(),
        filename: metadata.filename.clone(),
        source_path: format!("compiled://{}", metadata.source_path),
        checksum: raw_checksum(&checksum_material),
        up_sql: up_sql.to_owned(),
        down_sql: down_sql.map(str::to_owned),
        dialect,
        format: MigrationFormat::Single,
        transaction: metadata.transaction,
    })
}

fn validate_nonempty_sql(
    sql: &str,
    dialect: SqlDialect,
    direction: &str,
    path: &str,
) -> Result<()> {
    if split_sql_statements_for_dialect(sql, dialect).is_empty() {
        return Err(DbmError::InvalidMigration {
            path: path.to_owned(),
            message: format!("compiled migration {direction} is empty"),
        });
    }
    Ok(())
}

fn validate_statement_list(
    statements: &[String],
    dialect: SqlDialect,
    direction: &str,
) -> Result<()> {
    if statements.is_empty() {
        return Err(DbmError::InvalidMigration {
            path: "compiled Rust migration".to_owned(),
            message: format!("Rust migration {direction} plan cannot be empty"),
        });
    }
    for (index, statement) in statements.iter().enumerate() {
        if split_sql_statements_for_dialect(statement, dialect).len() != 1 {
            return Err(DbmError::InvalidMigration {
                path: "compiled Rust migration".to_owned(),
                message: format!(
                    "Rust migration {direction} step {} must contain exactly one SQL statement",
                    index + 1
                ),
            });
        }
    }
    Ok(())
}

fn compiled_sql(
    statements: &[crate::dsl::CompiledStatement],
    metadata: &MigrationMetadata,
) -> Result<String> {
    let mut sql = String::new();
    for statement in statements {
        if statement.result_shape() != ResultShape::NoRows {
            return Err(DbmError::Unsupported(format!(
                "DSL migration {} declares a row-returning statement, which the migration journal does not support",
                metadata.identity.version
            )));
        }
        if statement.safety_class() == SafetyClass::NonTransactional
            && metadata.transaction != TransactionPolicy::Never
        {
            return Err(DbmError::Safety(format!(
                "DSL migration {} contains a non-transactional statement and must declare transaction=never",
                metadata.identity.version
            )));
        }
        if !sql.is_empty() {
            sql.push_str(";\n");
        }
        sql.push_str(statement.sql());
    }
    sql.push(';');
    Ok(sql)
}

fn join_statements(statements: &[String]) -> String {
    let mut sql = String::new();
    for statement in statements {
        if !sql.is_empty() {
            sql.push_str(";\n");
        }
        sql.push_str(statement);
    }
    sql.push(';');
    sql
}

fn plan_fingerprint(plan: &MigrationPlan) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(b"dbm-rust-plan-v1\0");
    material.extend_from_slice(plan.dialect.label().as_bytes());
    for statement in &plan.up {
        append_fingerprint_part(&mut material, statement.as_bytes());
    }
    match plan.down.as_ref() {
        Some(statements) => {
            material.push(1);
            for statement in statements {
                append_fingerprint_part(&mut material, statement.as_bytes());
            }
        }
        None => material.push(0),
    }
    raw_checksum(&material)
}

fn append_fingerprint_part(material: &mut Vec<u8>, value: &[u8]) {
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value);
}

fn schema_to_sql_dialect(dialect: SchemaDialect) -> SqlDialect {
    match dialect {
        SchemaDialect::Sqlite => SqlDialect::Sqlite,
        SchemaDialect::Postgres => SqlDialect::Postgres,
        SchemaDialect::Mysql | SchemaDialect::MariaDb => SqlDialect::Mysql,
    }
}

fn dialect_mismatch(
    metadata: &MigrationMetadata,
    declared: SqlDialect,
    target: SqlDialect,
) -> DbmError {
    DbmError::InvalidMigration {
        path: metadata.source_path.clone(),
        message: format!(
            "compiled migration dialect {:?} does not match target dialect {:?}",
            declared, target
        ),
    }
}

fn validate_virtual_name(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('\0')
        || value.starts_with('/')
        || value.split(['/', '\\']).any(|component| component == "..")
    {
        return Err(DbmError::Configuration(format!(
            "compiled migration {label} must be a non-empty virtual path without parent traversal"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::dsl::{SchemaBuilder, SchemaDialect, SchemaMigration};
    use crate::error::DbmError;
    use crate::identity::MigrationIdentity;
    use crate::migration::{SqlDialect, TransactionPolicy};

    use super::{MigrationMetadata, MigrationPlan, MigrationRegistry, RustMigration};

    fn metadata(version: &str, transaction: TransactionPolicy) -> MigrationMetadata {
        MigrationMetadata::new(
            MigrationIdentity {
                namespace: "default".to_owned(),
                version: version.to_owned(),
            },
            "compiled_change",
            format!("{version}_compiled_change.rs"),
            format!("src/db_migrations/m_{version}_compiled_change.rs"),
            transaction,
        )
        .expect("valid migration metadata")
    }

    struct RustPlan;

    impl RustMigration for RustPlan {
        fn plan(&self, dialect: SqlDialect) -> crate::Result<MigrationPlan> {
            MigrationPlan::new(
                dialect,
                vec!["CREATE TABLE compiled_users (id INTEGER PRIMARY KEY)".to_owned()],
                Some(vec!["DROP TABLE compiled_users".to_owned()]),
            )
        }
    }

    #[test]
    fn rust_plan_requires_one_statement_per_step_and_both_bodies_nonempty() {
        assert!(matches!(
            MigrationPlan::new(
                SqlDialect::Sqlite,
                Vec::new(),
                Some(vec!["DROP TABLE users".to_owned()])
            ),
            Err(DbmError::InvalidMigration { .. })
        ));
        assert!(matches!(
            MigrationPlan::new(
                SqlDialect::Sqlite,
                vec!["CREATE TABLE users (id INT); DROP TABLE users".to_owned()],
                None
            ),
            Err(DbmError::InvalidMigration { .. })
        ));
    }

    #[test]
    fn registry_rejects_duplicate_compiled_identity() {
        let mut registry = MigrationRegistry::new();
        registry
            .register_sql(
                metadata("20261024000000000001", TransactionPolicy::Auto),
                SqlDialect::Sqlite,
                "CREATE TABLE first (id INTEGER)",
                None::<String>,
            )
            .expect("register first migration");
        let result = registry.register_rust(
            metadata("20261024000000000001", TransactionPolicy::Auto),
            RustPlan,
        );
        assert!(matches!(result, Err(DbmError::InvalidMigration { .. })));
    }

    #[test]
    fn schema_registration_keeps_explicit_dialect_and_up_down() {
        let mut up = SchemaBuilder::new();
        up.create_table("compiled_users", |table| {
            table
                .column("id", crate::dsl::ColumnType::Integer)?
                .primary_key();
            Ok(())
        })
        .expect("build Up");
        let mut down = SchemaBuilder::new();
        down.drop_table("compiled_users", false)
            .expect("build Down");
        let migration = SchemaMigration::new(
            up.finish().expect("seal Up"),
            down.finish().expect("seal Down"),
        )
        .expect("valid DSL migration");

        let mut registry = MigrationRegistry::new();
        registry
            .register_dsl(
                metadata("20261024000000000002", TransactionPolicy::Auto),
                SchemaDialect::Sqlite,
                migration,
            )
            .expect("register DSL migration");
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }
}
