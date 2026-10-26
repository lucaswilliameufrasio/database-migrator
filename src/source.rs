use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{LoadedConfig, SourceDir};
use crate::error::{DbmError, Result};
use crate::identity::{
    MigrationIdentity, new_version, validate_description, validate_repeatable_name,
    validate_version,
};
use crate::migration::{
    Migration, MigrationKind, PairMigrationInput, SqlDialect, TransactionPolicy, parse_pair,
    parse_repeatable, parse_single, split_sql_statements_for_dialect,
};
use crate::registry::MigrationRegistry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NewFormat {
    Single,
    Pair,
}

#[derive(Default)]
struct PairFiles {
    version: String,
    description: String,
    up: Option<(PathBuf, String)>,
    down: Option<(PathBuf, String)>,
}

enum Candidate {
    Single {
        path: PathBuf,
        version: String,
        description: String,
        source_path: String,
    },
    Pair(PairFiles),
    Repeatable {
        path: PathBuf,
        name: String,
        source_path: String,
    },
}

pub fn discover(config: &LoadedConfig) -> Result<Vec<Migration>> {
    let target_dialect = match config.config.target.driver.as_str() {
        "sqlite" => SqlDialect::Sqlite,
        "postgres" => SqlDialect::Postgres,
        "mysql" => SqlDialect::Mysql,
        driver => {
            return Err(DbmError::Unsupported(format!(
                "migration source discovery does not support driver '{driver}'"
            )));
        }
    };
    let mut candidates = Vec::new();
    let mut pairs = BTreeMap::<String, PairFiles>::new();

    for source in &config.source_dirs {
        collect_source(source, &mut candidates, &mut pairs)?;
    }

    for (key, pair) in pairs {
        if pair.up.is_none() {
            let path = pair
                .down
                .as_ref()
                .map(|(path, _)| path.display().to_string())
                .unwrap_or_else(|| key.clone());
            return Err(DbmError::InvalidMigration {
                path,
                message: "a .down.sql file has no matching .up.sql file".to_owned(),
            });
        }
        candidates.push(Candidate::Pair(pair));
    }

    let mut migrations = Vec::with_capacity(candidates.len());
    let mut seen_versions = BTreeSet::new();
    let mut seen_repeatables = BTreeSet::new();

    for candidate in candidates {
        let migration = match candidate {
            Candidate::Single {
                path,
                version,
                description,
                source_path,
            } => {
                let bytes = fs::read(&path).map_err(|error| DbmError::io(path.display(), error))?;
                parse_single(
                    MigrationIdentity {
                        namespace: config.config.namespace.clone(),
                        version,
                    },
                    description,
                    &path,
                    source_path,
                    &bytes,
                )?
            }
            Candidate::Pair(pair) => {
                let (up_path, up_source_path) = pair.up.expect("up file validated above");
                let up_bytes =
                    fs::read(&up_path).map_err(|error| DbmError::io(up_path.display(), error))?;
                let (down_path, down_source_path, down_bytes) = match pair.down {
                    Some((path, source_path)) => {
                        let bytes =
                            fs::read(&path).map_err(|error| DbmError::io(path.display(), error))?;
                        (Some(path), Some(source_path), Some(bytes))
                    }
                    None => (None, None, None),
                };
                let source_path = match down_source_path {
                    Some(down) => format!("{up_source_path}|{down}"),
                    None => up_source_path,
                };
                parse_pair(PairMigrationInput {
                    identity: MigrationIdentity {
                        namespace: config.config.namespace.clone(),
                        version: pair.version,
                    },
                    description: pair.description,
                    dialect: target_dialect,
                    up_path: &up_path,
                    down_path: down_path.as_deref(),
                    source_path,
                    up_bytes: &up_bytes,
                    down_bytes: down_bytes.as_deref(),
                })?
            }
            Candidate::Repeatable {
                path,
                name,
                source_path,
            } => {
                let bytes = fs::read(&path).map_err(|error| DbmError::io(path.display(), error))?;
                parse_repeatable(
                    MigrationIdentity {
                        namespace: config.config.namespace.clone(),
                        version: name.clone(),
                    },
                    name,
                    &path,
                    source_path,
                    &bytes,
                )?
            }
        };

        validate_migration_for_target(config, target_dialect, &migration)?;

        let unique_identity = match migration.kind {
            MigrationKind::Versioned => seen_versions.insert(migration.identity.version.clone()),
            MigrationKind::Repeatable => {
                seen_repeatables.insert(migration.identity.version.clone())
            }
        };
        if !unique_identity {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path.clone(),
                message: format!(
                    "duplicate {} migration identity {} across configured sources",
                    match migration.kind {
                        MigrationKind::Versioned => "versioned",
                        MigrationKind::Repeatable => "repeatable",
                    },
                    migration.identity.version
                ),
            });
        }
        migrations.push(migration);
    }

    sort_migrations(&mut migrations, &config.source_dirs);
    Ok(migrations)
}

