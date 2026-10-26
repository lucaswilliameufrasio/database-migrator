use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::{DbmError, Result};
use crate::identity::validate_namespace;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) version: u32,
    pub(crate) namespace: String,
    pub(crate) source: Vec<PathBuf>,
    pub(crate) target: TargetConfig,
    #[serde(default)]
    pub(crate) execution: ExecutionConfig,
    #[serde(default)]
    pub(crate) ai: AiConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AiConfig {
    pub(crate) enabled: bool,
    pub(crate) provider: String,
    pub(crate) profile: String,
    pub(crate) profiles: BTreeMap<String, AiProfileConfig>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "openai-compatible".to_owned(),
            profile: "default".to_owned(),
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AiProfileConfig {
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) api_key_env: String,
    pub(crate) timeout_seconds: u64,
    pub(crate) max_tokens: u32,
    pub(crate) max_context_bytes: usize,
}

impl Default for AiProfileConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            timeout_seconds: 60,
            max_tokens: 4096,
            max_context_bytes: 128 * 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetConfig {
    pub(crate) driver: String,
    #[serde(default)]
    pub(crate) database: Option<PathBuf>,
    #[serde(default)]
    pub(crate) url_env: Option<String>,
    #[serde(default)]
    pub(crate) tls_ca_cert_env: Option<String>,
    #[serde(default)]
    pub(crate) allow_insecure_transport: bool,
    pub(crate) environment: Environment,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Environment {
    Development,
    Test,
    Production,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ExecutionConfig {
    pub(crate) busy_timeout_seconds: u64,
    pub(crate) runner: Option<PathBuf>,
    pub(crate) runner_env: Vec<String>,
    pub(crate) runner_timeout_seconds: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            busy_timeout_seconds: 30,
            runner: None,
            runner_env: Vec::new(),
            runner_timeout_seconds: 3600,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SourceDir {
    pub(crate) path: PathBuf,
    pub(crate) logical_path: String,
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub(crate) config: Config,
    pub(crate) source_dirs: Vec<SourceDir>,
    pub(crate) database_path: Option<PathBuf>,
}

impl LoadedConfig {
    pub(crate) fn sqlite_database_path(&self) -> Result<&Path> {
        if self.config.target.driver != "sqlite" {
            return Err(DbmError::Unsupported(format!(
                "the {} adapter is not available for this operation",
                self.config.target.driver
            )));
        }
        self.database_path.as_deref().ok_or_else(|| {
            DbmError::Configuration("SQLite target is missing its database path".to_owned())
        })
    }

    pub(crate) fn target_description(&self) -> String {
        match self.config.target.driver.as_str() {
            "sqlite" => self
                .database_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "SQLite target is not configured".to_owned()),
            "postgres" => format!(
                "PostgreSQL target via environment variable {}",
                self.config.target.url_env.as_deref().unwrap_or("<missing>")
            ),
            "mysql" => format!(
                "MySQL/MariaDB target via environment variable {}",
                self.config.target.url_env.as_deref().unwrap_or("<missing>")
            ),
            driver => format!("{driver} target"),
        }
    }
}

pub fn load(path: &Path) -> Result<LoadedConfig> {
    let contents = fs::read_to_string(path).map_err(|error| DbmError::io(path.display(), error))?;
    let config: Config =
        toml::from_str(&contents).map_err(|error| DbmError::Configuration(error.to_string()))?;
    validate(&config)?;

    let current_dir = std::env::current_dir()
        .map_err(|error| DbmError::io("current working directory", error))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let root = match parent {
        Some(parent) if parent.is_absolute() => parent.to_path_buf(),
        Some(parent) => current_dir.join(parent),
        None => current_dir.clone(),
    };
    let root = fs::canonicalize(&root).map_err(|error| DbmError::io(root.display(), error))?;
    let config_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    let canonical_config_path = fs::canonicalize(&config_path)
        .map_err(|error| DbmError::io(config_path.display(), error))?;

    let mut source_dirs = Vec::with_capacity(config.source.len());
    for source in &config.source {
        let logical_path = logical_path(source)?;
        let source_path = root.join(source);
        let canonical_source_path = fs::canonicalize(&source_path)
            .map_err(|error| DbmError::io(source_path.display(), error))?;
        ensure_within_project(&root, &canonical_source_path, "migration source")?;
        if !canonical_source_path.is_dir() {
            return Err(DbmError::Configuration(format!(
                "migration source is not a directory: {}",
                source_path.display()
            )));
        }
        source_dirs.push(SourceDir {
            path: source_path,
            logical_path,
        });
    }

    let database_path = config
        .target
        .database
        .as_ref()
        .map(|database| root.join(database));
    if let Some(database_path) = database_path.as_ref() {
        let database_parent = database_path.parent().ok_or_else(|| {
            DbmError::Configuration("target database path must have a parent directory".to_owned())
        })?;
        let canonical_database_parent = fs::canonicalize(database_parent)
            .map_err(|error| DbmError::io(database_parent.display(), error))?;
        ensure_within_project(&root, &canonical_database_parent, "target database")?;
        let canonical_database_path = match fs::symlink_metadata(database_path) {
            Ok(_) => {
                let canonical_path = fs::canonicalize(database_path)
                    .map_err(|error| DbmError::io(database_path.display(), error))?;
                ensure_within_project(&root, &canonical_path, "target database")?;
                if !canonical_path.is_file() {
                    return Err(DbmError::Configuration(format!(
                        "SQLite target must be a regular file: {}",
                        database_path.display()
                    )));
                }
                if canonical_path == canonical_config_path {
                    return Err(DbmError::Configuration(
                        "target database cannot be the configuration file".to_owned(),
                    ));
                }
                Some(canonical_path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(DbmError::io(database_path.display(), error)),
        };

        for source in &source_dirs {
            let canonical_source_path = fs::canonicalize(&source.path)
                .map_err(|error| DbmError::io(source.path.display(), error))?;
            if canonical_database_path
                .as_ref()
                .is_some_and(|database| database.starts_with(&canonical_source_path))
                || database_path.starts_with(&source.path)
            {
                return Err(DbmError::Configuration(format!(
                    "target database must not be inside a migration source directory: {}",
                    database_path.display()
                )));
            }
        }
    }

    Ok(LoadedConfig {
        database_path,
        config,
        source_dirs,
    })
}

pub fn load_with_namespace(path: &Path, namespace: Option<&str>) -> Result<LoadedConfig> {
    let mut loaded = load(path)?;
    if let Some(namespace) = namespace {
        validate_namespace(namespace)?;
        loaded.config.namespace = namespace.to_owned();
    }
    Ok(loaded)
}

pub fn initialize(path: &Path) -> Result<()> {
    initialize_with_namespace(path, "default")
}

pub fn initialize_with_namespace(path: &Path, namespace: &str) -> Result<()> {
    validate_namespace(namespace)?;
    if path.exists() {
        return Err(DbmError::Configuration(format!(
            "refusing to overwrite existing config {}",
            path.display()
        )));
    }

    let current_dir = std::env::current_dir()
        .map_err(|error| DbmError::io("current working directory", error))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let root = match parent {
        Some(parent) if parent.is_absolute() => parent.to_path_buf(),
        Some(parent) => current_dir.join(parent),
        None => current_dir,
    };
    fs::create_dir_all(&root).map_err(|error| DbmError::io(root.display(), error))?;
    let canonical_root =
        fs::canonicalize(&root).map_err(|error| DbmError::io(root.display(), error))?;
    let migrations_dir = ensure_project_directory(&canonical_root, "migrations")?;
    ensure_project_directory(&migrations_dir, "versioned")?;

    let contents = format!(
        "version = 1\nnamespace = {namespace:?}\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"development\"\n\n\
         [execution]\nbusy_timeout_seconds = 30\n\n\
         [ai]\nenabled = false\nprovider = \"openai-compatible\"\nprofile = \"default\"\n"
    );

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let temporary_path = match parent {
        Some(parent) => parent.join(format!(".dbm-config-{}.tmp", uuid::Uuid::new_v4())),
        None => PathBuf::from(format!(".dbm-config-{}.tmp", uuid::Uuid::new_v4())),
    };
    let temporary = TemporaryConfig {
        path: temporary_path,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.path)
        .map_err(|error| DbmError::io(temporary.path.display(), error))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| DbmError::io(temporary.path.display(), error))?;
    file.sync_all()
        .map_err(|error| DbmError::io(temporary.path.display(), error))?;
    drop(file);
    fs::hard_link(&temporary.path, path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            DbmError::Configuration(format!(
                "refusing to overwrite existing config {}",
                path.display()
            ))
        } else {
            DbmError::io(path.display(), error)
        }
    })?;
    Ok(())
}

fn validate(config: &Config) -> Result<()> {
    if config.version != 1 {
        return Err(DbmError::Configuration(format!(
            "unsupported config version {}; expected 1",
            config.version
        )));
    }

    validate_namespace(&config.namespace)?;
    if config.source.is_empty() {
        return Err(DbmError::Configuration(
            "at least one migration source is required".to_owned(),
        ));
    }

    for source in &config.source {
        validate_relative_path(source, "migration source")?;
    }
    match config.target.driver.as_str() {
        "sqlite" => {
            let database = config.target.database.as_deref().ok_or_else(|| {
                DbmError::Configuration("SQLite target requires database".to_owned())
            })?;
            validate_relative_path(database, "target database")?;
            if config.target.url_env.is_some() {
                return Err(DbmError::Configuration(
                    "SQLite target cannot configure url_env".to_owned(),
                ));
            }
            if config.target.tls_ca_cert_env.is_some() {
                return Err(DbmError::Configuration(
                    "SQLite target cannot configure tls_ca_cert_env".to_owned(),
                ));
            }
            if config.target.allow_insecure_transport {
                return Err(DbmError::Configuration(
                    "allow_insecure_transport applies only to an explicitly local PostgreSQL or MySQL target"
                        .to_owned(),
                ));
            }
        }
        "postgres" | "mysql" => {
            let driver = config.target.driver.as_str();
            if config.target.database.is_some() {
                return Err(DbmError::Configuration(format!(
                    "{driver} target uses url_env instead of a local database path"
                )));
            }
            let environment_name = config.target.url_env.as_deref().ok_or_else(|| {
                DbmError::Configuration(format!("{driver} target requires url_env"))
            })?;
            validate_environment_name(environment_name)?;
            if let Some(ca_cert_environment_name) = config.target.tls_ca_cert_env.as_deref() {
                validate_environment_name(ca_cert_environment_name)?;
            }
            if config.target.allow_insecure_transport
                && config.target.environment == Environment::Production
            {
                return Err(DbmError::Configuration(
                    "allow_insecure_transport is forbidden for production targets".to_owned(),
                ));
            }
        }
        driver => {
            return Err(DbmError::Unsupported(format!(
                "driver '{driver}' is not supported; use 'sqlite', 'postgres', or 'mysql'"
            )));
        }
    }

    if config.execution.busy_timeout_seconds > 3600 {
        return Err(DbmError::Configuration(
            "busy_timeout_seconds must be between 0 and 3600".to_owned(),
        ));
    }

    if let Some(runner) = config.execution.runner.as_deref() {
        validate_relative_path(runner, "compiled migration runner")?;
    } else if !config.execution.runner_env.is_empty() {
        return Err(DbmError::Configuration(
            "execution.runner_env requires execution.runner".to_owned(),
        ));
    }
    for name in &config.execution.runner_env {
        validate_environment_name(name)?;
    }
    if config.execution.runner_timeout_seconds == 0
        || config.execution.runner_timeout_seconds > 86_400
    {
        return Err(DbmError::Configuration(
            "runner_timeout_seconds must be between 1 and 86400".to_owned(),
        ));
    }

    if config.ai.provider != "openai-compatible" {
        return Err(DbmError::Configuration(
            "ai.provider must be 'openai-compatible'".to_owned(),
        ));
    }
    validate_ai_profile_name(&config.ai.profile)?;
    if config.ai.enabled && !config.ai.profiles.contains_key(&config.ai.profile) {
        return Err(DbmError::Configuration(format!(
            "ai.profile '{}' is not defined in ai.profiles",
            config.ai.profile
        )));
    }
    for (name, profile) in &config.ai.profiles {
        validate_ai_profile_name(name)?;
        validate_ai_profile(profile)?;
    }

    Ok(())
}

fn validate_ai_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(DbmError::Configuration(
            "AI profile names must contain 1-64 ASCII letters, digits, '_' or '-'".to_owned(),
        ));
    }
    Ok(())
}

