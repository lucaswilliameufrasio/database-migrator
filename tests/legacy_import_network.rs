use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder};
use serde_json::Value;
use tempfile::TempDir;
use tokio_postgres::NoTls;
use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

const TARGET_URL_ENV: &str = "DBM_TEST_LEGACY_TARGET_URL";
const NATIVE_VERSION: &str = "20261025000000000000";

#[derive(Clone, Copy)]
enum LegacyFixture {
    Knex,
    Goose,
    Sqlx,
}

impl LegacyFixture {
    fn tool(self) -> &'static str {
        match self {
            Self::Knex => "knex",
            Self::Goose => "goose",
            Self::Sqlx => "sqlx",
        }
    }
}

fn project(directory: &TempDir, driver: &str, insecure: bool) -> Result<PathBuf, String> {
    let migration_directory = directory.path().join("migrations/versioned");
    fs::create_dir_all(&migration_directory)
        .map_err(|_| "could not create isolated migration directory".to_owned())?;
    let transaction_annotation = if driver == "mysql" {
        "-- dbm:transaction never\n"
    } else {
        ""
    };
    let migration = format!(
        "-- dbm:dialect {driver}\n{transaction_annotation}\
         -- dbm:up\n\
         CREATE TABLE dbm_native_probe (id BIGINT PRIMARY KEY);\n\
         -- dbm:down\n\
         DROP TABLE dbm_native_probe;\n"
    );
    fs::write(
        migration_directory.join(format!("{NATIVE_VERSION}_native_probe.sql")),
        migration,
    )
    .map_err(|_| "could not write isolated native migration".to_owned())?;
    let config = format!(
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"{driver}\"\nurl_env = \"{TARGET_URL_ENV}\"\n\
         environment = \"test\"\nallow_insecure_transport = {insecure}\n"
    );
    let config_path = directory.path().join("dbm.toml");
    fs::write(&config_path, config)
        .map_err(|_| "could not write isolated Database Migrator config".to_owned())?;
    Ok(config_path)
}

fn run(directory: &Path, target_url: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory)
        .args(arguments)
        .env(TARGET_URL_ENV, target_url)
        .output()
        .expect("start dbm CLI")
}

fn run_owned(directory: PathBuf, target_url: String, arguments: Vec<String>) -> Output {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run(&directory, &target_url, &arguments)
}

