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
