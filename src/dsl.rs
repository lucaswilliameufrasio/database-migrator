//! Provisional, declarative schema plans compiled to SQL without a database connection.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{DbmError, Result};
use crate::identity::hex_digest;
use crate::migration::{SqlDialect, split_sql_statements_for_dialect};

const AST_FINGERPRINT_DOMAIN: &[u8] = b"dbm-schema-ast-v1\0";
const SQL_FINGERPRINT_DOMAIN: &[u8] = b"dbm-schema-sql-v1\0";
const SCHEMA_COMPILER_VERSION: &str = "dbm-schema-compiler-v1";
const MAX_PORTABLE_IDENTIFIER_BYTES: usize = 63;

/// A target dialect. MariaDB is kept distinct from MySQL where type semantics differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDialect {
    Sqlite,
    Postgres,
    Mysql,
    #[serde(rename = "mariadb")]
    MariaDb,
}

impl SchemaDialect {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::MariaDb => "mariadb",
        }
    }

    fn sql_dialect(self) -> SqlDialect {
        match self {
            Self::Sqlite => SqlDialect::Sqlite,
            Self::Postgres => SqlDialect::Postgres,
            Self::Mysql | Self::MariaDb => SqlDialect::Mysql,
        }
    }

    fn quotes_identifiers_with_backticks(self) -> bool {
        matches!(self, Self::Mysql | Self::MariaDb)
    }

    fn ddl_safety(self) -> SafetyClass {
        match self {
            Self::Sqlite | Self::Postgres => SafetyClass::Transactional,
            Self::Mysql | Self::MariaDb => SafetyClass::NonTransactional,
        }
    }
}

impl SafetyClass {
    fn label(self) -> &'static [u8] {
        match self {
            Self::Transactional => b"transactional",
            Self::NonTransactional => b"non_transactional",
        }
    }
}

impl ResultShape {
    fn label(self) -> &'static [u8] {
        match self {
            Self::NoRows => b"no_rows",
            Self::Rows => b"rows",
        }
    }
}

/// A semantic column type, rather than a caller-provided SQL fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum ColumnType {
    Integer,
    BigInt,
    Varchar(u32),
    Text,
    Boolean,
    Decimal { precision: u8, scale: u8 },
    Timestamp,
    TimestampWithTimeZone,
    Binary,
    Json(JsonStorage),
    Uuid(UuidStorage),
}

/// The caller explicitly selects native server JSON semantics or plain text storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonStorage {
    Native,
    Text,
}

/// UUID representation is explicit because native support is not portable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UuidStorage {
    Native,
    Char36,
    Binary16,
}

/// Typed literal values accepted for defaults and check expressions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum LiteralValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Decimal(String),
    String(String),
}

/// A default value. SQL expressions are intentionally limited to current time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnDefault {
    Literal(LiteralValue),
    CurrentTimestamp,
}

/// Declares how an opaque raw SQL statement behaves; the compiler does not inspect it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClass {
    Transactional,
    NonTransactional,
}

/// Declares whether a raw statement returns rows, which a future runner must handle explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultShape {
    NoRows,
    Rows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Like,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CheckNode {
    Column {
        name: String,
    },
    Value {
        value: LiteralValue,
    },
    Compare {
        left: Box<CheckNode>,
        operator: ComparisonOperator,
        right: Box<CheckNode>,
    },
    And {
        left: Box<CheckNode>,
        right: Box<CheckNode>,
    },
    Or {
        left: Box<CheckNode>,
        right: Box<CheckNode>,
    },
    Not {
        expression: Box<CheckNode>,
    },
    IsNull {
        expression: Box<CheckNode>,
        negated: bool,
    },
}

/// A small expression tree for CHECK constraints; it cannot contain raw SQL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckExpression {
    node: CheckNode,
}

impl CheckExpression {
    pub fn column(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_identifier(&name, "check-expression column")?;
        Ok(Self {
            node: CheckNode::Column { name },
        })
    }

    pub fn value(value: LiteralValue) -> Result<Self> {
        if value == LiteralValue::Null {
            return Err(invalid_dsl(
                "NULL must be expressed with is_null() or is_not_null() in CHECK constraints",
            ));
        }
        validate_literal(&value)?;
        Ok(Self {
            node: CheckNode::Value { value },
        })
    }

    pub fn compare(self, operator: ComparisonOperator, right: Self) -> Result<Self> {
        if matches!(
            (&self.node, &right.node),
            (
                CheckNode::Value {
                    value: LiteralValue::Null
                },
                _
            ) | (
                _,
                CheckNode::Value {
                    value: LiteralValue::Null
                }
            )
        ) {
            return Err(invalid_dsl(
                "NULL comparisons must use is_null() or is_not_null()",
            ));
        }
        Ok(Self {
            node: CheckNode::Compare {
                left: Box::new(self.node),
                operator,
                right: Box::new(right.node),
            },
        })
    }

    pub fn and(self, right: Self) -> Self {
        Self {
            node: CheckNode::And {
                left: Box::new(self.node),
                right: Box::new(right.node),
            },
        }
    }

    pub fn or(self, right: Self) -> Self {
        Self {
            node: CheckNode::Or {
                left: Box::new(self.node),
                right: Box::new(right.node),
            },
        }
    }

    pub fn negate(self) -> Self {
        Self {
            node: CheckNode::Not {
                expression: Box::new(self.node),
            },
        }
    }

    pub fn is_null(self) -> Self {
        Self {
            node: CheckNode::IsNull {
                expression: Box::new(self.node),
                negated: false,
            },
        }
    }