fn run_concurrent_apply(
    directory: &Path,
    target_url: &str,
    tool: &str,
    digest: &str,
) -> Result<[Value; 2], String> {
    let arguments = vec![
        "--format".to_owned(),
        "json".to_owned(),
        "import".to_owned(),
        "apply".to_owned(),
        "--from".to_owned(),
        tool.to_owned(),
        "--plan-digest".to_owned(),
        digest.to_owned(),
        "--yes".to_owned(),
    ];
    let first_directory = directory.to_path_buf();
    let second_directory = directory.to_path_buf();
    let first_url = target_url.to_owned();
    let second_url = target_url.to_owned();
    let first_arguments = arguments.clone();
    let second_arguments = arguments;
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(move || run_owned(first_directory, first_url, first_arguments));
        let second = scope.spawn(move || run_owned(second_directory, second_url, second_arguments));
        (first.join(), second.join())
    });
    let outputs = [
        first.map_err(|_| "first concurrent dbm process panicked".to_owned())?,
        second.map_err(|_| "second concurrent dbm process panicked".to_owned())?,
    ];
    let mut reports = Vec::with_capacity(outputs.len());
    for output in outputs {
        if !output.status.success() {
            return Err(format!(
                "concurrent import failed with exit status {:?}: {}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        let report: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "concurrent import did not emit valid JSON".to_owned())?;
        reports.push(report);
    }
    reports
        .try_into()
        .map_err(|_| "concurrent import returned an invalid report count".to_owned())
}

fn run_success(directory: &Path, target_url: &str, arguments: &[&str]) -> Result<Value, String> {
    let output = run(directory, target_url, arguments);
    if !output.status.success() {
        return Err(format!(
            "CLI command failed with exit status {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "CLI command did not emit valid JSON".to_owned())
}

fn run_failure(directory: &Path, target_url: &str, arguments: &[&str]) -> Result<Value, String> {
    let output = run(directory, target_url, arguments);
    if output.status.success() {
        return Err("CLI command unexpectedly succeeded".to_owned());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "failed CLI command did not emit structured JSON".to_owned())
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        return Ok(());
    }
    Err(message.to_owned())
}

fn json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("CLI JSON is missing string field {key}"))
}

fn pg_target_url(base_url: &str, schema: &str) -> Result<String, String> {
    let config = base_url
        .parse::<PostgresConfig>()
        .map_err(|_| "DBM_TEST_DATABASE_URL is not a valid PostgreSQL URL".to_owned())?;
    let hosts_are_local = config.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) => {
            host.eq_ignore_ascii_case("localhost")
                || IpAddr::from_str(host)
                    .map(|address| address.is_loopback())
                    .unwrap_or(false)
        }
        Host::Unix(_) => true,
    }) && config.get_hostaddrs().iter().all(IpAddr::is_loopback);
    require(
        hosts_are_local,
        "network import tests refuse non-loopback PostgreSQL targets",
    )?;
    require(
        config.get_ssl_mode() == SslMode::Disable,
        "PostgreSQL network import tests require the documented local plaintext target",
    )?;

    let query_separator = if base_url.contains('?') { '&' } else { '?' };
    Ok(format!(
        "{base_url}{query_separator}options=-csearch_path%3D{schema}"
    ))
}

async fn connect_postgres(url: &str) -> Result<tokio_postgres::Client, String> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .map_err(|_| "could not connect to isolated PostgreSQL test target".to_owned())?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn setup_postgres_fixture(
    fixture: LegacyFixture,
) -> Result<(String, String, tokio_postgres::Client), String> {
    let base_url = std::env::var("DBM_TEST_DATABASE_URL")
        .map_err(|_| "DBM_TEST_DATABASE_URL is required for this opt-in test".to_owned())?;
    let schema = format!("dbm_legacy_{}", uuid::Uuid::new_v4().simple());
    let target_url = pg_target_url(&base_url, &schema)?;
    let base_client = connect_postgres(&base_url).await?;
    base_client
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .map_err(|_| "could not create isolated PostgreSQL schema".to_owned())?;
    let target_client = connect_postgres(&target_url).await?;
    let fixture_sql = postgres_fixture_sql(fixture);
    target_client
        .batch_execute(fixture_sql)
        .await
        .map_err(|_| "could not create PostgreSQL legacy fixture".to_owned())?;
    Ok((schema, target_url, base_client))
}

fn postgres_fixture_sql(fixture: LegacyFixture) -> &'static str {
    match fixture {
        LegacyFixture::Knex => {
            "CREATE TABLE knex_migrations (\
                 id BIGINT PRIMARY KEY, name TEXT NOT NULL, batch BIGINT NOT NULL, \
                 migration_time TIMESTAMPTZ\
             );\
             CREATE TABLE knex_migrations_lock (\
                 \"index\" BIGINT PRIMARY KEY, is_locked BIGINT NOT NULL\
             );\
             INSERT INTO knex_migrations VALUES \
                 (1, '001_create_legacy_users.sql', 3, '2026-10-01T00:00:00Z');\
             INSERT INTO knex_migrations_lock VALUES (1, 0);"
        }
        LegacyFixture::Goose => {
            "CREATE TABLE goose_db_version (\
                 id BIGINT PRIMARY KEY, version_id BIGINT NOT NULL, \
                 is_applied BOOLEAN NOT NULL, tstamp TIMESTAMPTZ\
             );\
             INSERT INTO goose_db_version VALUES \
                 (1, 0, TRUE, '2026-10-01T00:00:00Z'),\
                 (2, 100, TRUE, '2026-10-01T00:01:00Z'),\
                 (3, 200, TRUE, '2026-10-01T00:02:00Z'),\
                 (4, 100, FALSE, '2026-10-01T00:03:00Z');"
        }
        LegacyFixture::Sqlx => {
            "CREATE TABLE _sqlx_migrations (\
                 version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
                 installed_on TIMESTAMPTZ, success BOOLEAN NOT NULL, \
                 checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL\
             );\
             INSERT INTO _sqlx_migrations VALUES \
                 (42, 'create_accounts', '2026-10-03T00:00:00Z', TRUE, \
                  decode(repeat('ab', 48), 'hex'), 19),\
                 (43, 'create_invoices', NULL, FALSE, \
                  decode(repeat('cd', 48), 'hex'), 0);"
        }
    }
}