pub(crate) fn discover_with_registry(
    config: &LoadedConfig,
    registry: &MigrationRegistry,
) -> Result<Vec<Migration>> {
    let mut migrations = discover(config)?;
    let target_dialect = target_dialect(config)?;
    let mut seen_versions: BTreeSet<(bool, String)> = migrations
        .iter()
        .map(|migration| {
            (
                migration.kind == MigrationKind::Repeatable,
                migration.identity.version.clone(),
            )
        })
        .collect();

    for migration in registry.materialize(config)? {
        validate_migration_for_target(config, target_dialect, &migration)?;
        if !seen_versions.insert((
            migration.kind == MigrationKind::Repeatable,
            migration.identity.version.clone(),
        )) {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path.clone(),
                message: format!(
                    "duplicate migration identity {} across filesystem and compiled sources",
                    migration.identity.version
                ),
            });
        }
        migrations.push(migration);
    }

    sort_migrations(&mut migrations, &config.source_dirs);
    Ok(migrations)
}

fn sort_migrations(migrations: &mut [Migration], sources: &[SourceDir]) {
    migrations.sort_by(|left, right| {
        let left_repeatable = left.kind == MigrationKind::Repeatable;
        let right_repeatable = right.kind == MigrationKind::Repeatable;
        left_repeatable.cmp(&right_repeatable).then_with(|| {
            if left_repeatable {
                source_order(&left.source_path, sources)
                    .cmp(&source_order(&right.source_path, sources))
                    .then_with(|| left.identity.version.cmp(&right.identity.version))
            } else {
                left.identity.version.cmp(&right.identity.version)
            }
        })
    });
}

fn source_order(source_path: &str, sources: &[SourceDir]) -> usize {
    let mut selected = None;
    for (index, source) in sources.iter().enumerate() {
        let matches_source = if source.logical_path.is_empty() {
            !source_path.contains('/')
        } else {
            source_path.starts_with(&format!("{}/", source.logical_path))
        };
        if !matches_source {
            continue;
        }

        let candidate = (source.logical_path.len(), index);
        if selected.is_none_or(|(best_length, best_index)| {
            candidate.0 > best_length || (candidate.0 == best_length && candidate.1 < best_index)
        }) {
            selected = Some(candidate);
        }
    }
    selected.map_or(usize::MAX, |(_, index)| index)
}

fn target_dialect(config: &LoadedConfig) -> Result<SqlDialect> {
    match config.config.target.driver.as_str() {
        "sqlite" => Ok(SqlDialect::Sqlite),
        "postgres" => Ok(SqlDialect::Postgres),
        "mysql" => Ok(SqlDialect::Mysql),
        driver => Err(DbmError::Unsupported(format!(
            "migration source discovery does not support driver '{driver}'"
        ))),
    }
}