    pub fn is_not_null(self) -> Self {
        Self {
            node: CheckNode::IsNull {
                expression: Box::new(self.node),
                negated: true,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckConstraint {
    name: String,
    expression: CheckExpression,
}

impl CheckConstraint {
    pub fn new(name: impl Into<String>, expression: CheckExpression) -> Result<Self> {
        let name = name.into();
        validate_identifier(&name, "check constraint")?;
        Ok(Self { name, expression })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ForeignKey {
    columns: Vec<String>,
    referenced_table: String,
    referenced_columns: Vec<String>,
    on_delete: Option<ForeignKeyAction>,
    on_update: Option<ForeignKeyAction>,
}

impl ForeignKey {
    pub fn new(
        columns: &[&str],
        referenced_table: impl Into<String>,
        referenced_columns: &[&str],
    ) -> Result<Self> {
        if columns.is_empty() || columns.len() != referenced_columns.len() {
            return Err(invalid_dsl(
                "foreign key local and referenced column lists must be non-empty and have equal length",
            ));
        }
        let columns = validated_identifier_list(columns, "foreign-key column")?;
        let referenced_columns =
            validated_identifier_list(referenced_columns, "referenced column")?;
        let referenced_table = referenced_table.into();
        validate_identifier(&referenced_table, "referenced table")?;
        Ok(Self {
            columns,
            referenced_table,
            referenced_columns,
            on_delete: None,
            on_update: None,
        })
    }

    pub fn on_delete(mut self, action: ForeignKeyAction) -> Self {
        self.on_delete = Some(action);
        self
    }

    pub fn on_update(mut self, action: ForeignKeyAction) -> Self {
        self.on_update = Some(action);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ColumnDefinition {
    name: String,
    column_type: ColumnType,
    not_null: bool,
    primary_key: bool,
    unique: bool,
    default: Option<ColumnDefault>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct UniqueConstraint {
    name: String,
    columns: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TableDefinition {
    name: String,
    columns: Vec<ColumnDefinition>,
    primary_key: Option<Vec<String>>,
    unique_constraints: Vec<UniqueConstraint>,
    foreign_keys: Vec<ForeignKey>,
    checks: Vec<CheckConstraint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct IndexDefinition {
    name: String,
    table: String,
    columns: Vec<String>,
    unique: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RawStatement {
    sql: String,
    dialect: SchemaDialect,
    safety_class: SafetyClass,
    result_shape: ResultShape,
}

/// A sealed schema operation. Fields stay private; build plans through `SchemaBuilder`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", content = "definition", rename_all = "snake_case")]
enum SchemaOperation {
    CreateTable(TableDefinition),
    DropTable { name: String, if_exists: bool },
    CreateIndex(IndexDefinition),
    DropIndex { name: String, table: String },
    RawSql(RawStatement),
}

/// Immutable, validated AST for one direction of a declarative migration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SchemaPlan {
    operations: Vec<SchemaOperation>,
}

impl SchemaPlan {
    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    /// Compiles the immutable AST offline and fingerprints both AST and emitted SQL.
    pub fn compile(&self, dialect: SchemaDialect) -> Result<CompiledSchemaPlan> {
        let ast_bytes = serde_json::to_vec(&self.operations)?;
        let ast_fingerprint = fingerprint(
            AST_FINGERPRINT_DOMAIN,
            dialect.label().as_bytes(),
            &ast_bytes,
        );

        let mut statements = Vec::with_capacity(self.operations.len());
        for operation in &self.operations {
            statements.push(compile_operation(operation, dialect)?);
        }

        let mut sql_hasher = Sha256::new();
        sql_hasher.update(SQL_FINGERPRINT_DOMAIN);
        update_length_prefixed(&mut sql_hasher, dialect.label().as_bytes());
        update_length_prefixed(&mut sql_hasher, SCHEMA_COMPILER_VERSION.as_bytes());
        for statement in &statements {
            update_length_prefixed(&mut sql_hasher, statement.sql.as_bytes());
            update_length_prefixed(&mut sql_hasher, statement.safety_class.label());
            update_length_prefixed(&mut sql_hasher, statement.result_shape.label());
        }
        let sql_fingerprint = format!(
            "sha256:schema-sql:v1:{}",
            hex_digest(sql_hasher.finalize().as_slice())
        );

        Ok(CompiledSchemaPlan {
            dialect,
            compiler_version: SCHEMA_COMPILER_VERSION,
            ast_fingerprint,
            sql_fingerprint,
            statements,
        })
    }
}

/// Output of a pure dialect compilation; it does not connect to or mutate a database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledSchemaPlan {
    dialect: SchemaDialect,
    compiler_version: &'static str,
    ast_fingerprint: String,
    sql_fingerprint: String,
    statements: Vec<CompiledStatement>,
}

impl CompiledSchemaPlan {
    pub fn dialect(&self) -> SchemaDialect {
        self.dialect
    }

    pub fn compiler_version(&self) -> &'static str {
        self.compiler_version
    }

    pub fn ast_fingerprint(&self) -> &str {
        &self.ast_fingerprint
    }

    pub fn sql_fingerprint(&self) -> &str {
        &self.sql_fingerprint
    }

    pub fn statements(&self) -> &[CompiledStatement] {
        &self.statements
    }
}

/// One SQL statement and its declared execution characteristics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledStatement {
    sql: String,
    safety_class: SafetyClass,
    result_shape: ResultShape,
}

impl CompiledStatement {
    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn safety_class(&self) -> SafetyClass {
        self.safety_class
    }

    pub fn result_shape(&self) -> ResultShape {
        self.result_shape
    }
}

/// Mutable builder. `finish` consumes it and prevents later AST mutation.
#[derive(Debug, Default)]
pub struct SchemaBuilder {
    operations: Vec<SchemaOperation>,
}

impl SchemaBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_table<F>(&mut self, name: impl Into<String>, configure: F) -> Result<&mut Self>
    where
        F: FnOnce(&mut TableBuilder) -> Result<()>,
    {
        let mut table = TableBuilder::new(name.into())?;
        configure(&mut table)?;
        self.operations
            .push(SchemaOperation::CreateTable(table.finish()?));
        Ok(self)
    }

    pub fn drop_table(&mut self, name: impl Into<String>, if_exists: bool) -> Result<&mut Self> {
        let name = name.into();
        validate_identifier(&name, "table")?;
        self.operations
            .push(SchemaOperation::DropTable { name, if_exists });
        Ok(self)
    }

    pub fn create_index(
        &mut self,
        name: impl Into<String>,
        table: impl Into<String>,
        columns: &[&str],
        unique: bool,
    ) -> Result<&mut Self> {
        let name = name.into();
        let table = table.into();
        validate_identifier(&name, "index")?;
        validate_identifier(&table, "indexed table")?;
        let columns = validated_identifier_list(columns, "index column")?;
        if columns.is_empty() {
            return Err(invalid_dsl("an index must contain at least one column"));
        }
        self.operations
            .push(SchemaOperation::CreateIndex(IndexDefinition {
                name,
                table,
                columns,
                unique,
            }));
        Ok(self)
    }

    /// `table` is required for MySQL/MariaDB and ignored by SQLite/PostgreSQL compilation.
    pub fn drop_index(
        &mut self,
        name: impl Into<String>,
        table: impl Into<String>,
    ) -> Result<&mut Self> {
        let name = name.into();
        let table = table.into();
        validate_identifier(&name, "index")?;
        validate_identifier(&table, "indexed table")?;
        self.operations
            .push(SchemaOperation::DropIndex { name, table });
        Ok(self)
    }

    /// Adds an opaque single statement with an explicit dialect, safety class, and result shape.
    pub fn raw_sql(
        &mut self,
        sql: impl Into<String>,
        dialect: SchemaDialect,
        safety_class: SafetyClass,
        result_shape: ResultShape,
    ) -> Result<&mut Self> {
        let sql = sql.into();
        let statements = split_sql_statements_for_dialect(&sql, dialect.sql_dialect());
        if statements.len() != 1 {
            return Err(invalid_dsl(
                "raw_sql requires exactly one non-empty statement for its declared dialect",
            ));
        }
        self.operations.push(SchemaOperation::RawSql(RawStatement {
            sql,
            dialect,
            safety_class,
            result_shape,
        }));
        Ok(self)
    }

    pub fn finish(self) -> Result<SchemaPlan> {
        Ok(SchemaPlan {
            operations: self.operations,
        })
    }
}

#[derive(Debug)]
pub struct TableBuilder {
    table: TableDefinition,
}

impl TableBuilder {
    fn new(name: String) -> Result<Self> {
        validate_identifier(&name, "table")?;
        Ok(Self {
            table: TableDefinition {
                name,
                columns: Vec::new(),
                primary_key: None,
                unique_constraints: Vec::new(),
                foreign_keys: Vec::new(),
                checks: Vec::new(),
            },
        })
    }

    pub fn column(
        &mut self,
        name: impl Into<String>,
        column_type: ColumnType,
    ) -> Result<ColumnBuilder<'_>> {
        let name = name.into();
        validate_identifier(&name, "column")?;
        validate_column_type(&column_type)?;
        if self
            .table
            .columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(&name))
        {
            return Err(invalid_dsl(format!("duplicate column `{name}`")));
        }
        self.table.columns.push(ColumnDefinition {
            name,
            column_type,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
        });
        Ok(ColumnBuilder {
            column: self
                .table
                .columns
                .last_mut()
                .expect("column was just inserted"),
        })
    }

    pub fn primary_key(&mut self, columns: &[&str]) -> Result<&mut Self> {
        if self.table.primary_key.is_some() {
            return Err(invalid_dsl("table already has a primary key"));
        }
        let columns = validated_identifier_list(columns, "primary-key column")?;
        if columns.is_empty() {
            return Err(invalid_dsl(
                "a primary key must contain at least one column",
            ));
        }
        self.table.primary_key = Some(columns);
        Ok(self)
    }

    pub fn unique_constraint(
        &mut self,
        name: impl Into<String>,
        columns: &[&str],
    ) -> Result<&mut Self> {
        let name = name.into();
        validate_identifier(&name, "unique constraint")?;
        let columns = validated_identifier_list(columns, "unique-constraint column")?;
        if columns.is_empty() {
            return Err(invalid_dsl(
                "a unique constraint must contain at least one column",
            ));
        }
        self.table
            .unique_constraints
            .push(UniqueConstraint { name, columns });
        Ok(self)
    }

    pub fn foreign_key(&mut self, foreign_key: ForeignKey) -> Result<&mut Self> {
        self.table.foreign_keys.push(foreign_key);
        Ok(self)
    }

    pub fn check(&mut self, check: CheckConstraint) -> Result<&mut Self> {
        self.table.checks.push(check);
        Ok(self)
    }

    fn finish(self) -> Result<TableDefinition> {
        if self.table.columns.is_empty() {
            return Err(invalid_dsl(format!(
                "table `{}` must declare at least one column",
                self.table.name
            )));
        }

        let column_names = self
            .table
            .columns
            .iter()
            .map(|column| column.name.to_ascii_lowercase())
            .collect::<std::collections::HashSet<_>>();
        let inline_primary_keys = self
            .table
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .count();
        if inline_primary_keys > 1 || (inline_primary_keys > 0 && self.table.primary_key.is_some())
        {
            return Err(invalid_dsl(format!(
                "table `{}` declares more than one primary key",
                self.table.name
            )));
        }
        if let Some(columns) = &self.table.primary_key {
            validate_existing_columns(columns, &column_names, "primary key")?;
        }
        for constraint in &self.table.unique_constraints {
            validate_existing_columns(&constraint.columns, &column_names, "unique constraint")?;
        }
        let mut constraint_names = std::collections::HashSet::new();
        for constraint_name in self
            .table
            .unique_constraints
            .iter()
            .map(|constraint| &constraint.name)
            .chain(self.table.checks.iter().map(|check| &check.name))
        {
            if !constraint_names.insert(constraint_name.to_ascii_lowercase()) {
                return Err(invalid_dsl(format!(
                    "table `{}` repeats constraint name `{constraint_name}`",
                    self.table.name
                )));
            }
        }
        for foreign_key in &self.table.foreign_keys {
            validate_existing_columns(&foreign_key.columns, &column_names, "foreign key")?;
        }
        for check in &self.table.checks {
            validate_check_columns(&check.expression.node, &column_names)?;
        }
        Ok(self.table)
    }
}

pub struct ColumnBuilder<'a> {
    column: &'a mut ColumnDefinition,
}

impl ColumnBuilder<'_> {
    pub fn not_null(&mut self) -> &mut Self {
        self.column.not_null = true;
        self
    }

    pub fn primary_key(&mut self) -> &mut Self {
        self.column.primary_key = true;
        self.column.not_null = true;
        self
    }

    pub fn unique(&mut self) -> &mut Self {
        self.column.unique = true;
        self
    }

    pub fn default(&mut self, value: ColumnDefault) -> Result<&mut Self> {
        if self.column.default.is_some() {
            return Err(invalid_dsl(format!(
                "column `{}` already has a default",
                self.column.name
            )));
        }
        validate_column_default(&self.column.column_type, &value)?;
        self.column.default = Some(value);
        Ok(self)
    }
}

/// A declarative migration requires both directions; no inverse is synthesized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaMigration {
    up: SchemaPlan,
    down: SchemaPlan,
}

impl SchemaMigration {
    pub fn new(up: SchemaPlan, down: SchemaPlan) -> Result<Self> {
        if up.operations.is_empty() {
            return Err(invalid_dsl("declarative migration Up plan cannot be empty"));
        }
        if down.operations.is_empty() {
            return Err(invalid_dsl(
                "declarative migration Down plan must be declared and cannot be empty",
            ));
        }
        Ok(Self { up, down })
    }

    pub fn up(&self) -> &SchemaPlan {
        &self.up
    }

    pub fn down(&self) -> &SchemaPlan {
        &self.down
    }
}

fn compile_operation(
    operation: &SchemaOperation,
    dialect: SchemaDialect,
) -> Result<CompiledStatement> {
    let sql = match operation {
        SchemaOperation::CreateTable(table) => compile_create_table(table, dialect)?,
        SchemaOperation::DropTable { name, if_exists } => {
            let clause = if *if_exists { " IF EXISTS" } else { "" };
            format!("DROP TABLE{clause} {}", quote_identifier(name, dialect))
        }
        SchemaOperation::CreateIndex(index) => compile_create_index(index, dialect),
        SchemaOperation::DropIndex { name, table } => {
            if dialect.quotes_identifiers_with_backticks() {
                format!(
                    "DROP INDEX {} ON {}",
                    quote_identifier(name, dialect),
                    quote_identifier(table, dialect)
                )
            } else {
                format!("DROP INDEX {}", quote_identifier(name, dialect))
            }
        }
        SchemaOperation::RawSql(raw) => {
            if raw.dialect != dialect {
                return Err(unsupported(format!(
                    "raw SQL declares dialect `{}` but the plan targets `{}`",
                    raw.dialect.label(),
                    dialect.label()
                )));
            }
            return Ok(CompiledStatement {
                sql: raw.sql.clone(),
                safety_class: raw.safety_class,
                result_shape: raw.result_shape,
            });
        }
    };
    Ok(CompiledStatement {
        sql,
        safety_class: dialect.ddl_safety(),
        result_shape: ResultShape::NoRows,
    })
}

fn compile_create_table(table: &TableDefinition, dialect: SchemaDialect) -> Result<String> {
    let mut definitions = Vec::new();
    for column in &table.columns {
        definitions.push(compile_column(column, dialect)?);
    }
    if let Some(columns) = &table.primary_key {
        definitions.push(format!(
            "PRIMARY KEY ({})",
            quote_identifier_list(columns, dialect)
        ));
    }
    for constraint in &table.unique_constraints {
        definitions.push(format!(
            "CONSTRAINT {} UNIQUE ({})",
            quote_identifier(&constraint.name, dialect),
            quote_identifier_list(&constraint.columns, dialect)
        ));
    }
    for foreign_key in &table.foreign_keys {
        definitions.push(compile_foreign_key(foreign_key, dialect)?);
    }
    for check in &table.checks {
        definitions.push(format!(
            "CONSTRAINT {} CHECK ({})",
            quote_identifier(&check.name, dialect),
            compile_check_expression(&check.expression.node, dialect)?
        ));
    }
    Ok(format!(
        "CREATE TABLE {} ({})",
        quote_identifier(&table.name, dialect),
        definitions.join(", ")
    ))
}

fn compile_column(column: &ColumnDefinition, dialect: SchemaDialect) -> Result<String> {
    let (type_sql, type_check) = compile_column_type(&column.column_type, &column.name, dialect)?;
    let mut definition = format!("{} {type_sql}", quote_identifier(&column.name, dialect));
    if column.primary_key {
        definition.push_str(" PRIMARY KEY");
    }
    if column.not_null {
        definition.push_str(" NOT NULL");
    }
    if column.unique {
        definition.push_str(" UNIQUE");
    }
    if let Some(default) = &column.default {
        definition.push_str(" DEFAULT ");
        definition.push_str(&compile_default(default, dialect)?);
    }
    if let Some(check) = type_check {
        definition.push_str(" CHECK (");
        definition.push_str(&check);
        definition.push(')');
    }
    Ok(definition)
}

fn compile_column_type(
    column_type: &ColumnType,
    column_name: &str,
    dialect: SchemaDialect,
) -> Result<(String, Option<String>)> {
    let compiled = match column_type {
        ColumnType::Integer => match dialect {
            SchemaDialect::Sqlite | SchemaDialect::Postgres => "INTEGER".to_owned(),
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "INT".to_owned(),
        },
        ColumnType::BigInt => match dialect {
            SchemaDialect::Sqlite => "INTEGER".to_owned(),
            SchemaDialect::Postgres | SchemaDialect::Mysql | SchemaDialect::MariaDb => {
                "BIGINT".to_owned()
            }
        },
        ColumnType::Varchar(length) => format!("VARCHAR({length})"),
        ColumnType::Text => "TEXT".to_owned(),
        ColumnType::Boolean => match dialect {
            SchemaDialect::Sqlite => {
                let column = quote_identifier(column_name, dialect);
                return Ok(("INTEGER".to_owned(), Some(format!("{column} IN (0, 1)"))));
            }
            SchemaDialect::Postgres => "BOOLEAN".to_owned(),
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "BOOLEAN".to_owned(),
        },
        ColumnType::Decimal { precision, scale } => match dialect {
            SchemaDialect::Sqlite => {
                return Err(unsupported(
                    "SQLite does not guarantee fixed-precision decimal semantics; choose an explicit text representation",
                ));
            }
            SchemaDialect::Postgres | SchemaDialect::Mysql | SchemaDialect::MariaDb => {
                format!("DECIMAL({precision}, {scale})")
            }
        },
        ColumnType::Timestamp => match dialect {
            SchemaDialect::Sqlite => "TEXT".to_owned(),
            SchemaDialect::Postgres => "TIMESTAMP(6) WITHOUT TIME ZONE".to_owned(),
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "DATETIME(6)".to_owned(),
        },
        ColumnType::TimestampWithTimeZone => match dialect {
            SchemaDialect::Postgres => "TIMESTAMP(6) WITH TIME ZONE".to_owned(),
            SchemaDialect::Sqlite => {
                return Err(unsupported(
                    "SQLite has no timezone-aware timestamp type; choose an explicit text representation",
                ));
            }
            SchemaDialect::Mysql | SchemaDialect::MariaDb => {
                return Err(unsupported(format!(
                    "{} has no timestamp-with-time-zone type with PostgreSQL-equivalent semantics",
                    dialect.label()
                )));
            }
        },
        ColumnType::Binary => match dialect {
            SchemaDialect::Sqlite => "BLOB".to_owned(),
            SchemaDialect::Postgres => "BYTEA".to_owned(),
            SchemaDialect::Mysql => "LONGBLOB".to_owned(),
            SchemaDialect::MariaDb => "LONGBLOB".to_owned(),
        },
        ColumnType::Json(JsonStorage::Text) => match dialect {
            SchemaDialect::Sqlite | SchemaDialect::Postgres => "TEXT".to_owned(),
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "LONGTEXT".to_owned(),
        },
        ColumnType::Json(JsonStorage::Native) => match dialect {
            SchemaDialect::Sqlite => {
                let column = quote_identifier(column_name, dialect);
                return Ok(("TEXT".to_owned(), Some(format!("json_valid({column})"))));
            }
            SchemaDialect::Postgres => "JSONB".to_owned(),
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "JSON".to_owned(),
        },
        ColumnType::Uuid(UuidStorage::Native) => match dialect {
            SchemaDialect::Postgres => "UUID".to_owned(),
            SchemaDialect::MariaDb => "UUID".to_owned(),
            SchemaDialect::Sqlite | SchemaDialect::Mysql => {
                return Err(unsupported(format!(
                    "native UUID storage is not supported by `{}`; choose char36 or binary16 explicitly",
                    dialect.label()
                )));
            }
        },
        ColumnType::Uuid(UuidStorage::Char36) => "CHAR(36)".to_owned(),
        ColumnType::Uuid(UuidStorage::Binary16) => match dialect {
            SchemaDialect::Sqlite => {
                let column = quote_identifier(column_name, dialect);
                return Ok(("BLOB".to_owned(), Some(format!("length({column}) = 16"))));
            }
            SchemaDialect::Postgres => {
                let column = quote_identifier(column_name, dialect);
                return Ok((
                    "BYTEA".to_owned(),
                    Some(format!("octet_length({column}) = 16")),
                ));
            }
            SchemaDialect::Mysql | SchemaDialect::MariaDb => "BINARY(16)".to_owned(),
        },
    };
    Ok((compiled, None))
}

fn compile_default(default: &ColumnDefault, dialect: SchemaDialect) -> Result<String> {
    match default {
        ColumnDefault::Literal(value) => compile_literal(value, dialect),
        ColumnDefault::CurrentTimestamp => Ok("CURRENT_TIMESTAMP".to_owned()),
    }
}

fn compile_literal(value: &LiteralValue, dialect: SchemaDialect) -> Result<String> {
    match value {
        LiteralValue::Null => Ok("NULL".to_owned()),
        LiteralValue::Boolean(value) => match dialect {
            SchemaDialect::Sqlite => Ok(if *value { "1" } else { "0" }.to_owned()),
            SchemaDialect::Postgres | SchemaDialect::Mysql | SchemaDialect::MariaDb => {
                Ok(if *value { "TRUE" } else { "FALSE" }.to_owned())
            }
        },
        LiteralValue::Integer(value) => Ok(value.to_string()),
        LiteralValue::Decimal(value) => {
            validate_decimal_literal(value)?;
            Ok(value.clone())
        }
        LiteralValue::String(value) => {
            validate_string_literal(value, dialect)?;
            Ok(format!("'{}'", value.replace('\'', "''")))
        }
    }
}

fn validate_column_default(column_type: &ColumnType, default: &ColumnDefault) -> Result<()> {
    let ColumnDefault::Literal(value) = default else {
        return match column_type {
            ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => Ok(()),
            _ => Err(invalid_dsl(
                "CURRENT_TIMESTAMP default requires a timestamp column",
            )),
        };
    };
    validate_literal(value)?;
    match (column_type, value) {
        (_, LiteralValue::Null) => Ok(()),
        (ColumnType::Boolean, LiteralValue::Boolean(_)) => Ok(()),
        (ColumnType::Integer | ColumnType::BigInt, LiteralValue::Integer(_)) => Ok(()),
        (ColumnType::Decimal { precision, scale }, LiteralValue::Integer(value)) => {
            validate_decimal_fits(&value.to_string(), *precision, *scale)
        }
        (ColumnType::Decimal { precision, scale }, LiteralValue::Decimal(value)) => {
            validate_decimal_fits(value, *precision, *scale)
        }
        (ColumnType::Varchar(length), LiteralValue::String(value)) => {
            if value.chars().count() > *length as usize {
                return Err(invalid_dsl(format!(
                    "default value exceeds VARCHAR({length}) length"
                )));
            }
            Ok(())
        }
        (
            ColumnType::Text | ColumnType::Timestamp | ColumnType::TimestampWithTimeZone,
            LiteralValue::String(_),
        ) => Ok(()),
        (ColumnType::Json(JsonStorage::Text), LiteralValue::String(_)) => Ok(()),
        (ColumnType::Json(JsonStorage::Native), LiteralValue::String(value)) => {
            serde_json::from_str::<serde_json::Value>(value).map_err(|error| {
                invalid_dsl(format!("native JSON default is not valid JSON: {error}"))
            })?;
            Ok(())
        }
        (
            ColumnType::Uuid(UuidStorage::Native | UuidStorage::Char36),
            LiteralValue::String(value),
        ) => {
            let parsed = uuid::Uuid::parse_str(value)
                .map_err(|_| invalid_dsl("UUID default must use canonical hyphenated form"))?;
            if parsed.to_string() != *value {
                return Err(invalid_dsl(
                    "UUID default must use lowercase canonical hyphenated form",
                ));
            }
            Ok(())
        }
        _ => Err(invalid_dsl(format!(
            "default literal type does not match column type {column_type:?}"
        ))),
    }
}

fn compile_foreign_key(foreign_key: &ForeignKey, dialect: SchemaDialect) -> Result<String> {
    if matches!(dialect, SchemaDialect::Mysql | SchemaDialect::MariaDb)
        && (foreign_key.on_delete == Some(ForeignKeyAction::SetDefault)
            || foreign_key.on_update == Some(ForeignKeyAction::SetDefault))
    {
        return Err(unsupported(format!(
            "{} InnoDB does not support SET DEFAULT foreign-key actions",
            dialect.label()
        )));
    }
    let mut sql = format!(
        "FOREIGN KEY ({}) REFERENCES {} ({})",
        quote_identifier_list(&foreign_key.columns, dialect),
        quote_identifier(&foreign_key.referenced_table, dialect),
        quote_identifier_list(&foreign_key.referenced_columns, dialect)
    );
    if let Some(action) = foreign_key.on_delete {
        sql.push_str(" ON DELETE ");
        sql.push_str(action.sql());
    }
    if let Some(action) = foreign_key.on_update {
        sql.push_str(" ON UPDATE ");
        sql.push_str(action.sql());
    }
    Ok(sql)
}

impl ForeignKeyAction {
    fn sql(self) -> &'static str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        }
    }
}

fn compile_create_index(index: &IndexDefinition, dialect: SchemaDialect) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    format!(
        "CREATE {unique}INDEX {} ON {} ({})",
        quote_identifier(&index.name, dialect),
        quote_identifier(&index.table, dialect),
        quote_identifier_list(&index.columns, dialect)
    )
}

