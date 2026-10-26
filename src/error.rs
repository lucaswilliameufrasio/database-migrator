use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbmError {
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("invalid migration {path}: {message}")]
    InvalidMigration { path: String, message: String },
    #[error("schema divergence: {0}")]
    Diverged(String),
    #[error("safety policy blocked the operation: {0}")]
    Safety(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("migration execution failed: {0}")]
    Execution(String),
    #[error("migration outcome requires explicit reconciliation: {0}")]
    ReconciliationRequired(String),
    #[error("database lock unavailable: {0}")]
    Lock(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("SQLite error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("{driver} database error: {message}")]
    Driver {
        driver: &'static str,
        message: String,
    },
    #[error("JSON encoding error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timestamp error: {0}")]
    Timestamp(String),
    #[error("AI operation failed [{code}]: {message}")]
    Ai { code: &'static str, message: String },
}

impl DbmError {
    pub fn io(path: impl ToString, source: io::Error) -> Self {
        Self::Io {
            path: path.to_string(),
            source,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Configuration(_) | Self::Io { .. } | Self::Timestamp(_) => 2,
            Self::InvalidMigration { .. } => 3,
            Self::Diverged(_) => 4,
            Self::Lock(_) => 5,
            Self::Execution(_) => 6,
            Self::ReconciliationRequired(_) => 7,
            Self::Safety(_) => 9,
            Self::Database(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                5
            }
            Self::Database(_) => 10,
            Self::Driver { .. } => 10,
            Self::Unsupported(_) => 13,
            Self::Ai { .. } => 14,
            Self::Json(_) => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, DbmError>;
