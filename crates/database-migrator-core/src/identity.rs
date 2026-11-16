use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, PrimitiveDateTime, format_description};

use crate::error::{DbmError, Result};

const TIMESTAMP_FORMAT: &str = "[year][month][day][hour][minute][second][subsecond digits:6]";
const UTC_DATETIME_FORMAT: &str =
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MigrationIdentity {
    pub namespace: String,
    pub version: String,
}

pub fn validate_namespace(namespace: &str) -> Result<()> {
    if namespace.is_empty() || namespace.len() > 128 {
        return Err(DbmError::Configuration(
            "namespace must contain between 1 and 128 ASCII characters".to_owned(),
        ));
    }

    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(DbmError::Configuration(
            "namespace may contain only ASCII letters, digits, '_' and '-'".to_owned(),
        ));
    }

    Ok(())
}

pub fn validate_description(description: &str) -> Result<()> {
    if description.is_empty()
        || !description
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(DbmError::Configuration(
            "migration description must use lowercase ASCII letters, digits and '_'".to_owned(),
        ));
    }

    Ok(())
}

pub fn validate_repeatable_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let starts_with_lowercase = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
    if name.is_empty()
        || name.len() > 128
        || !starts_with_lowercase
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(DbmError::Configuration(
            "repeatable name must start with a lowercase ASCII letter and contain at most 128 lowercase letters, digits, or '_'"
                .to_owned(),
        ));
    }

    Ok(())
}

pub fn validate_version(version: &str) -> Result<()> {
    if version.len() != 20 || !version.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DbmError::Configuration(format!(
            "native version must be a 20-digit UTC timestamp: {version}"
        )));
    }

    let format = timestamp_format()?;
    let parsed = PrimitiveDateTime::parse(version, &format).map_err(|error| {
        DbmError::Configuration(format!(
            "invalid UTC migration timestamp {version}: {error}"
        ))
    })?;
    let canonical = parsed
        .assume_utc()
        .format(&format)
        .map_err(|error| DbmError::Timestamp(error.to_string()))?;

    if canonical != version {
        return Err(DbmError::Configuration(format!(
            "migration timestamp is not canonical: {version}"
        )));
    }

    Ok(())
}

pub fn new_version() -> Result<String> {
    let format = timestamp_format()?;
    OffsetDateTime::now_utc()
        .format(&format)
        .map_err(|error| DbmError::Timestamp(error.to_string()))
}

pub fn utc_timestamp() -> Result<String> {
    let format = format_description::parse_borrowed::<2>(UTC_DATETIME_FORMAT)
        .map_err(|error| DbmError::Timestamp(error.to_string()))?;
    OffsetDateTime::now_utc()
        .format(&format)
        .map_err(|error| DbmError::Timestamp(error.to_string()))
}

pub fn migration_key(identity: &MigrationIdentity) -> String {
    let namespace = identity.namespace.as_bytes();
    let version = identity.version.as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-migration-key-v1\0");
    hasher.update((namespace.len() as u32).to_be_bytes());
    hasher.update(namespace);
    hasher.update(b"versioned\0");
    hasher.update((version.len() as u32).to_be_bytes());
    hasher.update(version);
    hex_digest(hasher.finalize().as_slice())
}

pub fn repeatable_migration_key(identity: &MigrationIdentity) -> String {
    let namespace = identity.namespace.as_bytes();
    let name = identity.version.as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-migration-key-v1\0");
    hasher.update((namespace.len() as u32).to_be_bytes());
    hasher.update(namespace);
    hasher.update(b"repeatable\0");
    hasher.update((name.len() as u32).to_be_bytes());
    hasher.update(name);
    hex_digest(hasher.finalize().as_slice())
}

pub fn legacy_migration_key(
    namespace: &str,
    origin_tool: &str,
    source_identity: &str,
    origin_identity: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-migration-key-v1\0");
    for value in [
        namespace,
        "legacy",
        origin_tool,
        source_identity,
        origin_identity,
    ] {
        hasher.update((value.len() as u32).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hex_digest(hasher.finalize().as_slice())
}

pub fn raw_checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:raw:v1:{}", hex_digest(digest.as_slice()))
}

pub fn pair_checksum(up: &[u8], down: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-pair-v1\0up\0");
    hasher.update((up.len() as u64).to_be_bytes());
    hasher.update(up);

    match down {
        Some(bytes) => {
            hasher.update(b"\0down\0present\0");
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        }
        None => hasher.update(b"\0down\0absent\0"),
    }

    format!(
        "sha256:pair:v1:{}",
        hex_digest(hasher.finalize().as_slice())
    )
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn timestamp_format() -> Result<Vec<time::format_description::BorrowedFormatItem<'static>>> {
    format_description::parse_borrowed::<2>(TIMESTAMP_FORMAT)
        .map_err(|error| DbmError::Timestamp(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        MigrationIdentity, legacy_migration_key, migration_key, pair_checksum, raw_checksum,
        validate_description, validate_namespace, validate_version,
    };

    #[test]
    fn accepts_valid_utc_microsecond_version() {
        assert!(validate_version("20261021162643981234").is_ok());
    }

    #[test]
    fn rejects_invalid_calendar_date_and_wrong_width() {
        assert!(validate_version("20260230162643981234").is_err());
        assert!(validate_version("2026102116264398123").is_err());
    }

    #[test]
    fn validates_namespace_and_description_separately() {
        assert!(validate_namespace("billing-v1").is_ok());
        assert!(validate_namespace("billing.v1").is_err());
        assert!(validate_description("create_users2").is_ok());
        assert!(validate_description("Create-Users").is_err());
    }

    #[test]
    fn hashes_exact_bytes_and_separates_pair_encoding() {
        assert_ne!(raw_checksum(b"SELECT 1;\n"), raw_checksum(b"SELECT 1;\r\n"));
        assert_ne!(
            pair_checksum(b"ab", Some(b"c")),
            pair_checksum(b"a", Some(b"bc"))
        );
        assert_ne!(pair_checksum(b"up", None), pair_checksum(b"up", Some(b"")));
    }

    #[test]
    fn native_identity_key_is_stable_and_namespace_scoped() {
        let identity = MigrationIdentity {
            namespace: "default".to_owned(),
            version: "20261021162643981234".to_owned(),
        };
        let same = identity.clone();
        let other_namespace = MigrationIdentity {
            namespace: "billing".to_owned(),
            version: identity.version.clone(),
        };

        assert_eq!(migration_key(&identity), migration_key(&same));
        assert_ne!(migration_key(&identity), migration_key(&other_namespace));
    }

    #[test]
    fn legacy_identity_key_includes_tool_source_and_record_identity() {
        let key = legacy_migration_key("default", "knex", "sha256:source-a", "1");
        assert_eq!(
            key,
            legacy_migration_key("default", "knex", "sha256:source-a", "1")
        );
        assert_ne!(
            key,
            legacy_migration_key("default", "goose", "sha256:source-a", "1")
        );
        assert_ne!(
            key,
            legacy_migration_key("default", "knex", "sha256:source-b", "1")
        );
        assert_ne!(
            key,
            legacy_migration_key("default", "knex", "sha256:source-a", "2")
        );
    }
}