fn compile_check_expression(node: &CheckNode, dialect: SchemaDialect) -> Result<String> {
    match node {
        CheckNode::Column { name } => Ok(quote_identifier(name, dialect)),
        CheckNode::Value { value } => compile_literal(value, dialect),
        CheckNode::Compare {
            left,
            operator,
            right,
        } => Ok(format!(
            "({} {} {})",
            compile_check_expression(left, dialect)?,
            operator.sql(),
            compile_check_expression(right, dialect)?
        )),
        CheckNode::And { left, right } => Ok(format!(
            "({} AND {})",
            compile_check_expression(left, dialect)?,
            compile_check_expression(right, dialect)?
        )),
        CheckNode::Or { left, right } => Ok(format!(
            "({} OR {})",
            compile_check_expression(left, dialect)?,
            compile_check_expression(right, dialect)?
        )),
        CheckNode::Not { expression } => Ok(format!(
            "(NOT {})",
            compile_check_expression(expression, dialect)?
        )),
        CheckNode::IsNull {
            expression,
            negated,
        } => Ok(format!(
            "({} IS {}NULL)",
            compile_check_expression(expression, dialect)?,
            if *negated { "NOT " } else { "" }
        )),
    }
}

impl ComparisonOperator {
    fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
            Self::Like => "LIKE",
        }
    }
}

