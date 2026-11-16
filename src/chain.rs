//! Phone chain: replay each published SQLite schema on a disposable file.
//!
//! The configured database is not opened. Down SQL is not executed. The
//! published checksum is `sha256:raw:v1` of the up SQL bytes the phone runs,
//! not the checksum of the migration file.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use cap_std::fs::OpenOptions;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde::Serialize;

use crate::config::{LoadedConfig, ProjectDirectory};
use crate::error::{DbmError, Result};
use crate::identity::{self, raw_checksum};
use crate::migration::{self, MigrationKind, SqlDialect, TransactionPolicy};
use crate::source;

const SCHEMA_VERSION: &str = "1.0";
const PUBLISHED_DIRECTORY: &str = "published";

#[derive(Debug)]
pub(crate) enum ChainRequest {
    Replay,
    Record { name: String, through: String },
}

#[derive(Debug)]
pub(crate) enum ChainResult {
    Replay(ReplayReport),
    Record(RecordReport),
}

#[derive(Debug, Serialize)]
pub(crate) struct ReplayReport {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) fresh: FreshChain,
    pub(crate) published: Vec<PublishedChain>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FreshChain {
    pub(crate) applied: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PublishedChain {
    pub(crate) name: String,
    pub(crate) head: String,
    pub(crate) applied: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecordReport {
    pub(crate) schema_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) status: &'static str,
    pub(crate) name: String,
    pub(crate) through: String,
    pub(crate) versions: Vec<RecordedVersion>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecordedVersion {
    pub(crate) version: String,
    pub(crate) checksum: String,
}

struct PhoneMigration {
    version: String,
    up_sql: String,
    checksum: String,
}

struct PublishedSchema {
    name: String,
    versions: Vec<PublishedVersion>,
}

struct PublishedVersion {
    version: String,
    checksum: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    objects: Vec<SchemaObject>,
    rows: Vec<TableRows>,
}

#[derive(Debug, PartialEq, Eq)]
struct SchemaObject {
    kind: String,
    name: String,
    sql: String,
}

#[derive(Debug, PartialEq, Eq)]
struct TableRows {
    name: String,
    rows: Vec<String>,
}

pub(crate) fn execute(config: &LoadedConfig, request: ChainRequest) -> Result<ChainResult> {
    match request {
        ChainRequest::Replay => Ok(ChainResult::Replay(replay(config)?)),
        ChainRequest::Record { name, through } => {
            Ok(ChainResult::Record(record(config, &name, &through)?))
        }
    }
}

fn replay(config: &LoadedConfig) -> Result<ReplayReport> {
    let phone = load_phone_migrations(config)?;
    let published_directory = require_published_directory(config)?;
    let published = read_published(&published_directory)?;
    let mut prefixes = Vec::with_capacity(published.len());
    for schema in &published {
        prefixes.push(shipped_prefix(&phone, schema)?);
    }

    let applied = versions_of(&phone);
    let fresh_snapshot = replay_sql(config, &phone, 0, "fresh")?;
    let mut published_reports = Vec::with_capacity(published.len());
    for (schema, shipped) in published.iter().zip(prefixes) {
        let label = format!("published {}", schema.name);
        let snapshot = replay_sql(config, &phone, shipped, &label)?;
        if snapshot != fresh_snapshot {
            return Err(DbmError::Execution(format!(
                "published {} does not reach the same SQLite catalog as a fresh chain",
                schema.name
            )));
        }
        let head = schema
            .versions
            .last()
            .expect("published schema names at least one version")
            .version
            .clone();
        published_reports.push(PublishedChain {
            name: schema.name.clone(),
            head,
            applied: applied.clone(),
        });
    }

    Ok(ReplayReport {
        schema_version: SCHEMA_VERSION,
        command: "chain",
        status: "ok",
        fresh: FreshChain { applied },
        published: published_reports,
    })
}

fn record(config: &LoadedConfig, name: &str, through: &str) -> Result<RecordReport> {
    validate_schema_name(name)?;
    identity::validate_version(through).map_err(|_| {
        DbmError::Configuration(format!(
            "chain record through {through} must be a 20-digit UTC timestamp"
        ))
    })?;
    let phone = load_phone_migrations(config)?;
    let position = phone
        .iter()
        .position(|migration| migration.version == through)
        .ok_or_else(|| {
            DbmError::Configuration(format!(
                "chain record through {through} is not a versioned migration in this project"
            ))
        })?;

    let published_directory = ensure_published_directory(config)?;
    let file_name = format!("{name}.txt");
    let file_path = published_directory.path().join(&file_name);
    match published_directory.dir().symlink_metadata(&file_name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(DbmError::Configuration(format!(
                "refusing to write published schema through a symlink: {}",
                file_path.display()
            )));
        }
        Ok(_) => {
            return Err(DbmError::Configuration(format!(
                "refusing to overwrite published schema {name}"
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(DbmError::io(file_path.display(), error)),
    }

    let mut body =
        String::from("# up SQL checksums. The phone hashes these bytes, not the migration file.\n");
    let mut versions = Vec::new();
    for migration in phone.iter().take(position + 1) {
        body.push_str(&migration.version);
        body.push(' ');
        body.push_str(&migration.checksum);
        body.push('\n');
        versions.push(RecordedVersion {
            version: migration.version.clone(),
            checksum: migration.checksum.clone(),
        });
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = published_directory
        .dir()
        .open_with(&file_name, &options)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                DbmError::Configuration(format!("refusing to overwrite published schema {name}"))
            } else {
                DbmError::io(file_path.display(), error)
            }
        })?;
    if let Err(error) = file.write_all(body.as_bytes()) {
        drop(file);
        let _ = published_directory.dir().remove_file(&file_name);
        return Err(DbmError::io(file_path.display(), error));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = published_directory.dir().remove_file(&file_name);
        return Err(DbmError::io(file_path.display(), error));
    }

    Ok(RecordReport {
        schema_version: SCHEMA_VERSION,
        command: "chain record",
        status: "recorded",
        name: name.to_owned(),
        through: through.to_owned(),
        versions,
    })
}

fn load_phone_migrations(config: &LoadedConfig) -> Result<Vec<PhoneMigration>> {
    if config.config.target.driver != "sqlite" {
        return Err(DbmError::Unsupported(
            "chain replays published schemas on a disposable SQLite file and this config is not sqlite"
                .to_owned(),
        ));
    }
    if config.config.execution.runner.is_some() {
        return Err(DbmError::Unsupported(
            "chain does not call the migration runner and does not open the configured database"
                .to_owned(),
        ));
    }

    let discovered = source::discover(config)?;
    let mut phone = Vec::new();
    for migration in discovered {
        if migration.kind == MigrationKind::Repeatable {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path,
                message: "phone chain accepts versioned SQL only".to_owned(),
            });
        }
        if migration.dialect != SqlDialect::Sqlite {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path,
                message: "phone chain accepts the SQLite dialect only".to_owned(),
            });
        }
        if migration.transaction == TransactionPolicy::Never {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path,
                message: "phone chain runs each up section in one transaction".to_owned(),
            });
        }
        reject_phone_sql(&migration.source_path, &migration.up_sql)?;
        let checksum = raw_checksum(migration.up_sql.as_bytes());
        phone.push(PhoneMigration {
            version: migration.identity.version,
            up_sql: migration.up_sql,
            checksum,
        });
    }
    Ok(phone)
}

