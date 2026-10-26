//! SQL-only database migration engine with SQLite, PostgreSQL, MySQL, and MariaDB adapters.
//!
//! The public API and database metadata remain provisional until the full V1
//! extension and compatibility contracts have been validated.

#[cfg(feature = "ai")]
pub mod ai;
mod async_runtime;
pub mod cli;
pub mod config;
mod database;
pub mod dsl;
pub mod engine;
pub mod error;
mod identity;
mod legacy;
mod migration;
pub mod registry;
pub mod runner;
mod source;
mod storage;

pub use engine::MigrationContext;
pub use error::{DbmError, Result};
pub use identity::MigrationIdentity;
pub use migration::{MigrationFormat, MigrationKind, SqlDialect, TransactionPolicy};
pub use registry::{MigrationMetadata, MigrationPlan, MigrationRegistry, RustMigration};

#[cfg(feature = "fuzzing-harness")]
#[doc(hidden)]
pub mod fuzzing {
    use std::path::Path;

    use crate::identity::MigrationIdentity;
    use crate::migration::{
        PairMigrationInput, SqlDialect, parse_pair, parse_repeatable, parse_single,
        split_sql_statements_for_dialect,
    };

    fn identity() -> MigrationIdentity {
        MigrationIdentity {
            namespace: "default".to_owned(),
            version: "20261026000000000001".to_owned(),
        }
    }

    pub fn exercise_scanners(input: &[u8]) {
        let sql = String::from_utf8_lossy(input);
        for dialect in [SqlDialect::Sqlite, SqlDialect::Postgres, SqlDialect::Mysql] {
            let _ = split_sql_statements_for_dialect(&sql, dialect);
        }
    }

    pub fn exercise_parsers(input: &[u8]) {
        let path = Path::new("fuzz-migration.sql");
        let migration = parse_single(
            identity(),
            "fuzz_migration".to_owned(),
            path,
            "fuzz/fuzz-migration.sql".to_owned(),
            input,
        );
        if let Ok(migration) = migration {
            let _ = split_sql_statements_for_dialect(&migration.up_sql, migration.dialect);
            if let Some(down_sql) = migration.down_sql.as_deref() {
                let _ = split_sql_statements_for_dialect(down_sql, migration.dialect);
            }
        }

        let repeatable = parse_repeatable(
            identity(),
            "fuzz_repeatable".to_owned(),
            path,
            "fuzz/R__fuzz_repeatable.sql".to_owned(),
            input,
        );
        if let Ok(migration) = repeatable {
            let _ = split_sql_statements_for_dialect(&migration.up_sql, migration.dialect);
        }

        let pair = parse_pair(PairMigrationInput {
            identity: identity(),
            description: "fuzz_pair".to_owned(),
            dialect: SqlDialect::Sqlite,
            up_path: Path::new("fuzz-migration.up.sql"),
            down_path: None,
            source_path: "fuzz/fuzz-migration.up.sql".to_owned(),
            up_bytes: input,
            down_bytes: None,
        });
        if let Ok(migration) = pair {
            let _ = split_sql_statements_for_dialect(&migration.up_sql, migration.dialect);
        }

        let split = input.len() / 2;
        let pair = parse_pair(PairMigrationInput {
            identity: identity(),
            description: "fuzz_pair_with_down".to_owned(),
            dialect: SqlDialect::Sqlite,
            up_path: Path::new("fuzz-migration.up.sql"),
            down_path: Some(Path::new("fuzz-migration.down.sql")),
            source_path: "fuzz/fuzz-migration.up.sql|fuzz/fuzz-migration.down.sql".to_owned(),
            up_bytes: &input[..split],
            down_bytes: Some(&input[split..]),
        });
        if let Ok(migration) = pair {
            let _ = split_sql_statements_for_dialect(&migration.up_sql, migration.dialect);
            if let Some(down_sql) = migration.down_sql.as_deref() {
                let _ = split_sql_statements_for_dialect(down_sql, migration.dialect);
            }
        }
    }
}