pub(crate) fn validate_migration_for_target(
    config: &LoadedConfig,
    target_dialect: SqlDialect,
    migration: &Migration,
) -> Result<()> {
    if migration.dialect != target_dialect {
        return Err(DbmError::InvalidMigration {
            path: migration.source_path.clone(),
            message: format!(
                "migration dialect {:?} does not match target driver '{}'",
                migration.dialect, config.config.target.driver
            ),
        });
    }
    if target_dialect == SqlDialect::Mysql
        && migration.format == crate::migration::MigrationFormat::Pair
    {
        return Err(DbmError::Unsupported(format!(
            "MySQL/MariaDB migration {} uses paired files, which cannot declare the required transaction=never policy; use a single-file migration",
            migration.identity.version
        )));
    }
    if target_dialect == SqlDialect::Mysql && migration.transaction != TransactionPolicy::Never {
        return Err(DbmError::Unsupported(format!(
            "migration {} must explicitly declare transaction=never for the initial MySQL/MariaDB execution profile",
            migration.identity.version
        )));
    }
    if migration.transaction == TransactionPolicy::Never
        && !matches!(target_dialect, SqlDialect::Postgres | SqlDialect::Mysql)
    {
        return Err(DbmError::Unsupported(format!(
            "migration {} requires non-transactional execution, which is not supported by this target",
            migration.identity.version
        )));
    }
    if migration.transaction == TransactionPolicy::Never && target_dialect == SqlDialect::Mysql {
        validate_mysql_nontransactional_source(&migration.source_path, "up", &migration.up_sql)?;
        if let Some(down_sql) = migration.down_sql.as_deref() {
            validate_mysql_nontransactional_source(&migration.source_path, "down", down_sql)?;
        }
    } else if migration.transaction == TransactionPolicy::Never {
        let up_operation =
            validate_nontransactional_source(&migration.source_path, "up", &migration.up_sql)?;
        let crate::migration::PostgresNonTransactionalIndexOperation::Create {
            index_name: up_index_name,
            ..
        } = up_operation
        else {
            return Err(DbmError::InvalidMigration {
                path: migration.source_path.clone(),
                message: "PostgreSQL non-transactional up must create one concurrent index"
                    .to_owned(),
            });
        };
        if let Some(down_sql) = migration.down_sql.as_deref() {
            let down_operation =
                validate_nontransactional_source(&migration.source_path, "down", down_sql)?;
            if !matches!(
                down_operation,
                crate::migration::PostgresNonTransactionalIndexOperation::Drop {
                    index_name
                } if index_name == up_index_name
            ) {
                return Err(DbmError::InvalidMigration {
                    path: migration.source_path.clone(),
                    message: "PostgreSQL non-transactional down must drop the concurrent index created by up"
                        .to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn validate_nontransactional_source(
    path: &str,
    direction: &str,
    sql: &str,
) -> Result<crate::migration::PostgresNonTransactionalIndexOperation> {
    let statements = split_sql_statements_for_dialect(sql, SqlDialect::Postgres);
    if statements.len() != 1 {
        return Err(DbmError::InvalidMigration {
            path: path.to_owned(),
            message: format!(
                "PostgreSQL non-transactional {direction} must contain exactly one statement"
            ),
        });
    }
    crate::migration::parse_postgres_nontransactional_index_operation(statements[0]).map_err(
        |error| DbmError::InvalidMigration {
            path: path.to_owned(),
            message: format!("unsupported PostgreSQL non-transactional {direction}: {error}"),
        },
    )
}

fn validate_mysql_nontransactional_source(path: &str, direction: &str, sql: &str) -> Result<()> {
    let statements = split_sql_statements_for_dialect(sql, SqlDialect::Mysql);
    if statements.is_empty() {
        return Err(DbmError::InvalidMigration {
            path: path.to_owned(),
            message: format!("non-transactional {direction} section contains no SQL steps"),
        });
    }
    for statement in statements {
        crate::migration::validate_mysql_migration_statement(statement).map_err(|error| {
            DbmError::InvalidMigration {
                path: path.to_owned(),
                message: format!("non-transactional {direction} step is not supported: {error}"),
            }
        })?;
    }
    Ok(())
}

pub fn create_new(
    config: &LoadedConfig,
    description: &str,
    format: NewFormat,
) -> Result<Vec<PathBuf>> {
    validate_description(description)?;
    let source = config.source_dirs.first().ok_or_else(|| {
        DbmError::Configuration("at least one migration source is required".to_owned())
    })?;
    if !source.path.is_dir() {
        return Err(DbmError::Configuration(format!(
            "migration source directory does not exist: {}",
            source.path.display()
        )));
    }

    create_new_at(config, source, description, format, &new_version()?)
}

fn create_new_at(
    config: &LoadedConfig,
    source: &SourceDir,
    description: &str,
    format: NewFormat,
    version: &str,
) -> Result<Vec<PathBuf>> {
    validate_version(version).map_err(|error| DbmError::Safety(error.to_string()))?;

    let _creation_reservation = reserve(
        source.path.join(".dbm-create.lock"),
        "another dbm new command is using this source",
    )?;

    let existing = discover(config)?;
    if existing.iter().any(|migration| {
        migration.kind == MigrationKind::Versioned && migration.identity.version.as_str() >= version
    }) {
        return Err(DbmError::Safety(format!(
            "clock timestamp {version} is not newer than existing migrations; refusing to reuse or reorder a version"
        )));
    }

    let reservation_path = source.path.join(format!(".dbm-new-{version}.lock"));
    let reservation = reserve(
        reservation_path.clone(),
        &format!("timestamp {version} is already reserved"),
    )?;

    let current = discover(config)?;
    if current.iter().any(|migration| {
        migration.kind == MigrationKind::Versioned && migration.identity.version.as_str() >= version
    }) {
        return Err(DbmError::Safety(format!(
            "a migration appeared with timestamp {version} while creating a new migration"
        )));
    }

    let prefix = format!("{version}_{description}");
    let paths = match format {
        NewFormat::Single => {
            let path = source.path.join(format!("{prefix}.sql"));
            let contents = format!(
                "-- dbm:dialect {}\n\
                 -- dbm:transaction auto\n\
                 -- dbm:up\n\
                 -- Add migration SQL here.\n\
                 \n\
                 -- dbm:down\n\
                 -- Add rollback SQL here, or remove this section to mark it irreversible.\n",
                config.config.target.driver
            );
            create_exclusive_file(&path, contents.as_bytes())?;
            vec![path]
        }
        NewFormat::Pair => {
            let up_path = source.path.join(format!("{prefix}.up.sql"));
            let down_path = source.path.join(format!("{prefix}.down.sql"));
            create_exclusive_file(&up_path, b"-- Add migration SQL here.\n")?;
            if let Err(error) = create_exclusive_file(&down_path, b"-- Add rollback SQL here.\n") {
                let _ = fs::remove_file(&up_path);
                return Err(error);
            }
            vec![up_path, down_path]
        }
    };

    drop(reservation);
    Ok(paths)
}

fn reserve(path: PathBuf, reason: &str) -> Result<Reservation> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                DbmError::Safety(format!(
                    "{reason}; inspect stale reservation {} before removing it",
                    path.display()
                ))
            } else {
                DbmError::io(path.display(), error)
            }
        })?;
    Ok(Reservation {
        path,
        file: Some(file),
    })
}

fn collect_source(
    source: &SourceDir,
    candidates: &mut Vec<Candidate>,
    pairs: &mut BTreeMap<String, PairFiles>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(&source.path)
        .map_err(|error| DbmError::io(source.path.display(), error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DbmError::Configuration(format!(
            "migration source must be a real directory, not a symlink: {}",
            source.path.display()
        )));
    }

    let entries =
        fs::read_dir(&source.path).map_err(|error| DbmError::io(source.path.display(), error))?;
    for entry in entries {
        let entry = entry.map_err(|error| DbmError::io(source.path.display(), error))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| DbmError::io(path.display(), error))?;

        if file_type.is_symlink() {
            if path.extension().is_some_and(|extension| extension == "sql") {
                return Err(DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: "symbolic-link migration files are not supported".to_owned(),
                });
            }
            continue;
        }
        if !file_type.is_file() || path.extension().is_none_or(|extension| extension != "sql") {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| DbmError::InvalidMigration {
                path: path.display().to_string(),
                message: "migration filename is not valid UTF-8".to_owned(),
            })?;
        let source_path = logical_file_path(source, name);

        if let Some(stem) = name.strip_suffix(".up.sql") {
            if stem.starts_with("R__") {
                return Err(DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: "repeatable migrations cannot use paired .up.sql/.down.sql files"
                        .to_owned(),
                });
            }
            let (version, description) = parse_stem(stem, &path)?;
            let key = format!("{}/{}", source.logical_path, stem);
            let pair = pairs.entry(key).or_default();
            if pair.up.is_some() {
                return Err(DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: "duplicate .up.sql file for migration".to_owned(),
                });
            }
            pair.version = version;
            pair.description = description;
            pair.up = Some((path, source_path));
            continue;
        }

        if let Some(stem) = name.strip_suffix(".down.sql") {
            if stem.starts_with("R__") {
                return Err(DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: "repeatable migrations cannot use paired .up.sql/.down.sql files"
                        .to_owned(),
                });
            }
            let (version, description) = parse_stem(stem, &path)?;
            let key = format!("{}/{}", source.logical_path, stem);
            let pair = pairs.entry(key).or_default();
            if pair.down.is_some() {
                return Err(DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: "duplicate .down.sql file for migration".to_owned(),
                });
            }
            pair.version = version;
            pair.description = description;
            pair.down = Some((path, source_path));
            continue;
        }

        let stem = name
            .strip_suffix(".sql")
            .ok_or_else(|| DbmError::InvalidMigration {
                path: path.display().to_string(),
                message: "expected a .sql migration file".to_owned(),
            })?;
        if let Some(repeatable_name) = stem.strip_prefix("R__") {
            validate_repeatable_name(repeatable_name).map_err(|error| {
                DbmError::InvalidMigration {
                    path: path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
            let repeatable_name = repeatable_name.to_owned();
            candidates.push(Candidate::Repeatable {
                path,
                name: repeatable_name,
                source_path,
            });
            continue;
        }
        let (version, description) = parse_stem(stem, &path)?;
        candidates.push(Candidate::Single {
            path,
            version,
            description,
            source_path,
        });
    }

    Ok(())
}

fn parse_stem(stem: &str, path: &Path) -> Result<(String, String)> {
    let (version, description) =
        stem.split_once('_')
            .ok_or_else(|| DbmError::InvalidMigration {
                path: path.display().to_string(),
                message: "filename must be <20-digit-version>_<description>.sql".to_owned(),
            })?;
    validate_version(version).map_err(|error| DbmError::InvalidMigration {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    validate_description(description).map_err(|error| DbmError::InvalidMigration {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    Ok((version.to_owned(), description.to_owned()))
}

fn logical_file_path(source: &SourceDir, name: &str) -> String {
    if source.logical_path.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{name}", source.logical_path)
    }
}

fn create_exclusive_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| DbmError::io(path.display(), error))?;

    if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(DbmError::io(path.display(), error));
    }

    Ok(())
}

struct Reservation {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{NewFormat, create_new_at, discover, reserve};
    use crate::config::{LoadedConfig, SourceDir};
    use crate::migration::MigrationFormat;

    fn test_config(root: &std::path::Path) -> LoadedConfig {
        let source_path = root.join("migrations");
        fs::create_dir_all(&source_path).expect("create source directory");
        LoadedConfig {
            config: crate::config::Config {
                version: 1,
                namespace: "default".to_owned(),
                source: vec!["migrations".into()],
                target: crate::config::TargetConfig {
                    driver: "sqlite".to_owned(),
                    database: Some("db.sqlite".into()),
                    url_env: None,
                    tls_ca_cert_env: None,
                    allow_insecure_transport: false,
                    environment: crate::config::Environment::Development,
                },
                execution: crate::config::ExecutionConfig::default(),
                ai: crate::config::AiConfig::default(),
            },
            source_dirs: vec![SourceDir {
                path: source_path,
                logical_path: "migrations".to_owned(),
            }],
            database_path: Some(root.join("db.sqlite")),
        }
    }

    #[test]
    fn discovers_single_and_pair_files_in_version_order() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_create_users.sql"),
            "-- dbm:up\nCREATE TABLE users (id INTEGER);\n-- dbm:down\nDROP TABLE users;\n",
        )
        .expect("write single migration");
        fs::write(
            source.join("20261021162743981234_add_name.up.sql"),
            "ALTER TABLE users ADD COLUMN name TEXT;\n",
        )
        .expect("write pair up");
        fs::write(
            source.join("20261021162743981234_add_name.down.sql"),
            "ALTER TABLE users DROP COLUMN name;\n",
        )
        .expect("write pair down");

        let migrations = discover(&config).expect("discover migrations");

        assert_eq!(migrations.len(), 2);
        assert_eq!(migrations[0].identity.version, "20261021162643981234");
        assert_eq!(migrations[1].format, MigrationFormat::Pair);
    }

    #[test]
    fn repeatables_are_sorted_by_source_then_name_and_duplicate_names_are_rejected() {
        let directory = tempfile::tempdir().expect("temporary project directory");
        let mut config = test_config(directory.path());
        let first_source = config.source_dirs[0].path.clone();
        let second_source = directory.path().join("migrations/second");
        fs::create_dir_all(&second_source).expect("create second migration source");
        config.source_dirs.push(super::SourceDir {
            path: second_source.clone(),
            logical_path: "migrations/second".to_owned(),
        });

        for (source, name) in [
            (&first_source, "R__z_views.sql"),
            (&first_source, "R__b_functions.sql"),
            (&second_source, "R__a_views.sql"),
        ] {
            fs::write(source.join(name), "SELECT 1;\n").expect("write repeatable source");
        }

        let migrations = discover(&config).expect("discover repeatables from both sources");
        let identities: Vec<&str> = migrations
            .iter()
            .map(|migration| migration.identity.version.as_str())
            .collect();
        assert_eq!(
            identities,
            ["b_functions", "z_views", "a_views"],
            "repeatables follow configured source order, then logical name"
        );
        assert!(
            migrations
                .iter()
                .all(|migration| migration.kind == crate::migration::MigrationKind::Repeatable)
        );

        fs::write(second_source.join("R__b_functions.sql"), "SELECT 2;\n")
            .expect("write colliding repeatable name");
        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::InvalidMigration { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_versions_and_orphan_down_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_first.sql"),
            "-- dbm:up\nSELECT 1;\n",
        )
        .expect("write single migration");
        fs::write(
            source.join("20261021162643981234_second.sql"),
            "-- dbm:up\nSELECT 2;\n",
        )
        .expect("write duplicate migration");
        assert!(discover(&config).is_err());

        fs::remove_file(source.join("20261021162643981234_first.sql"))
            .expect("remove first migration");
        fs::remove_file(source.join("20261021162643981234_second.sql"))
            .expect("remove duplicate migration");
        fs::write(
            source.join("20261021162643981234_orphan.down.sql"),
            "DROP TABLE users;\n",
        )
        .expect("write orphan down");
        assert!(discover(&config).is_err());
    }

    #[test]
    fn postgres_dialect_source_cannot_be_run_by_sqlite_target() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_postgres_only.sql"),
            "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n",
        )
        .expect("write PostgreSQL migration");

        let error = discover(&config).expect_err("SQLite target rejects PostgreSQL SQL");
        assert!(matches!(
            error,
            crate::error::DbmError::InvalidMigration { .. }
        ));
    }

    #[test]
    fn discovers_postgres_migrations_from_a_reference_only_target_config() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config(directory.path());
        config.config.target.driver = "postgres".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_postgres_only.sql"),
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);\n",
        )
        .expect("write PostgreSQL migration");

        let migrations = discover(&config).expect("discover PostgreSQL migration");

        assert_eq!(migrations.len(), 1);
        assert_eq!(
            migrations[0].dialect,
            crate::migration::SqlDialect::Postgres
        );
        assert_eq!(
            migrations[0].transaction,
            crate::migration::TransactionPolicy::Never
        );
    }

    #[test]
    fn mysql_migrations_require_explicit_never_and_match_the_target_dialect() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config(directory.path());
        config.config.target.driver = "mysql".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_MYSQL_URL".to_owned());
        let source = &config.source_dirs[0].path;
        let path = source.join("20261021162643981234_mysql_only.sql");

        fs::write(
            &path,
            "-- dbm:dialect mysql\n-- dbm:up\nCREATE TABLE users (id BIGINT PRIMARY KEY);\n",
        )
        .expect("write migration without explicit transaction policy");
        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::Unsupported(_))
        ));

        fs::write(
            &path,
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE users (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE users;\n",
        )
        .expect("write explicitly non-transactional migration");
        let migrations = discover(&config).expect("discover MySQL migration");

        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].dialect, crate::migration::SqlDialect::Mysql);
        assert_eq!(
            migrations[0].transaction,
            crate::migration::TransactionPolicy::Never
        );
    }

    #[test]
    fn mysql_migration_discovery_rejects_runner_control_and_stored_objects() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config(directory.path());
        config.config.target.driver = "mysql".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_MYSQL_URL".to_owned());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_mysql_control.sql"),
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nSET autocommit = 0;\n",
        )
        .expect("write runner-control migration");

        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::InvalidMigration { .. })
        ));
    }

    #[test]
    fn mysql_migration_discovery_rejects_pair_format_without_explicit_policy() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config(directory.path());
        config.config.target.driver = "mysql".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_MYSQL_URL".to_owned());
        let source = &config.source_dirs[0].path;
        fs::write(
            source.join("20261021162643981234_mysql_pair.up.sql"),
            "CREATE TABLE users (id BIGINT PRIMARY KEY);\n",
        )
        .expect("write MySQL paired up migration");
        fs::write(
            source.join("20261021162643981234_mysql_pair.down.sql"),
            "DROP TABLE users;\n",
        )
        .expect("write MySQL paired down migration");

        let error = discover(&config).expect_err("MySQL pair has no explicit safety policy");

        assert!(matches!(error, crate::error::DbmError::Unsupported(_)));
        assert!(error.to_string().contains("single-file migration"));
    }

    #[test]
    fn rejects_multistep_or_non_index_postgres_nontransactional_migrations() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config(directory.path());
        config.config.target.driver = "postgres".to_owned();
        config.config.target.database = None;
        config.config.target.url_env = Some("DBM_TEST_DATABASE_URL".to_owned());
        let source = &config.source_dirs[0].path;
        let path = source.join("20261021162643981234_invalid_nontransactional.sql");

        fs::write(
            &path,
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY first_idx ON users (email);\nCREATE INDEX CONCURRENTLY second_idx ON users (name);\n",
        )
        .expect("write multi-step non-transactional migration");
        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::InvalidMigration { .. })
        ));

        fs::write(
            &path,
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE users (id integer);\n",
        )
        .expect("write unsupported non-transactional DDL");
        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::InvalidMigration { .. })
        ));

        fs::write(
            &path,
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);\n-- dbm:down\nDROP INDEX CONCURRENTLY users_name_idx;\n",
        )
        .expect("write mismatched concurrent-index rollback");
        assert!(matches!(
            discover(&config),
            Err(crate::error::DbmError::InvalidMigration { .. })
        ));

        fs::write(
            &path,
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);\n-- dbm:down\nDROP INDEX CONCURRENTLY users_email_idx;\n",
        )
        .expect("write matching concurrent-index rollback");
        assert_eq!(
            discover(&config)
                .expect("discover matching index pair")
                .len(),
            1
        );
    }

    #[test]
    fn new_reserves_timestamp_and_refuses_reuse() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0];
        let version = "20261021162643981234";

        let created = create_new_at(&config, source, "create_users", NewFormat::Single, version)
            .expect("create migration");
        assert!(created[0].exists());
        assert!(
            !source
                .path
                .join(format!(".dbm-new-{version}.lock"))
                .exists()
        );
        assert!(create_new_at(&config, source, "other", NewFormat::Single, version).is_err());
    }

    #[test]
    fn new_fails_closed_when_another_creator_holds_the_source_lock() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0];
        let lock = reserve(
            source.path.join(".dbm-create.lock"),
            "test creator owns source lock",
        )
        .expect("hold source lock");

        assert!(
            create_new_at(
                &config,
                source,
                "create_users",
                NewFormat::Single,
                "20261021162643981234"
            )
            .is_err()
        );
        drop(lock);
        assert!(
            create_new_at(
                &config,
                source,
                "create_users",
                NewFormat::Single,
                "20261021162643981234"
            )
            .is_ok()
        );
    }

    #[test]
    fn pair_creation_preserves_an_existing_orphan_down_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = test_config(directory.path());
        let source = &config.source_dirs[0];
        let version = "20261021162643981234";
        let prefix = format!("{version}_create_users");
        let down_path = source.path.join(format!("{prefix}.down.sql"));
        fs::write(&down_path, "preserve me\n").expect("pre-create down collision");

        assert!(create_new_at(&config, source, "create_users", NewFormat::Pair, version).is_err());

        assert!(!source.path.join(format!("{prefix}.up.sql")).exists());
        assert_eq!(
            fs::read_to_string(down_path).expect("read collision"),
            "preserve me\n"
        );
        assert!(!source.path.join(".dbm-create.lock").exists());
        assert!(
            !source
                .path
                .join(format!(".dbm-new-{version}.lock"))
                .exists()
        );
    }
}