fn reject_phone_sql(path: &str, sql: &str) -> Result<()> {
    if sql.as_bytes().contains(&0) {
        return Err(DbmError::Safety(format!(
            "{path}: phone SQL cannot contain a NUL byte"
        )));
    }
    let statements = migration::split_sql_statements_for_dialect(sql, SqlDialect::Sqlite);
    if statements.is_empty() {
        return Err(DbmError::Safety(format!(
            "{path}: phone up SQL contains no statement"
        )));
    }
    for statement in statements {
        let keyword = statement_keyword(statement).ok_or_else(|| {
            DbmError::Safety(format!("{path}: phone up SQL statement has no command"))
        })?;
        if is_forbidden(&keyword) {
            return Err(DbmError::Safety(format!(
                "{path}: phone up SQL cannot run `{keyword}`"
            )));
        }
    }
    Ok(())
}

fn is_forbidden(keyword: &str) -> bool {
    matches!(
        keyword,
        "begin"
            | "commit"
            | "rollback"
            | "end"
            | "savepoint"
            | "release"
            | "attach"
            | "detach"
            | "vacuum"
    )
}

fn statement_keyword(statement: &str) -> Option<String> {
    let bytes = statement.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'-' && bytes.get(index + 1) == Some(&b'-') {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 < bytes.len() {
                index += 2;
            } else {
                index = bytes.len();
            }
            continue;
        }
        if !bytes[index].is_ascii_alphabetic() {
            return None;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        return Some(statement[start..index].to_ascii_lowercase());
    }
    None
}