async fn drop_postgres_schema(client: &tokio_postgres::Client, schema: &str) {
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop only the isolated PostgreSQL test schema");
}

async fn postgres_end_to_end(fixture: LegacyFixture) -> Result<(), String> {
    let (schema, target_url, cleanup_client) = setup_postgres_fixture(fixture).await?;
    let directory =
        tempfile::tempdir().map_err(|_| "could not create temporary project".to_owned())?;
    let project_result = async {
        project(&directory, "postgres", true)?;
        let tool = fixture.tool();
        let inspect = run_success(
            directory.path(),
            &target_url,
            &["--format", "json", "import", "inspect", "--from", tool],
        )?;
        require(
            inspect["source"]["current_migrations"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            &format!("PostgreSQL inspect did not report a current legacy migration: {inspect}"),
        )?;
        let legacy_identity = inspect["source"]["current_migrations"][0]["original_identity"]
            .as_str()
            .ok_or_else(|| "PostgreSQL inspect omitted the legacy identity".to_owned())?
            .to_owned();
        let plan = run_success(
            directory.path(),
            &target_url,
            &["--format", "json", "import", "plan", "--from", tool],
        )?;
        require(
            plan["status"] == "ready",
            "PostgreSQL import plan is not ready",
        )?;
        let digest = json_string(&plan, "digest")?.to_owned();

        let stale = format!("sha256:{}", "0".repeat(64));
        let stale_output = run_failure(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "apply",
                "--from",
                tool,
                "--plan-digest",
                &stale,
                "--yes",
            ],
        )?;
        require(
            stale_output["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "stale PostgreSQL plan did not return a structured error",
        )?;
        let metadata_tables: i64 = cleanup_client
            .query_one(
                "SELECT COUNT(*)::bigint FROM information_schema.tables \
                 WHERE table_schema = $1 AND table_name LIKE 'schema_migration%'",
                &[&schema],
            )
            .await
            .map_err(|_| "could not inspect PostgreSQL metadata side effects".to_owned())?
            .get(0);
        require(
            metadata_tables == 0,
            "PostgreSQL inspect/plan/stale apply unexpectedly initialized metadata",
        )?;

        let parallel_reports = run_concurrent_apply(directory.path(), &target_url, tool, &digest)?;
        let applied = parallel_reports
            .iter()
            .find(|report| report["status"] == "applied")
            .ok_or_else(|| "concurrent PostgreSQL import did not apply".to_owned())?;
        require(
            parallel_reports
                .iter()
                .any(|report| report["status"] == "already_applied"),
            "concurrent PostgreSQL import was not idempotently serialized",
        )?;
        let repeated = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "apply",
                "--from",
                tool,
                "--plan-digest",
                &digest,
                "--yes",
            ],
        )?;
        require(
            repeated["status"] == "already_applied",
            "repeated PostgreSQL import was not idempotent",
        )?;
        let import_id = json_string(applied, "import_id")?.to_owned();
        let native_probe_count: i64 = cleanup_client
            .query_one(
                "SELECT COUNT(*)::bigint FROM information_schema.tables \
                 WHERE table_schema = $1 AND table_name = 'dbm_native_probe'",
                &[&schema],
            )
            .await
            .map_err(|_| "could not verify import did not execute native SQL".to_owned())?
            .get(0);
        require(
            native_probe_count == 0,
            "PostgreSQL import apply executed a native migration",
        )?;
        cleanup_client
            .batch_execute(&format!(
                "UPDATE {schema}.goose_db_version \
                 SET tstamp = tstamp + INTERVAL '1 minute' WHERE id = 3"
            ))
            .await
            .map_err(|_| "could not mutate isolated Goose source for drift test".to_owned())?;
        let drift_verify = run_failure(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "verify",
                "--from",
                tool,
                "--import-id",
                &import_id,
            ],
        )?;
        require(
            drift_verify["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "PostgreSQL import verification did not detect source drift",
        )?;
        cleanup_client
            .batch_execute(&format!(
                "UPDATE {schema}.goose_db_version \
                 SET tstamp = '2026-10-01T00:02:00Z' WHERE id = 3"
            ))
            .await
            .map_err(|_| "could not restore isolated Goose source fixture".to_owned())?;
        let verified = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "verify",
                "--from",
                tool,
                "--import-id",
                &import_id,
            ],
        )?;
        require(
            verified["status"] == "verified",
            "PostgreSQL import did not verify",
        )?;

        let up = run_success(directory.path(), &target_url, &["--format", "json", "up"])?;
        require(
            up["status"] == "succeeded",
            "native PostgreSQL probe did not apply",
        )?;
        let blocked_down =
            run_failure(directory.path(), &target_url, &["--format", "json", "down"])?;
        require(
            blocked_down["errors"]
                .as_array()
                .is_some_and(|errors| errors.iter().any(|error| error["code"] == "SAFETY_BLOCKED")),
            "PostgreSQL rollback was not blocked before legacy adoption",
        )?;

        let evidence_path = directory.path().join("adoption-evidence.txt");
        fs::write(&evidence_path, "isolated PostgreSQL cutover evidence\n")
            .map_err(|_| "could not write adoption evidence".to_owned())?;
        let evidence_path = evidence_path.to_string_lossy().into_owned();
        let adopted = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "adopt",
                "--from",
                tool,
                "--import-id",
                &import_id,
                "--legacy-identity",
                &legacy_identity,
                "--version",
                NATIVE_VERSION,
                "--evidence",
                &evidence_path,
                "--reason",
                "validated isolated cutover",
                "--actor",
                "test-operator",
                "--yes",
            ],
        )?;
        require(
            adopted["status"] == "adopted",
            "PostgreSQL legacy adoption failed",
        )?;
        let down = run_success(directory.path(), &target_url, &["--format", "json", "down"])?;
        require(
            down["status"] == "succeeded",
            "PostgreSQL Down failed after adoption",
        )?;
        Ok(())
    }
    .await;
    drop_postgres_schema(&cleanup_client, &schema).await;
    project_result
}