fn validate_column_type(column_type: &ColumnType) -> Result<()> {
    match column_type {
        ColumnType::Varchar(0) => Err(invalid_dsl("VARCHAR length must be greater than zero")),
        ColumnType::Decimal { precision, scale }
            if *precision == 0 || *precision > 65 || scale > precision =>
        {
            Err(invalid_dsl(
                "DECIMAL precision must be 1..=65 and scale cannot exceed precision",
            ))
        }
        _ => Ok(()),
    }
}

fn validate_literal(value: &LiteralValue) -> Result<()> {
    match value {
        LiteralValue::Decimal(value) => validate_decimal_literal(value),
        LiteralValue::String(value) => {
            if value.contains('\0') {
                return Err(invalid_dsl("string literals cannot contain NUL"));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_decimal_literal(value: &str) -> Result<()> {
    decimal_digit_counts(value).map(|_| ())
}

fn validate_decimal_fits(value: &str, precision: u8, scale: u8) -> Result<()> {
    let (integer_digits, fractional_digits) = decimal_digit_counts(value)?;
    if fractional_digits > usize::from(scale) || integer_digits > usize::from(precision - scale) {
        return Err(invalid_dsl(format!(
            "decimal default `{value}` exceeds DECIMAL({precision}, {scale})"
        )));
    }
    Ok(())
}

fn decimal_digit_counts(value: &str) -> Result<(usize, usize)> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(invalid_dsl("decimal literal cannot be empty"));
    }
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut integer_digits = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        integer_digits += 1;
        index += 1;
    }
    let mut fractional_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            fractional_digits += 1;
            index += 1;
        }
    }
    if index != bytes.len() || integer_digits + fractional_digits == 0 {
        return Err(invalid_dsl(format!("invalid decimal literal `{value}`")));
    }
    Ok((integer_digits, fractional_digits))
}