fn shipped_prefix(phone: &[PhoneMigration], schema: &PublishedSchema) -> Result<usize> {
    if schema.versions.len() > phone.len() {
        return Err(DbmError::Diverged(format!(
            "published {} names versions that are not in the current chain",
            schema.name
        )));
    }
    for (index, published) in schema.versions.iter().enumerate() {
        let migration = &phone[index];
        if migration.version != published.version {
            return Err(DbmError::Diverged(format!(
                "published {} is not a prefix of the current chain at {}",
                schema.name, published.version
            )));
        }
        if migration.checksum != published.checksum {
            return Err(DbmError::Diverged(format!(
                "published {} version {} checksum mismatch: shipped {} current {}",
                schema.name, published.version, published.checksum, migration.checksum
            )));
        }
    }
    Ok(schema.versions.len())
}

fn versions_of(phone: &[PhoneMigration]) -> Vec<String> {
    let mut versions = Vec::with_capacity(phone.len());
    for migration in phone {
        versions.push(migration.version.clone());
    }
    versions
}

fn replay_sql(
    config: &LoadedConfig,
    phone: &[PhoneMigration],
    shipped: usize,
    label: &str,
) -> Result<Snapshot> {
    let directory = tempfile::TempDir::new()
        .map_err(|error| DbmError::io("disposable SQLite directory", error))?;
    let path = directory.path().join("chain.sqlite");
    if config
        .database_path
        .as_ref()
        .is_some_and(|configured| same_file(configured, &path))
    {
        return Err(DbmError::Safety(
            "chain refused to open the configured database".to_owned(),
        ));
    }
    let mut connection =
        Connection::open_with_flags(&path, disposable_flags()).map_err(|error| {
            DbmError::Execution(format!(
                "chain {label} could not open a disposable SQLite file: {error}"
            ))
        })?;
    for (index, migration) in phone.iter().enumerate() {
        let phase = if index < shipped {
            "shipped"
        } else {
            "pending"
        };
        apply_up(&mut connection, label, phase, migration)?;
    }
    read_snapshot(&connection, label)
}

fn same_file(configured: &Path, disposable: &Path) -> bool {
    if configured == disposable {
        return true;
    }
    match (fs::canonicalize(configured), fs::canonicalize(disposable)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn disposable_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
}

fn apply_up(
    connection: &mut Connection,
    label: &str,
    phase: &str,
    migration: &PhoneMigration,
) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            DbmError::Execution(format!(
                "chain {label} could not start a transaction for {phase} {}: {error}",
                migration.version
            ))
        })?;
    if let Err(error) = transaction.execute_batch(&migration.up_sql) {
        return Err(DbmError::Execution(format!(
            "chain {label} failed while applying {phase} {}: {error}",
            migration.version
        )));
    }
    transaction.commit().map_err(|error| {
        DbmError::Execution(format!(
            "chain {label} failed while committing {phase} {}: {error}",
            migration.version
        ))
    })?;
    Ok(())
}

fn read_snapshot(connection: &Connection, label: &str) -> Result<Snapshot> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, IFNULL(sql, '') \
             FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY type, name",
        )
        .map_err(|error| snapshot_error(label, error))?;
    let mut objects = Vec::new();
    let mut rows = statement
        .query([])
        .map_err(|error| snapshot_error(label, error))?;
    while let Some(row) = rows.next().map_err(|error| snapshot_error(label, error))? {
        objects.push(SchemaObject {
            kind: row.get(0).map_err(|error| snapshot_error(label, error))?,
            name: row.get(1).map_err(|error| snapshot_error(label, error))?,
            sql: row.get(2).map_err(|error| snapshot_error(label, error))?,
        });
    }
    drop(rows);
    drop(statement);

    let mut table_rows = Vec::new();
    for object in &objects {
        if object.kind != "table" {
            continue;
        }
        table_rows.push(TableRows {
            name: object.name.clone(),
            rows: read_table_rows(connection, label, &object.name)?,
        });
    }
    Ok(Snapshot {
        objects,
        rows: table_rows,
    })
}