fn validate_ai_profile(profile: &AiProfileConfig) -> Result<()> {
    if !validate_ai_endpoint(&profile.endpoint) {
        return Err(DbmError::Configuration(
            "AI endpoint must be an absolute HTTPS URL (or loopback HTTP URL) without credentials, query, or fragment".to_owned(),
        ));
    }
    if profile.model.is_empty()
        || profile.model.len() > 256
        || profile.model.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(DbmError::Configuration(
            "AI model must contain between 1 and 256 bytes".to_owned(),
        ));
    }
    validate_ai_environment_name(&profile.api_key_env)?;
    if profile.timeout_seconds == 0 || profile.timeout_seconds > 300 {
        return Err(DbmError::Configuration(
            "AI timeout_seconds must be between 1 and 300".to_owned(),
        ));
    }
    if profile.max_tokens == 0 || profile.max_tokens > 32_768 {
        return Err(DbmError::Configuration(
            "AI max_tokens must be between 1 and 32768".to_owned(),
        ));
    }
    if profile.max_context_bytes == 0 || profile.max_context_bytes > 1_048_576 {
        return Err(DbmError::Configuration(
            "AI max_context_bytes must be between 1 and 1048576".to_owned(),
        ));
    }
    Ok(())
}

fn validate_ai_endpoint(endpoint: &str) -> bool {
    if endpoint.is_empty()
        || endpoint.bytes().any(|byte| byte.is_ascii_whitespace())
        || endpoint.contains('@')
        || endpoint.contains('?')
        || endpoint.contains('#')
    {
        return false;
    }
    let Some((scheme, remainder)) = endpoint.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "https" | "http") {
        return false;
    }
    let authority = remainder.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.ends_with(':')
        || authority.contains('[') != authority.contains(']')
    {
        return false;
    }
    let (host, is_ipv6) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if !suffix.is_empty() && (!suffix.starts_with(':') || !valid_ai_endpoint_port(&suffix[1..]))
        {
            return false;
        }
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        (host, true)
    } else {
        let mut pieces = authority.split(':');
        let host = pieces.next().unwrap_or_default();
        if let Some(port) = pieces.next()
            && !valid_ai_endpoint_port(port)
        {
            return false;
        }
        if pieces.next().is_some() {
            return false;
        }
        (host, false)
    };
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    if is_ipv6 {
        return scheme == "https" || host == "::1";
    }
    if host.parse::<std::net::Ipv4Addr>().is_err() && !valid_ai_endpoint_hostname(host) {
        return false;
    }
    if scheme == "https" {
        return true;
    }
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"
}