fn validate_string_literal(value: &str, dialect: SchemaDialect) -> Result<()> {
    validate_literal(&LiteralValue::String(value.to_owned()))?;
    if dialect.quotes_identifiers_with_backticks() && value.contains('\\') {
        return Err(unsupported(
            "MySQL/MariaDB string defaults containing backslash are rejected because SQL mode changes their meaning",
        ));
    }
    Ok(())
}

fn validate_existing_columns(
    columns: &[String],
    existing: &std::collections::HashSet<String>,
    constraint_kind: &str,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for column in columns {
        let canonical = column.to_ascii_lowercase();
        if !existing.contains(&canonical) {
            return Err(invalid_dsl(format!(
                "{constraint_kind} references undeclared column `{column}`"
            )));
        }
        if !seen.insert(canonical) {
            return Err(invalid_dsl(format!(
                "{constraint_kind} repeats column `{column}`"
            )));
        }
    }
    Ok(())
}

fn validate_check_columns(
    node: &CheckNode,
    existing: &std::collections::HashSet<String>,
) -> Result<()> {
    match node {
        CheckNode::Column { name } => {
            if !existing.contains(&name.to_ascii_lowercase()) {
                return Err(invalid_dsl(format!(
                    "check constraint references undeclared column `{name}`"
                )));
            }
        }
        CheckNode::Value { value } => validate_literal(value)?,
        CheckNode::Compare { left, right, .. }
        | CheckNode::And { left, right }
        | CheckNode::Or { left, right } => {
            validate_check_columns(left, existing)?;
            validate_check_columns(right, existing)?;
        }
        CheckNode::Not { expression } | CheckNode::IsNull { expression, .. } => {
            validate_check_columns(expression, existing)?;
        }
    }
    Ok(())
}