fn read_table_rows(connection: &Connection, label: &str, table: &str) -> Result<Vec<String>> {
    let mut info = connection
        .prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
        .map_err(|error| snapshot_error(label, error))?;
    let mut columns = Vec::new();
    let mut info_rows = info
        .query([])
        .map_err(|error| snapshot_error(label, error))?;
    while let Some(row) = info_rows
        .next()
        .map_err(|error| snapshot_error(label, error))?
    {
        let name: String = row.get(1).map_err(|error| snapshot_error(label, error))?;
        columns.push(name);
    }
    drop(info_rows);
    drop(info);
    if columns.is_empty() {
        return Ok(Vec::new());
    }

    let mut select = String::new();
    let mut order = String::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            select.push_str(", ");
            order.push_str(", ");
        }
        let quoted = quote_ident(column);
        select.push_str("quote(");
        select.push_str(&quoted);
        select.push(')');
        order.push_str(&quoted);
    }
    let sql = format!(
        "SELECT {select} FROM {} ORDER BY {order}",
        quote_ident(table)
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| snapshot_error(label, error))?;
    let mut query_rows = statement
        .query([])
        .map_err(|error| snapshot_error(label, error))?;
    let mut lines = Vec::new();
    while let Some(row) = query_rows
        .next()
        .map_err(|error| snapshot_error(label, error))?
    {
        let mut line = String::new();
        for index in 0..columns.len() {
            if index > 0 {
                line.push('\t');
            }
            let value: String = row
                .get(index)
                .map_err(|error| snapshot_error(label, error))?;
            line.push_str(&value);
        }
        lines.push(line);
    }
    Ok(lines)
}