fn valid_ai_endpoint_port(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn valid_ai_endpoint_hostname(host: &str) -> bool {
    if host.len() > 253 || !host.is_ascii() {
        return false;
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return false;
        }
    }
    true
}

fn validate_ai_environment_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(DbmError::Configuration(
            "AI api_key_env must be a non-empty environment variable name".to_owned(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(DbmError::Configuration(
            "AI api_key_env must be a valid environment variable name".to_owned(),
        ));
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(DbmError::Configuration(
            "url_env must be a non-empty environment variable name".to_owned(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(DbmError::Configuration(
            "url_env must be a valid environment variable name".to_owned(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, label: &str) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || !path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(DbmError::Configuration(format!(
            "{label} path must stay inside the project directory: {}",
            path.display()
        )));
    }

    Ok(())
}

fn ensure_within_project(root: &Path, candidate: &Path, label: &str) -> Result<()> {
    if !candidate.starts_with(root) {
        return Err(DbmError::Configuration(format!(
            "{label} resolves outside the project directory: {}",
            candidate.display()
        )));
    }
    Ok(())
}

fn ensure_project_directory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DbmError::Configuration(format!(
                    "refusing to use a symlink or non-directory project path: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(DbmError::io(path.display(), error)),
            }
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| DbmError::io(path.display(), error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DbmError::Configuration(format!(
                    "refusing to use a symlink or non-directory project path: {}",
                    path.display()
                )));
            }
        }
        Err(error) => return Err(DbmError::io(path.display(), error)),
    }

    let canonical_path =
        fs::canonicalize(&path).map_err(|error| DbmError::io(path.display(), error))?;
    ensure_within_project(parent, &canonical_path, "project directory")?;
    Ok(canonical_path)
}

struct TemporaryConfig {
    path: PathBuf,
}

impl Drop for TemporaryConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn logical_path(path: &Path) -> Result<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_str().ok_or_else(|| {
                DbmError::Configuration(format!(
                    "migration source path is not valid UTF-8: {}",
                    path.display()
                ))
            })?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DbmError::Configuration(format!(
                    "migration source path is not project-relative: {}",
                    path.display()
                )));
            }
        }
    }

    Ok(components.join("/"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Environment, initialize, load, validate_ai_endpoint};
    use crate::error::DbmError;

    #[test]
    fn init_creates_config_and_source_without_overwriting() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("dbm.toml");

        initialize(&config_path).expect("initialization succeeds");
        assert!(directory.path().join("migrations/versioned").is_dir());
        assert!(initialize(&config_path).is_err());

        let loaded = load(&config_path).expect("generated config parses");
        assert_eq!(loaded.config.target.environment, Environment::Development);
        assert!(!loaded.config.ai.enabled);
        assert_eq!(loaded.config.ai.provider, "openai-compatible");
        assert_eq!(loaded.source_dirs.len(), 1);
    }

    #[test]
    fn ai_profiles_keep_secret_as_environment_reference_and_require_secure_endpoint() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create migrations source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='sqlite'\ndatabase='dbm.sqlite'\nenvironment='test'\n[ai]\nenabled=true\nprovider='openai-compatible'\nprofile='local'\n[ai.profiles.local]\nendpoint='https://api.example.test/v1'\nmodel='model-name'\napi_key_env='DBM_AI_KEY'\ntimeout_seconds=20\nmax_tokens=1024\nmax_context_bytes=65536\n",
        )
        .expect("write AI profile config");

        let loaded = load(&config_path).expect("load AI profile config");
        assert!(loaded.config.ai.enabled);
        assert_eq!(loaded.config.ai.profiles["local"].api_key_env, "DBM_AI_KEY");
        assert!(!format!("{loaded:?}").contains("actual-secret"));

        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='sqlite'\ndatabase='dbm.sqlite'\nenvironment='test'\n[ai]\nenabled=true\nprovider='openai-compatible'\nprofile='local'\n[ai.profiles.local]\nendpoint='http://api.example.test/v1'\nmodel='model-name'\napi_key_env='DBM_AI_KEY'\n",
        )
        .expect("write insecure AI profile config");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));

        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='sqlite'\ndatabase='dbm.sqlite'\nenvironment='test'\n[ai]\nenabled=true\nprovider='openai-compatible'\nprofile='missing'\n",
        )
        .expect("write missing-profile config");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }

    #[test]
    fn ai_endpoint_validation_allows_https_or_loopback_and_rejects_malformed_authorities() {
        for endpoint in [
            "https://api.example.test/v1",
            "https://[2001:db8::1]:8443/v1",
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(
                validate_ai_endpoint(endpoint),
                "expected allowed endpoint {endpoint}"
            );
        }
        for endpoint in [
            "http://api.example.test/v1",
            "ftp://api.example.test/v1",
            "https://bad host/v1",
            "https://api.example.test:invalid/v1",
            "https://api.example.test:0/v1",
            "https://[not-ipv6]/v1",
            "https://api..example.test/v1",
            "https://user:password@api.example.test/v1",
            "https://api.example.test/v1?token=secret",
        ] {
            assert!(
                !validate_ai_endpoint(endpoint),
                "expected rejected endpoint {endpoint}"
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_and_parent_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['../outside']\n[target]\ndriver='sqlite'\ndatabase='db.sqlite'\nenvironment='development'\n",
        )
        .expect("write config");

        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }

    #[test]
    fn postgres_config_keeps_only_the_environment_variable_reference() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create migrations source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='postgres'\nurl_env='DBM_TEST_DATABASE_URL'\nenvironment='test'\n",
        )
        .expect("write PostgreSQL config");

        let loaded = load(&config_path).expect("load URL-reference config");

        assert!(loaded.database_path.is_none());
        assert_eq!(
            loaded.target_description(),
            "PostgreSQL target via environment variable DBM_TEST_DATABASE_URL"
        );
        assert!(!format!("{loaded:?}").contains("postgres://"));
    }

    #[test]
    fn mysql_config_keeps_only_the_environment_variable_reference() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create migrations source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='mysql'\nurl_env='DBM_TEST_MYSQL_URL'\ntls_ca_cert_env='DBM_TEST_MYSQL_CA'\nenvironment='test'\n",
        )
        .expect("write MySQL config");

        let loaded = load(&config_path).expect("load URL-reference config");

        assert!(loaded.database_path.is_none());
        assert_eq!(
            loaded.target_description(),
            "MySQL/MariaDB target via environment variable DBM_TEST_MYSQL_URL"
        );
        assert!(!format!("{loaded:?}").contains("mysql://"));
    }

    #[test]
    fn insecure_postgres_transport_is_rejected_for_production() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create migrations source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='postgres'\nurl_env='DBM_TEST_DATABASE_URL'\nallow_insecure_transport=true\nenvironment='production'\n",
        )
        .expect("write PostgreSQL config");

        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }

    #[test]
    fn rejects_direct_connection_urls_and_invalid_url_environment_names() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("migrations/versioned"))
            .expect("create migrations source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='postgres'\nurl='unused-direct-value'\nenvironment='test'\n",
        )
        .expect("write direct URL config");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));

        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations/versioned']\n[target]\ndriver='postgres'\nurl_env='DBM;DATABASE_URL'\nenvironment='test'\n",
        )
        .expect("write invalid variable reference");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }

    #[test]
    fn rejects_database_targets_inside_migration_sources() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("migrations")).expect("create source");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations']\n[target]\ndriver='sqlite'\ndatabase='migrations/db.sqlite'\nenvironment='development'\n",
        )
        .expect("write config");

        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn init_refuses_to_follow_a_migrations_directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), directory.path().join("migrations"))
            .expect("create migrations symlink");
        let config_path = directory.path().join("dbm.toml");

        assert!(matches!(
            initialize(&config_path),
            Err(DbmError::Configuration(_))
        ));
        assert!(!outside.path().join("versioned").exists());
        assert!(!config_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_and_database_symlinks_that_escape_project_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("outside directory");
        let source = directory.path().join("migrations");
        symlink(outside.path(), &source).expect("create source symlink");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations']\n[target]\ndriver='sqlite'\ndatabase='db.sqlite'\nenvironment='development'\n",
        )
        .expect("write config");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));

        fs::remove_file(&source).expect("remove source symlink");
        fs::create_dir(&source).expect("create source directory");
        let external_database = outside.path().join("db.sqlite");
        fs::write(&external_database, b"").expect("create external database target");
        symlink(&external_database, directory.path().join("db.sqlite"))
            .expect("create database symlink");
        let config_path = directory.path().join("dbm.toml");
        fs::write(
            &config_path,
            "version=1\nnamespace='default'\nsource=['migrations']\n[target]\ndriver='sqlite'\ndatabase='db.sqlite'\nenvironment='development'\n",
        )
        .expect("write config");
        assert!(matches!(
            load(&config_path),
            Err(DbmError::Configuration(_))
        ));
    }
}