fn validated_identifier_list(values: &[&str], kind: &str) -> Result<Vec<String>> {
    let mut output = Vec::with_capacity(values.len());
    let mut seen = std::collections::HashSet::new();
    for value in values {
        validate_identifier(value, kind)?;
        if !seen.insert(value.to_ascii_lowercase()) {
            return Err(invalid_dsl(format!("{kind} `{value}` is repeated")));
        }
        output.push((*value).to_owned());
    }
    Ok(output)
}

fn validate_identifier(identifier: &str, kind: &str) -> Result<()> {
    let mut bytes = identifier.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    let valid_rest = bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_first || !valid_rest || identifier.len() > MAX_PORTABLE_IDENTIFIER_BYTES {
        return Err(invalid_dsl(format!(
            "{kind} identifier `{identifier}` must match [A-Za-z_][A-Za-z0-9_]* and contain at most {MAX_PORTABLE_IDENTIFIER_BYTES} ASCII bytes"
        )));
    }
    Ok(())
}

fn quote_identifier(identifier: &str, dialect: SchemaDialect) -> String {
    if dialect.quotes_identifiers_with_backticks() {
        format!("`{identifier}`")
    } else {
        format!("\"{identifier}\"")
    }
}

fn quote_identifier_list(identifiers: &[String], dialect: SchemaDialect) -> String {
    let mut quoted = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        quoted.push(quote_identifier(identifier, dialect));
    }
    quoted.join(", ")
}

fn fingerprint(domain: &[u8], dialect: &[u8], payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    update_length_prefixed(&mut hasher, dialect);
    update_length_prefixed(&mut hasher, payload);
    format!(
        "sha256:schema-ast:v1:{}",
        hex_digest(hasher.finalize().as_slice())
    )
}

fn update_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn invalid_dsl(message: impl Into<String>) -> DbmError {
    DbmError::Configuration(format!("schema DSL: {}", message.into()))
}