fn mysql_url_for_database(base_url: &str, database: &str) -> Result<String, String> {
    let scheme_end = base_url
        .find("://")
        .map(|index| index + 3)
        .ok_or_else(|| "DBM_TEST_MYSQL_URL is not a valid MySQL URL".to_owned())?;
    let query_start = base_url.find('?').unwrap_or(base_url.len());
    let path_start = base_url[scheme_end..query_start]
        .rfind('/')
        .map(|index| scheme_end + index + 1)
        .ok_or_else(|| "DBM_TEST_MYSQL_URL must select a database".to_owned())?;
    Ok(format!(
        "{}{database}{}",
        &base_url[..path_start],
        &base_url[query_start..]
    ))
}

fn mysql_target_is_local(options: &Opts) -> bool {
    let host = options.ip_or_hostname();
    host.eq_ignore_ascii_case("localhost")
        || IpAddr::from_str(host)
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

async fn create_mysql_fixture(
    base_url: &str,
    database: &str,
    fixture: LegacyFixture,
) -> Result<(Conn, String), String> {
    let base_options = Opts::from_url(base_url)
        .map_err(|_| "DBM_TEST_MYSQL_URL is not a valid MySQL URL".to_owned())?;
    require(
        mysql_target_is_local(&base_options),
        "network import tests refuse non-loopback MySQL/MariaDB targets",
    )?;
    let mut admin = Conn::new(base_options.clone())
        .await
        .map_err(|_| "could not connect to isolated MySQL/MariaDB test target".to_owned())?;
    admin
        .query_drop(format!(
            "CREATE DATABASE `{database}` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        ))
        .await
        .map_err(|_| "could not create isolated MySQL/MariaDB database".to_owned())?;
    let target_url = mysql_url_for_database(base_url, database)?;
    let target_options =
        Opts::from(OptsBuilder::from_opts(base_options).db_name(Some(database.to_owned())));
    let mut target = Conn::new(target_options)
        .await
        .map_err(|_| "could not connect to isolated MySQL/MariaDB database".to_owned())?;
    let fixture_statements = mysql_fixture_sql(fixture);
    for statement in fixture_statements {
        target
            .query_drop(*statement)
            .await
            .map_err(|_| "could not create MySQL/MariaDB legacy fixture".to_owned())?;
    }
    Ok((admin, target_url))
}

fn mysql_fixture_sql(fixture: LegacyFixture) -> &'static [&'static str] {
    match fixture {
        LegacyFixture::Knex => &[
            "CREATE TABLE knex_migrations (\
                 id BIGINT PRIMARY KEY, name LONGTEXT NOT NULL, batch BIGINT NOT NULL, \
                 migration_time DATETIME(6)\
             ) ENGINE=InnoDB",
            "CREATE TABLE knex_migrations_lock (\
                 `index` BIGINT PRIMARY KEY, is_locked TINYINT NOT NULL\
             ) ENGINE=InnoDB",
            "INSERT INTO knex_migrations VALUES \
                 (1, '001_create_legacy_users.sql', 3, '2026-10-01 00:00:00.000000')",
            "INSERT INTO knex_migrations_lock VALUES (1, 0)",
        ],
        LegacyFixture::Goose => &[
            "CREATE TABLE goose_db_version (\
                 id BIGINT PRIMARY KEY, version_id BIGINT NOT NULL, \
                 is_applied BOOLEAN NOT NULL, tstamp DATETIME(6)\
             ) ENGINE=InnoDB",
            "INSERT INTO goose_db_version VALUES \
                 (1, 0, TRUE, '2026-10-01 00:00:00.000000'),\
                 (2, 100, TRUE, '2026-10-01 00:01:00.000000'),\
                 (3, 200, TRUE, '2026-10-01 00:02:00.000000'),\
                 (4, 100, FALSE, '2026-10-01 00:03:00.000000')",
        ],
        LegacyFixture::Sqlx => &[
            "CREATE TABLE _sqlx_migrations (\
                 version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
                 installed_on DATETIME(6), success BOOLEAN NOT NULL, \
                 checksum BINARY(48) NOT NULL, execution_time BIGINT NOT NULL\
             ) ENGINE=InnoDB",
            "INSERT INTO _sqlx_migrations VALUES \
                 (42, 'create_accounts', '2026-10-03 00:00:00.000000', TRUE, \
                  UNHEX(REPEAT('ab', 48)), 19),\
                 (43, 'create_invoices', NULL, FALSE, \
                  UNHEX(REPEAT('cd', 48)), 0)",
        ],
    }
}

async fn mysql_end_to_end(fixture: LegacyFixture, environment_name: &str) -> Result<(), String> {
    let base_url = std::env::var(environment_name)
        .map_err(|_| format!("{environment_name} is required for this opt-in test"))?;
    let database = format!("dbm_import_{}", uuid::Uuid::new_v4().simple());
    let (mut admin, target_url) = create_mysql_fixture(&base_url, &database, fixture).await?;
    let directory =
        tempfile::tempdir().map_err(|_| "could not create temporary project".to_owned())?;
    let project_result = async {
        project(&directory, "mysql", true)?;
        let tool = fixture.tool();
        if matches!(fixture, LegacyFixture::Sqlx) {
            let blocked_plan = run_success(
                directory.path(),
                &target_url,
                &["--format", "json", "import", "plan", "--from", tool],
            )?;
            require(
                blocked_plan["status"] == "blocked"
                    && blocked_plan["blockers"].as_array().is_some_and(|blockers| {
                        blockers
                            .iter()
                            .any(|blocker| blocker["code"] == "SQLX_MIGRATION_FAILED")
                    }),
                "failed SQLx migration was not blocked on MariaDB",
            )?;
            admin
                .query_drop(format!(
                    "DELETE FROM `{database}`.`_sqlx_migrations` WHERE version = 43"
                ))
                .await
                .map_err(|_| "could not remove failed SQLx test event".to_owned())?;
        }

        let inspect = run_success(
            directory.path(),
            &target_url,
            &["--format", "json", "import", "inspect", "--from", tool],
        )?;
        let legacy_identity = inspect["source"]["current_migrations"][0]["original_identity"]
            .as_str()
            .ok_or_else(|| "MySQL/MariaDB inspect omitted the legacy identity".to_owned())?
            .to_owned();

        let plan = run_success(
            directory.path(),
            &target_url,
            &["--format", "json", "import", "plan", "--from", tool],
        )?;
        require(
            plan["status"] == "ready",
            "MySQL/MariaDB import plan is not ready",
        )?;
        let metadata_tables: Option<u64> = admin
            .query_first(format!(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = '{database}' AND table_name LIKE 'schema_migration%'"
            ))
            .await
            .map_err(|_| "could not inspect MySQL/MariaDB metadata side effects".to_owned())?;
        require(
            metadata_tables == Some(0),
            "MySQL/MariaDB inspect/plan unexpectedly initialized metadata",
        )?;
        let digest = json_string(&plan, "digest")?.to_owned();
        let parallel_reports = run_concurrent_apply(directory.path(), &target_url, tool, &digest)?;
        let applied = parallel_reports
            .iter()
            .find(|report| report["status"] == "applied")
            .ok_or_else(|| "concurrent MySQL/MariaDB import did not apply".to_owned())?;
        require(
            parallel_reports
                .iter()
                .any(|report| report["status"] == "already_applied"),
            "concurrent MySQL/MariaDB import was not idempotently serialized",
        )?;
        let import_id = json_string(applied, "import_id")?.to_owned();
        let native_probe_count: Option<u64> = admin
            .query_first(format!(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = '{database}' AND table_name = 'dbm_native_probe'"
            ))
            .await
            .map_err(|_| "could not verify import did not execute native SQL".to_owned())?;
        require(
            native_probe_count == Some(0),
            "MySQL/MariaDB import apply executed a native migration",
        )?;
        let repeated = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "apply",
                "--from",
                tool,
                "--plan-digest",
                &digest,
                "--yes",
            ],
        )?;
        require(
            repeated["status"] == "already_applied",
            "repeated MySQL/MariaDB import was not idempotent",
        )?;
        let (drift_sql, restore_sql) = match fixture {
            LegacyFixture::Knex => (
                format!("UPDATE `{database}`.`knex_migrations` SET batch = 4 WHERE id = 1"),
                format!("UPDATE `{database}`.`knex_migrations` SET batch = 3 WHERE id = 1"),
            ),
            LegacyFixture::Goose => (
                format!(
                    "UPDATE `{database}`.`goose_db_version` SET tstamp = \
                     DATE_ADD(tstamp, INTERVAL 1 MINUTE) WHERE id = 3"
                ),
                format!(
                    "UPDATE `{database}`.`goose_db_version` SET tstamp = \
                     '2026-10-01 00:02:00.000000' WHERE id = 3"
                ),
            ),
            LegacyFixture::Sqlx => (
                format!(
                    "UPDATE `{database}`.`_sqlx_migrations` \
                     SET description = 'changed_description' WHERE version = 42"
                ),
                format!(
                    "UPDATE `{database}`.`_sqlx_migrations` \
                     SET description = 'create_accounts' WHERE version = 42"
                ),
            ),
        };
        admin
            .query_drop(drift_sql)
            .await
            .map_err(|_| "could not mutate isolated legacy source for drift test".to_owned())?;
        let drift_verify = run_failure(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "verify",
                "--from",
                tool,
                "--import-id",
                &import_id,
            ],
        )?;
        require(
            drift_verify["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "MySQL/MariaDB import verification did not detect source drift",
        )?;
        admin
            .query_drop(restore_sql)
            .await
            .map_err(|_| "could not restore isolated legacy source fixture".to_owned())?;
        let verify = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "verify",
                "--from",
                tool,
                "--import-id",
                &import_id,
            ],
        )?;
        require(
            verify["status"] == "verified",
            "MySQL/MariaDB import did not verify",
        )?;

        let up = run_success(directory.path(), &target_url, &["--format", "json", "up"])?;
        require(
            up["status"] == "succeeded",
            "native MySQL/MariaDB probe did not apply",
        )?;
        let blocked_down =
            run_failure(directory.path(), &target_url, &["--format", "json", "down"])?;
        require(
            blocked_down["errors"]
                .as_array()
                .is_some_and(|errors| errors.iter().any(|error| error["code"] == "SAFETY_BLOCKED")),
            "MySQL/MariaDB rollback was not blocked before legacy adoption",
        )?;

        let evidence_path = directory.path().join("adoption-evidence.txt");
        fs::write(&evidence_path, "isolated MySQL/MariaDB cutover evidence\n")
            .map_err(|_| "could not write adoption evidence".to_owned())?;
        let evidence_path = evidence_path.to_string_lossy().into_owned();
        let adopted = run_success(
            directory.path(),
            &target_url,
            &[
                "--format",
                "json",
                "import",
                "adopt",
                "--from",
                tool,
                "--import-id",
                &import_id,
                "--legacy-identity",
                &legacy_identity,
                "--version",
                NATIVE_VERSION,
                "--evidence",
                &evidence_path,
                "--reason",
                "validated isolated cutover",
                "--actor",
                "test-operator",
                "--yes",
            ],
        )?;
        require(
            adopted["status"] == "adopted",
            "MySQL/MariaDB legacy adoption failed",
        )?;
        let down = run_success(directory.path(), &target_url, &["--format", "json", "down"])?;
        require(
            down["status"] == "succeeded",
            "MySQL/MariaDB Down failed after adoption",
        )?;
        Ok(())
    }
    .await;
    admin
        .query_drop(format!("DROP DATABASE `{database}`"))
        .await
        .expect("drop only the isolated MySQL/MariaDB test database");
    project_result
}

#[tokio::test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
async fn postgres_goose_import_runs_cli_cutover_verify_adopt_and_rollback_gates() {
    postgres_end_to_end(LegacyFixture::Goose)
        .await
        .expect("PostgreSQL F8 CLI integration journey");
}

#[tokio::test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL 8.4 instance"]
async fn mysql_knex_import_runs_cli_cutover_verify_adopt_and_rollback_gates() {
    mysql_end_to_end(LegacyFixture::Knex, "DBM_TEST_MYSQL_URL")
        .await
        .expect("MySQL F8 CLI integration journey");
}

#[tokio::test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
async fn mariadb_sqlx_import_runs_cli_failure_and_cutover_gates() {
    mysql_end_to_end(LegacyFixture::Sqlx, "DBM_TEST_MARIADB_URL")
        .await
        .expect("MariaDB F8 CLI integration journey");
}