fn quote_ident(name: &str) -> String {
    let mut quoted = String::from("\"");
    for character in name.chars() {
        if character == '"' {
            quoted.push('"');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn snapshot_error(label: &str, error: rusqlite::Error) -> DbmError {
    DbmError::Execution(format!(
        "chain {label} could not read the disposable SQLite catalog: {error}"
    ))
}

fn require_published_directory(config: &LoadedConfig) -> Result<ProjectDirectory> {
    let path = config.project_directory.path().join(PUBLISHED_DIRECTORY);
    match config
        .project_directory
        .dir()
        .symlink_metadata(PUBLISHED_DIRECTORY)
    {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DbmError::Configuration(format!(
                    "published must be a real directory next to the config: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(DbmError::Configuration(
                "chain requires a published directory next to the config. Record a shipped schema with `dbm chain record <name> --through <version>`, or create an empty published directory when nothing has shipped"
                    .to_owned(),
            ));
        }
        Err(error) => return Err(DbmError::io(path.display(), error)),
    }
    config
        .project_directory
        .open_dir(Path::new(PUBLISHED_DIRECTORY))
        .map_err(|error| DbmError::io(path.display(), error))
}

fn ensure_published_directory(config: &LoadedConfig) -> Result<ProjectDirectory> {
    let path = config.project_directory.path().join(PUBLISHED_DIRECTORY);
    match config
        .project_directory
        .dir()
        .symlink_metadata(PUBLISHED_DIRECTORY)
    {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DbmError::Configuration(format!(
                    "published must be a real directory next to the config: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            config
                .project_directory
                .dir()
                .create_dir(PUBLISHED_DIRECTORY)
                .map_err(|error| DbmError::io(path.display(), error))?;
            let metadata = config
                .project_directory
                .dir()
                .symlink_metadata(PUBLISHED_DIRECTORY)
                .map_err(|error| DbmError::io(path.display(), error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DbmError::Configuration(format!(
                    "published must be a real directory next to the config: {}",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(DbmError::io(path.display(), error)),
    }
    config
        .project_directory
        .open_dir(Path::new(PUBLISHED_DIRECTORY))
        .map_err(|error| DbmError::io(path.display(), error))
}

fn read_published(directory: &ProjectDirectory) -> Result<Vec<PublishedSchema>> {
    let mut names = Vec::new();
    for entry in directory
        .dir()
        .read_dir(".")
        .map_err(|error| DbmError::io(directory.path().display(), error))?
    {
        let entry = entry.map_err(|error| DbmError::io(directory.path().display(), error))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(DbmError::Configuration(
                "published file name must be UTF-8".to_owned(),
            ));
        };
        if name == "." || name == ".." {
            continue;
        }
        let metadata = directory
            .dir()
            .symlink_metadata(name)
            .map_err(|error| DbmError::io(directory.path().join(name).display(), error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(DbmError::Configuration(format!(
                "published entry must be a regular file: {name}"
            )));
        }
        let Some(schema_name) = name.strip_suffix(".txt") else {
            return Err(DbmError::Configuration(format!(
                "published entry must be a .txt schema file: {name}"
            )));
        };
        validate_schema_name(schema_name)?;
        names.push((schema_name.to_owned(), name.to_owned()));
    }
    names.sort();

    let mut schemas = Vec::with_capacity(names.len());
    for (schema_name, file_name) in names {
        let path = directory.path().join(&file_name);
        let bytes = directory
            .dir()
            .read(&file_name)
            .map_err(|error| DbmError::io(path.display(), error))?;
        let text = String::from_utf8(bytes).map_err(|_| {
            DbmError::Configuration(format!("published schema {schema_name} must be UTF-8"))
        })?;
        schemas.push(parse_published(&schema_name, &text)?);
    }
    Ok(schemas)
}

fn parse_published(name: &str, text: &str) -> Result<PublishedSchema> {
    let mut versions = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let version = fields.next().ok_or_else(|| {
            DbmError::Configuration(format!(
                "published {name} line {line_number} must contain a version and a checksum"
            ))
        })?;
        let checksum = fields.next().ok_or_else(|| {
            DbmError::Configuration(format!(
                "published {name} line {line_number} must contain a version and a checksum"
            ))
        })?;
        if fields.next().is_some() {
            return Err(DbmError::Configuration(format!(
                "published {name} line {line_number} has extra fields"
            )));
        }
        if identity::validate_version(version).is_err() {
            return Err(DbmError::Configuration(format!(
                "published {name} version {version} must be a 20-digit UTC timestamp"
            )));
        }
        if !is_phone_checksum(checksum) {
            return Err(DbmError::Configuration(format!(
                "published {name} version {version} checksum must be sha256:raw:v1 of the up SQL"
            )));
        }
        if versions
            .iter()
            .any(|existing: &PublishedVersion| existing.version == version)
        {
            return Err(DbmError::Configuration(format!(
                "published {name} repeats version {version}"
            )));
        }
        versions.push(PublishedVersion {
            version: version.to_owned(),
            checksum: checksum.to_owned(),
        });
    }
    if versions.is_empty() {
        return Err(DbmError::Configuration(format!(
            "published {name} must name at least one shipped version"
        )));
    }
    Ok(PublishedSchema {
        name: name.to_owned(),
        versions,
    })
}

fn is_phone_checksum(checksum: &str) -> bool {
    let Some(hex) = checksum.strip_prefix("sha256:raw:v1:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_schema_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.ends_with(".txt")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(DbmError::Configuration(
            "published schema name must be 1-64 ASCII letters, digits, '.', '_' or '-', and must not start with '.' or end with '.txt'"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        PhoneMigration, Snapshot, is_forbidden, parse_published, quote_ident, read_snapshot,
        reject_phone_sql, replay_sql, shipped_prefix,
    };
    use crate::error::DbmError;
    use rusqlite::Connection;

    #[test]
    fn rejects_transaction_control_in_phone_sql() {
        let error = reject_phone_sql(
            "migrations/versioned/one.sql",
            "BEGIN;\nCREATE TABLE note (id INTEGER);\n",
        )
        .expect_err("begin is controlled by the chain");
        assert!(matches!(error, DbmError::Safety(_)));
        assert!(error.to_string().contains("begin"));
        assert!(is_forbidden("vacuum"));
        assert!(!is_forbidden("create"));
    }

    #[test]
    fn checksum_mismatch_is_divergence() {
        let phone = [PhoneMigration {
            version: "20261115000000000001".to_owned(),
            up_sql: "SELECT 1;\n".to_owned(),
            checksum:
                "sha256:raw:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
        }];
        let published = parse_published(
            "store-1.0",
            "20261115000000000001 sha256:raw:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        )
        .expect("published schema parses");
        let error = shipped_prefix(&phone, &published).expect_err("checksums differ");
        assert!(matches!(error, DbmError::Diverged(_)));
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn published_schema_must_be_a_prefix() {
        let phone = [
            phone_migration("20261115000000000001"),
            phone_migration("20261115000000000002"),
        ];
        let published = parse_published(
            "store-1.1",
            "20261115000000000002 sha256:raw:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        )
        .expect("published schema parses");
        let error = shipped_prefix(&phone, &published).expect_err("skipped the first version");
        assert!(error.to_string().contains("not a prefix"));
    }

    #[test]
    fn row_changes_make_snapshots_differ() {
        let left = Connection::open_in_memory().expect("left database");
        left.execute_batch(
            "CREATE TABLE note (id INTEGER PRIMARY KEY); INSERT INTO note (id) VALUES (1);",
        )
        .expect("left sql");
        let right = Connection::open_in_memory().expect("right database");
        right
            .execute_batch(
                "CREATE TABLE note (id INTEGER PRIMARY KEY); INSERT INTO note (id) VALUES (2);",
            )
            .expect("right sql");
        let left_snapshot = read_snapshot(&left, "left").expect("left snapshot");
        let right_snapshot = read_snapshot(&right, "right").expect("right snapshot");
        assert_ne!(left_snapshot, right_snapshot);
        assert!(
            left_snapshot
                .objects
                .iter()
                .all(|object| !object.name.starts_with("sqlite_"))
        );
        let _typed: Snapshot = left_snapshot;
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn shipped_prefix_is_applied_before_pending_sql() {
        let directory = tempfile::tempdir().expect("project");
        let marker = directory.path().join("app.sqlite");
        fs::write(&marker, b"marker").expect("marker");
        let project =
            crate::config::ProjectDirectory::open_ambient(directory.path()).expect("open project");
        let config = crate::config::LoadedConfig {
            config: crate::config::Config {
                version: 1,
                namespace: "default".to_owned(),
                source: Vec::new(),
                target: crate::config::TargetConfig {
                    driver: "sqlite".to_owned(),
                    database: Some(std::path::PathBuf::from("app.sqlite")),
                    url_env: None,
                    tls_ca_cert_env: None,
                    allow_insecure_transport: false,
                    environment: crate::config::Environment::Production,
                },
                execution: crate::config::ExecutionConfig::default(),
                ai: crate::config::AiConfig::default(),
            },
            source_dirs: Vec::new(),
            database_path: Some(marker.clone()),
            project_directory: project,
        };
        let phone = vec![
            PhoneMigration {
                version: "20261115000000000001".to_owned(),
                up_sql: "CREATE TABLE note (id INTEGER PRIMARY KEY, name TEXT);\n\
                         INSERT INTO note (id, name) VALUES (1, 'shipped');\n"
                    .to_owned(),
                checksum: "ignored".to_owned(),
            },
            PhoneMigration {
                version: "20261115000000000002".to_owned(),
                up_sql: "UPDATE note SET name = 'kept' WHERE id = 1;\n".to_owned(),
                checksum: "ignored".to_owned(),
            },
        ];
        let snapshot = replay_sql(&config, &phone, 1, "published store-1.0").expect("replay");
        let kept = snapshot
            .rows
            .iter()
            .any(|table| table.rows.iter().any(|row| row.contains("'kept'")));
        assert!(kept, "pending update did not run on the shipped schema");
        assert_eq!(fs::read(&marker).expect("marker bytes"), b"marker");
    }

    fn phone_migration(version: &str) -> PhoneMigration {
        PhoneMigration {
            version: version.to_owned(),
            up_sql: "SELECT 1;\n".to_owned(),
            checksum:
                "sha256:raw:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
        }
    }
}