fn unsupported(message: impl Into<String>) -> DbmError {
    DbmError::Unsupported(format!("schema DSL: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::{
        CheckConstraint, CheckExpression, ColumnDefault, ColumnType, ComparisonOperator,
        ForeignKey, ForeignKeyAction, JsonStorage, LiteralValue, ResultShape, SafetyClass,
        SchemaBuilder, SchemaDialect, SchemaMigration, UuidStorage,
    };

    fn users_up_plan() -> crate::Result<super::SchemaPlan> {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("users", |table| {
                table
                    .column("id", ColumnType::BigInt)?
                    .primary_key()
                    .not_null();
                table
                    .column("email", ColumnType::Varchar(255))?
                    .not_null()
                    .unique();
                table
                    .column("active", ColumnType::Boolean)?
                    .not_null()
                    .default(ColumnDefault::Literal(LiteralValue::Boolean(true)))?;
                table.check(CheckConstraint::new(
                    "users_email_nonempty",
                    CheckExpression::column("email")?.compare(
                        ComparisonOperator::NotEqual,
                        CheckExpression::value(LiteralValue::String(String::new()))?,
                    )?,
                )?)?;
                Ok(())
            })?
            .create_index("users_email_idx", "users", &["email"], true)?;
        builder.finish()
    }

    #[test]
    fn compiles_create_table_and_index_for_each_database_dialect() {
        let plan = users_up_plan().expect("build users schema plan");
        let expected = [
            (
                SchemaDialect::Sqlite,
                "CREATE TABLE \"users\" (\"id\" INTEGER PRIMARY KEY NOT NULL, \"email\" VARCHAR(255) NOT NULL UNIQUE, \"active\" INTEGER NOT NULL DEFAULT 1 CHECK (\"active\" IN (0, 1)), CONSTRAINT \"users_email_nonempty\" CHECK ((\"email\" <> '')))".to_owned(),
                SafetyClass::Transactional,
            ),
            (
                SchemaDialect::Postgres,
                "CREATE TABLE \"users\" (\"id\" BIGINT PRIMARY KEY NOT NULL, \"email\" VARCHAR(255) NOT NULL UNIQUE, \"active\" BOOLEAN NOT NULL DEFAULT TRUE, CONSTRAINT \"users_email_nonempty\" CHECK ((\"email\" <> '')))".to_owned(),
                SafetyClass::Transactional,
            ),
            (
                SchemaDialect::Mysql,
                "CREATE TABLE `users` (`id` BIGINT PRIMARY KEY NOT NULL, `email` VARCHAR(255) NOT NULL UNIQUE, `active` BOOLEAN NOT NULL DEFAULT TRUE, CONSTRAINT `users_email_nonempty` CHECK ((`email` <> '')))".to_owned(),
                SafetyClass::NonTransactional,
            ),
            (
                SchemaDialect::MariaDb,
                "CREATE TABLE `users` (`id` BIGINT PRIMARY KEY NOT NULL, `email` VARCHAR(255) NOT NULL UNIQUE, `active` BOOLEAN NOT NULL DEFAULT TRUE, CONSTRAINT `users_email_nonempty` CHECK ((`email` <> '')))".to_owned(),
                SafetyClass::NonTransactional,
            ),
        ];
        for (dialect, expected_create, safety) in expected {
            let compiled = plan.compile(dialect).expect("compile schema plan");
            assert_eq!(compiled.statements()[0].sql(), expected_create);
            assert_eq!(
                compiled.statements()[0].safety_class(),
                safety,
                "DDL execution class for {dialect:?}"
            );
            assert_eq!(
                compiled.statements()[1].sql(),
                match dialect {
                    SchemaDialect::Sqlite | SchemaDialect::Postgres => {
                        "CREATE UNIQUE INDEX \"users_email_idx\" ON \"users\" (\"email\")"
                    }
                    SchemaDialect::Mysql | SchemaDialect::MariaDb => {
                        "CREATE UNIQUE INDEX `users_email_idx` ON `users` (`email`)"
                    }
                }
            );
        }
    }

    #[test]
    fn requires_explicit_nonempty_up_and_down_plans() {
        let up = users_up_plan().expect("build Up plan");
        let mut down_builder = SchemaBuilder::new();
        down_builder
            .drop_table("users", false)
            .expect("declare explicit Down");
        let down = down_builder.finish().expect("finish Down");
        let migration = SchemaMigration::new(up, down).expect("explicit directions");
        assert_eq!(migration.up().operation_count(), 2);
        assert_eq!(migration.down().operation_count(), 1);

        let empty = SchemaBuilder::new().finish().expect("empty plan builder");
        assert!(SchemaMigration::new(empty.clone(), empty).is_err());
    }

    #[test]
    fn fingerprints_are_stable_versioned_and_distinguish_dialect_and_direction() {
        let first = users_up_plan()
            .expect("build first plan")
            .compile(SchemaDialect::Postgres)
            .expect("compile first plan");
        let same = users_up_plan()
            .expect("build same plan")
            .compile(SchemaDialect::Postgres)
            .expect("compile identical plan");
        let other_dialect = users_up_plan()
            .expect("build other dialect plan")
            .compile(SchemaDialect::Mysql)
            .expect("compile same AST for MySQL");
        assert_eq!(first.ast_fingerprint(), same.ast_fingerprint());
        assert_eq!(first.sql_fingerprint(), same.sql_fingerprint());
        assert_ne!(first.ast_fingerprint(), other_dialect.ast_fingerprint());
        assert_ne!(first.sql_fingerprint(), other_dialect.sql_fingerprint());
        assert!(first.ast_fingerprint().starts_with("sha256:schema-ast:v1:"));
        assert!(first.sql_fingerprint().starts_with("sha256:schema-sql:v1:"));
        assert_eq!(first.compiler_version(), "dbm-schema-compiler-v1");

        let mut down_builder = SchemaBuilder::new();
        down_builder
            .drop_table("users", false)
            .expect("declare Down operation");
        let down = down_builder
            .finish()
            .expect("finish Down plan")
            .compile(SchemaDialect::Postgres)
            .expect("compile Down plan");
        assert_ne!(first.ast_fingerprint(), down.ast_fingerprint());
        assert_ne!(first.sql_fingerprint(), down.sql_fingerprint());
    }

    #[test]
    fn rejects_nonportable_or_ambiguous_types_fail_closed() {
        for (dialect, column_type) in [
            (SchemaDialect::Sqlite, ColumnType::TimestampWithTimeZone),
            (SchemaDialect::Mysql, ColumnType::TimestampWithTimeZone),
            (SchemaDialect::MariaDb, ColumnType::TimestampWithTimeZone),
            (
                SchemaDialect::Sqlite,
                ColumnType::Decimal {
                    precision: 9,
                    scale: 2,
                },
            ),
            (SchemaDialect::Sqlite, ColumnType::Uuid(UuidStorage::Native)),
            (SchemaDialect::Mysql, ColumnType::Uuid(UuidStorage::Native)),
        ] {
            let mut builder = SchemaBuilder::new();
            builder
                .create_table("typed", |table| {
                    table.column("value", column_type)?;
                    Ok(())
                })
                .expect("build a typed table");
            let error = builder
                .finish()
                .expect("finish AST")
                .compile(dialect)
                .expect_err("unsupported semantics fail closed");
            assert!(matches!(error, crate::DbmError::Unsupported(_)));
        }
    }

    #[test]
    fn supports_explicit_native_json_and_uuid_storage() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("payloads", |table| {
                table.column("body", ColumnType::Json(JsonStorage::Native))?;
                table.column("id", ColumnType::Uuid(UuidStorage::Native))?;
                Ok(())
            })
            .expect("declare explicit native types");
        let plan = builder.finish().expect("finish native type plan");
        assert!(plan.compile(SchemaDialect::Sqlite).is_err());
        assert!(plan.compile(SchemaDialect::Mysql).is_err());
        let postgres = plan
            .compile(SchemaDialect::Postgres)
            .expect("PostgreSQL native types");
        assert!(postgres.statements()[0].sql().contains("\"body\" JSONB"));
        assert!(postgres.statements()[0].sql().contains("\"id\" UUID"));
        let mariadb = plan
            .compile(SchemaDialect::MariaDb)
            .expect("MariaDB native types");
        assert!(mariadb.statements()[0].sql().contains("`body` JSON"));
        assert!(mariadb.statements()[0].sql().contains("`id` UUID"));
    }

    #[test]
    fn explicit_uuid_storage_compiles_with_dialect_specific_constraints() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("ids", |table| {
                table.column("text_id", ColumnType::Uuid(UuidStorage::Char36))?;
                table.column("binary_id", ColumnType::Uuid(UuidStorage::Binary16))?;
                Ok(())
            })
            .expect("declare explicit UUID encodings");
        let plan = builder.finish().expect("finish UUID plan");
        let expectations = [
            (
                SchemaDialect::Sqlite,
                "CHAR(36)",
                "BLOB CHECK (length(\"binary_id\") = 16)",
            ),
            (
                SchemaDialect::Postgres,
                "CHAR(36)",
                "BYTEA CHECK (octet_length(\"binary_id\") = 16)",
            ),
            (SchemaDialect::Mysql, "CHAR(36)", "BINARY(16)"),
            (SchemaDialect::MariaDb, "CHAR(36)", "BINARY(16)"),
        ];
        for (dialect, text_type, binary_type) in expectations {
            let compiled = plan.compile(dialect).expect("compile UUID representation");
            assert!(compiled.statements()[0].sql().contains(text_type));
            assert!(compiled.statements()[0].sql().contains(binary_type));
        }
    }

    #[test]
    fn validates_identifiers_and_constraint_references_before_compilation() {
        let mut invalid_identifier = SchemaBuilder::new();
        assert!(
            invalid_identifier
                .create_table("users; DROP TABLE users", |_| Ok(()))
                .is_err()
        );

        let mut invalid_reference = SchemaBuilder::new();
        let error = invalid_reference
            .create_table("users", |table| {
                table.column("id", ColumnType::Integer)?;
                table.primary_key(&["missing"])?;
                Ok(())
            })
            .expect_err("cross-reference must fail while sealing the table");
        assert!(matches!(error, crate::DbmError::Configuration(_)));

        let mut duplicate_case = SchemaBuilder::new();
        assert!(
            duplicate_case
                .create_table("users", |table| {
                    table.column("id", ColumnType::Integer)?;
                    table.column("ID", ColumnType::Integer)?;
                    Ok(())
                })
                .is_err()
        );
    }

    #[test]
    fn compiles_foreign_keys_and_named_constraints() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("posts", |table| {
                table.column("id", ColumnType::BigInt)?.primary_key();
                table.column("author_id", ColumnType::BigInt)?.not_null();
                table.foreign_key(
                    ForeignKey::new(&["author_id"], "users", &["id"])?
                        .on_delete(ForeignKeyAction::Cascade)
                        .on_update(ForeignKeyAction::NoAction),
                )?;
                table.unique_constraint("posts_author_id_unique", &["author_id"])?;
                Ok(())
            })
            .expect("declare FK and unique constraint");
        let plan = builder.finish().expect("validate FK and unique references");
        let pg = plan
            .compile(SchemaDialect::Postgres)
            .expect("compile PostgreSQL constraints");
        assert!(
            pg.statements()[0]
                .sql()
                .contains("CONSTRAINT \"posts_author_id_unique\" UNIQUE (\"author_id\")")
        );
        assert!(pg.statements()[0].sql().contains(
            "FOREIGN KEY (\"author_id\") REFERENCES \"users\" (\"id\") ON DELETE CASCADE ON UPDATE NO ACTION"
        ));
    }

    #[test]
    fn raw_sql_is_one_statement_dialect_bound_and_explicitly_typed() {
        let mut builder = SchemaBuilder::new();
        assert!(
            builder
                .raw_sql(
                    "SELECT 1; SELECT 2",
                    SchemaDialect::Postgres,
                    SafetyClass::Transactional,
                    ResultShape::Rows,
                )
                .is_err()
        );
        builder
            .raw_sql(
                "SELECT ';not a separator'",
                SchemaDialect::Postgres,
                SafetyClass::Transactional,
                ResultShape::Rows,
            )
            .expect("single statement with a quoted delimiter");
        let plan = builder.finish().expect("finish raw statement plan");
        let compiled = plan
            .compile(SchemaDialect::Postgres)
            .expect("compile matching dialect");
        assert_eq!(compiled.statements()[0].result_shape(), ResultShape::Rows);
        assert_eq!(
            compiled.statements()[0].safety_class(),
            SafetyClass::Transactional
        );
        assert!(plan.compile(SchemaDialect::Mysql).is_err());
    }

    #[test]
    fn string_literals_are_quoted_and_sql_mode_sensitive_backslashes_fail_closed() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("labels", |table| {
                table
                    .column("value", ColumnType::Text)?
                    .default(ColumnDefault::Literal(LiteralValue::String(
                        "owner's label".to_owned(),
                    )))?;
                Ok(())
            })
            .expect("build escaped string default");
        let plan = builder.finish().expect("finish string plan");
        assert!(
            plan.compile(SchemaDialect::Postgres)
                .expect("compile string literal")
                .statements()[0]
                .sql()
                .contains("DEFAULT 'owner''s label'")
        );

        let mut backslash_builder = SchemaBuilder::new();
        backslash_builder
            .create_table("labels", |table| {
                table
                    .column("value", ColumnType::Text)?
                    .default(ColumnDefault::Literal(LiteralValue::String(
                        "path\\name".to_owned(),
                    )))?;
                Ok(())
            })
            .expect("build SQL mode-sensitive literal");
        let error = backslash_builder
            .finish()
            .expect("finish AST")
            .compile(SchemaDialect::Mysql)
            .expect_err("MySQL SQL mode ambiguity fails closed");
        assert!(matches!(error, crate::DbmError::Unsupported(_)));
    }

    #[test]
    fn defaults_are_type_checked_and_decimal_precision_is_bounded() {
        let mut mismatched = SchemaBuilder::new();
        let error =
            mismatched
                .create_table("mismatched", |table| {
                    table.column("enabled", ColumnType::Boolean)?.default(
                        ColumnDefault::Literal(LiteralValue::String("true".to_owned())),
                    )?;
                    Ok(())
                })
                .expect_err("default literal must match its column type");
        assert!(matches!(error, crate::DbmError::Configuration(_)));

        let mut too_many_digits = SchemaBuilder::new();
        let error = too_many_digits
            .create_table("amounts", |table| {
                table
                    .column(
                        "amount",
                        ColumnType::Decimal {
                            precision: 5,
                            scale: 2,
                        },
                    )?
                    .default(ColumnDefault::Literal(LiteralValue::Decimal(
                        "1234.56".to_owned(),
                    )))?;
                Ok(())
            })
            .expect_err("decimal default must fit precision and scale");
        assert!(matches!(error, crate::DbmError::Configuration(_)));

        let mut current_time_on_text = SchemaBuilder::new();
        let error = current_time_on_text
            .create_table("labels", |table| {
                table
                    .column("label", ColumnType::Text)?
                    .default(ColumnDefault::CurrentTimestamp)?;
                Ok(())
            })
            .expect_err("CURRENT_TIMESTAMP is restricted to timestamp columns");
        assert!(matches!(error, crate::DbmError::Configuration(_)));
    }

    #[test]
    fn rejects_unsupported_mysql_foreign_key_action() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("children", |table| {
                table.column("parent_id", ColumnType::BigInt)?;
                table.foreign_key(
                    ForeignKey::new(&["parent_id"], "parents", &["id"])?
                        .on_delete(ForeignKeyAction::SetDefault),
                )?;
                Ok(())
            })
            .expect("declare supported AST action before dialect compilation");
        let plan = builder.finish().expect("finish foreign-key plan");
        assert!(plan.compile(SchemaDialect::Postgres).is_ok());
        assert!(matches!(
            plan.compile(SchemaDialect::Mysql),
            Err(crate::DbmError::Unsupported(_))
        ));
        assert!(matches!(
            plan.compile(SchemaDialect::MariaDb),
            Err(crate::DbmError::Unsupported(_))
        ));
    }

    #[test]
    fn sqlite_json_native_storage_adds_validity_check() {
        let mut builder = SchemaBuilder::new();
        builder
            .create_table("payloads", |table| {
                table.column("body", ColumnType::Json(JsonStorage::Native))?;
                Ok(())
            })
            .expect("declare JSON column");
        let compiled = builder
            .finish()
            .expect("finish JSON plan")
            .compile(SchemaDialect::Sqlite)
            .expect("compile JSON validation");
        assert_eq!(
            compiled.statements()[0].sql(),
            "CREATE TABLE \"payloads\" (\"body\" TEXT CHECK (json_valid(\"body\")))"
        );
    }
}
