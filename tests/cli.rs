use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("dbm process starts")
}

fn run_with_environment(directory: &Path, arguments: &[&str], name: &str, value: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory)
        .args(arguments)
        .env(name, value)
        .output()
        .expect("dbm process starts")
}

#[cfg(not(feature = "ai"))]
#[test]
fn default_binary_does_not_expose_ai_commands() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let help = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .arg("--help")
        .output()
        .expect("start help command");
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("ai"));

    let ai = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["ai", "--help"])
        .output()
        .expect("start disabled AI command");
    assert!(!ai.status.success());
}

fn mysql_options(connection_url: &str, database: Option<&str>) -> mysql_async::Opts {
    let options = mysql_async::Opts::from_url(connection_url).expect("MySQL test URL is valid");
    let builder = mysql_async::OptsBuilder::from_opts(options).prefer_socket(Some(false));
    match database {
        Some(database) => builder.db_name(Some(database.to_owned())).into(),
        None => builder.into(),
    }
}

fn mysql_execute(connection_url: &str, sql: &str) -> Result<(), String> {
    use mysql_async::prelude::Queryable;

    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let mut connection = mysql_async::Conn::new(mysql_options(connection_url, None))
            .await
            .map_err(|_| "connect to disposable MySQL/MariaDB")?;
        let execution = connection
            .query_drop(sql)
            .await
            .map_err(|_| "execute isolated MySQL/MariaDB test setup");
        let close = connection.disconnect().await;
        match (execution, close) {
            (Err(error), _) => Err(error.to_owned()),
            (Ok(()), Err(_)) => Err("close disposable MySQL/MariaDB connection".to_owned()),
            (Ok(()), Ok(())) => Ok(()),
        }
    })
}

fn mysql_scalar(connection_url: &str, database: &str, sql: &str) -> Result<i64, String> {
    use mysql_async::prelude::Queryable;

    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let mut connection = mysql_async::Conn::new(mysql_options(connection_url, Some(database)))
            .await
            .map_err(|_| "connect to disposable MySQL/MariaDB")?;
        let value = connection
            .query_first::<i64, _>(sql)
            .await
            .map_err(|_| "read isolated MySQL/MariaDB state")?
            .ok_or_else(|| "MySQL/MariaDB returned no scalar row".to_owned())?;
        connection
            .disconnect()
            .await
            .map_err(|_| "close disposable MySQL/MariaDB connection")?;
        Ok(value)
    })
}

fn mysql_database_url(connection_url: &str, database: &str) -> String {
    let (base, query) = match connection_url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (connection_url, None),
    };
    let authority = base
        .strip_prefix("mysql://")
        .expect("test URL uses mysql scheme")
        .split_once('/')
        .map_or(base, |(authority, _)| {
            let scheme_end = "mysql://".len();
            &base[..scheme_end + authority.len()]
        });
    match query {
        Some(query) => format!("{authority}/{database}?{query}"),
        None => format!("{authority}/{database}"),
    }
}

fn mysql_url_with_hostname(connection_url: &str, hostname: &str) -> String {
    let (scheme, remainder) = connection_url
        .split_once("://")
        .expect("MySQL test URL has a scheme");
    let (authority, path) = remainder
        .split_once('/')
        .expect("MySQL test URL selects a database");
    let (user_info, host_port) = authority
        .rsplit_once('@')
        .expect("MySQL test URL has a user identity");
    let port = host_port
        .rsplit_once(':')
        .map(|(_, port)| port)
        .expect("MySQL test URL uses an explicit TCP port");
    format!("{scheme}://{user_info}@{hostname}:{port}/{path}")
}

struct MySqlDatabaseCleanup {
    connection_url: String,
    database: String,
    cleaned: bool,
}

impl MySqlDatabaseCleanup {
    fn clean(&mut self) -> Result<(), String> {
        mysql_execute(
            &self.connection_url,
            &format!("DROP DATABASE IF EXISTS `{}`", self.database),
        )?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for MySqlDatabaseCleanup {
    fn drop(&mut self) {
        if !self.cleaned {
            let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.clean();
            }));
            let _ = cleanup;
        }
    }
}

fn mysql_lock_name(database: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-mysql-named-lock-v1\0");
    hasher.update((database.len() as u64).to_be_bytes());
    hasher.update(database.as_bytes());
    let digest = hasher.finalize();
    let mut name = String::from("dbm:");
    for byte in digest.iter().take(30) {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn mysql_statement_hash(statement: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbm-sql-step-v1\0");
    hasher.update(statement.as_bytes());
    format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn mysql_repair_plan(statements: &[&str], inspection: &str) -> String {
    serde_json::json!({
        "schema_version": "1.0",
        "summary": "confirm the failed MySQL migration was manually returned to its unapplied state",
        "steps": statements
            .iter()
            .enumerate()
            .map(|(step_index, statement)| {
                serde_json::json!({
                    "step_index": step_index,
                    "statement_hash": mysql_statement_hash(statement),
                    "state": "confirmed_not_applied",
                    "inspection": inspection,
                })
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn assert_mysql_session_lock_available(
    runtime: &tokio::runtime::Runtime,
    connection: &mut mysql_async::Conn,
    lock_name: &str,
) {
    use mysql_async::prelude::Queryable;

    let acquired: Option<(Option<i64>,)> = runtime
        .block_on(connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
        .expect("probe release of the Database Migrator session lock");
    assert_eq!(
        acquired,
        Some((Some(1),)),
        "Database Migrator should release its MySQL named lock before returning"
    );
    runtime
        .block_on(connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release the test lock probe");
}

fn mysql_deadlock_preserves_named_lock(connection_url: &str, database: &str) {
    use mysql_async::prelude::Queryable;

    let runtime = tokio::runtime::Runtime::new().expect("create MySQL deadlock test runtime");
    runtime.block_on(async {
        let options = mysql_options(connection_url, Some(database));
        let mut first = mysql_async::Conn::new(options.clone())
            .await
            .expect("connect first InnoDB deadlock session");
        let mut second = mysql_async::Conn::new(options.clone())
            .await
            .expect("connect second InnoDB deadlock session");
        let mut observer = mysql_async::Conn::new(options)
            .await
            .expect("connect named-lock observer session");
        let lock_name = mysql_lock_name(database);
        let first_session_id = u64::from(first.id());
        let acquired: Option<(Option<i64>,)> = first
            .query_first(format!("SELECT GET_LOCK('{lock_name}', 0)"))
            .await
            .expect("first session acquires the Database Migrator lock");
        assert_eq!(acquired, Some((Some(1),)));

        first
            .query_drop("CREATE TABLE deadlock_probe (id BIGINT PRIMARY KEY) ENGINE=InnoDB")
            .await
            .expect("create the isolated InnoDB deadlock table");
        first
            .query_drop("INSERT INTO deadlock_probe VALUES (1), (2)")
            .await
            .expect("seed the isolated InnoDB deadlock table");
        first
            .query_drop("START TRANSACTION")
            .await
            .expect("begin first deadlock transaction");
        second
            .query_drop("START TRANSACTION")
            .await
            .expect("begin second deadlock transaction");
        first
            .query_drop("UPDATE deadlock_probe SET id = id WHERE id = 1")
            .await
            .expect("first transaction locks row one");
        second
            .query_drop("UPDATE deadlock_probe SET id = id WHERE id = 2")
            .await
            .expect("second transaction locks row two");

        let (first_result, second_result) = tokio::join!(
            first.query_drop("UPDATE deadlock_probe SET id = id WHERE id = 2"),
            second.query_drop("UPDATE deadlock_probe SET id = id WHERE id = 1"),
        );
        let first_was_deadlock_victim = matches!(
            &first_result,
            Err(mysql_async::Error::Server(server_error)) if server_error.code == 1213
        );
        let second_was_deadlock_victim = matches!(
            &second_result,
            Err(mysql_async::Error::Server(server_error)) if server_error.code == 1213
        );
        assert_ne!(
            first_was_deadlock_victim, second_was_deadlock_victim,
            "exactly one InnoDB transaction should be selected as the deadlock victim"
        );
        assert_ne!(first_result.is_ok(), second_result.is_ok());

        let lock_owner: Option<(Option<u64>,)> = first
            .query_first(format!("SELECT IS_USED_LOCK('{lock_name}')"))
            .await
            .expect("inspect named-lock ownership after the InnoDB deadlock");
        assert_eq!(lock_owner, Some((Some(first_session_id),)));
        let blocked_observer: Option<(Option<i64>,)> = observer
            .query_first(format!("SELECT GET_LOCK('{lock_name}', 0)"))
            .await
            .expect("verify the named lock remains exclusive after deadlock");
        assert_eq!(blocked_observer, Some((Some(0),)));

        first
            .query_drop("ROLLBACK")
            .await
            .expect("finish first deadlock transaction");
        second
            .query_drop("ROLLBACK")
            .await
            .expect("finish second deadlock transaction");
        let released: Option<(Option<i64>,)> = first
            .query_first(format!("SELECT RELEASE_LOCK('{lock_name}')"))
            .await
            .expect("release named lock after the deadlock test");
        assert_eq!(released, Some((Some(1),)));
        let reacquired: Option<(Option<i64>,)> = observer
            .query_first(format!("SELECT GET_LOCK('{lock_name}', 0)"))
            .await
            .expect("verify lock is available after explicit release");
        assert_eq!(reacquired, Some((Some(1),)));
        let observer_released: Option<(Option<i64>,)> = observer
            .query_first(format!("SELECT RELEASE_LOCK('{lock_name}')"))
            .await
            .expect("release observer named lock");
        assert_eq!(observer_released, Some((Some(1),)));
        first
            .query_drop("DROP TABLE deadlock_probe")
            .await
            .expect("remove the isolated InnoDB deadlock table");
        first
            .disconnect()
            .await
            .expect("close first deadlock session");
        second
            .disconnect()
            .await
            .expect("close second deadlock session");
        observer
            .disconnect()
            .await
            .expect("close named-lock observer session");
    });
}

fn reconcile_mysql_run_as_unapplied(
    directory: &Path,
    environment_variable: &str,
    target_url: &str,
    run_id: &str,
    statements: &[&str],
    reason: &str,
) -> Output {
    fs::write(
        directory.join("evidence.txt"),
        "inspected the isolated target and manually removed every effect of the failed run",
    )
    .expect("write external repair evidence");
    fs::write(
        directory.join("repair-plan.json"),
        mysql_repair_plan(
            statements,
            "the stated post-repair inspection found no effect from this statement",
        ),
    )
    .expect("write structured per-statement repair plan");
    run_with_environment(
        directory,
        &[
            "--format",
            "json",
            "doctor",
            "reconcile",
            "--run-id",
            run_id,
            "--result",
            "operation-not-applied",
            "--evidence",
            "evidence.txt",
            "--repair-plan",
            "repair-plan.json",
            "--reason",
            reason,
            "--actor",
            "integration-test",
            "--approve",
        ],
        environment_variable,
        target_url,
    )
}

struct MySqlPartialFailureScenario<'a> {
    version: &'a str,
    filename: &'a str,
    initial_up_sql: &'a str,
    corrected_up_sql: &'a str,
    down_sql: &'a str,
    statements: &'a [&'a str],
    failed_step_index: usize,
    partial_effect_query: &'a str,
    expected_partial_effect: i64,
    cleanup_sql: &'a str,
    corrected_state_query: &'a str,
    expected_corrected_state: i64,
    rollback_steps: &'a str,
}

fn run_mysql_partial_failure_case(
    directory: &Path,
    environment_variable: &str,
    target_url: &str,
    database_url: &str,
    scenario: MySqlPartialFailureScenario<'_>,
) {
    let MySqlPartialFailureScenario {
        version,
        filename,
        initial_up_sql,
        corrected_up_sql,
        down_sql,
        statements,
        failed_step_index,
        partial_effect_query,
        expected_partial_effect,
        cleanup_sql,
        corrected_state_query,
        expected_corrected_state,
        rollback_steps,
    } = scenario;
    let migration_path = directory.join("migrations/versioned").join(filename);
    fs::write(
        &migration_path,
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\n{initial_up_sql}\n-- dbm:down\n{down_sql}\n"
        ),
    )
    .expect("write partially failing MySQL migration");

    let failed = run_with_environment(
        directory,
        &["--format", "json", "up"],
        environment_variable,
        target_url,
    );
    assert!(
        !failed.status.success(),
        "the configured MySQL partial-failure scenario should fail at step {failed_step_index}"
    );
    let history = run_with_environment(
        directory,
        &["--format", "json", "history"],
        environment_variable,
        target_url,
    );
    assert!(
        history.status.success(),
        "MySQL history failed after partial DDL"
    );
    let run_id = json(&history)["runs"]
        .as_array()
        .expect("MySQL history rows")
        .iter()
        .find(|run| run["migration_id"] == version && run["state"] == "indeterminate")
        .and_then(|run| run["run_id"].as_str())
        .expect("partial MySQL migration run remains indeterminate")
        .to_owned();
    let recorded_steps = mysql_scalar(
        database_url,
        &mysql_database_name(target_url),
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.migration_version = '{version}'"
        ),
    )
    .expect("inspect MySQL step journal after partial failure");
    assert_eq!(
        recorded_steps,
        i64::try_from(failed_step_index + 1).unwrap()
    );
    let acknowledged_steps = mysql_scalar(
        database_url,
        &mysql_database_name(target_url),
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.migration_version = '{version}' \
               AND step.step_index < {failed_step_index} \
               AND step.state = 'acknowledged'"
        ),
    )
    .expect("inspect acknowledged MySQL steps");
    assert_eq!(
        acknowledged_steps,
        i64::try_from(failed_step_index).unwrap()
    );
    let indeterminate_steps = mysql_scalar(
        database_url,
        &mysql_database_name(target_url),
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.migration_version = '{version}' \
               AND step.step_index = {failed_step_index} \
               AND step.state = 'indeterminate'"
        ),
    )
    .expect("inspect failed MySQL step state");
    assert_eq!(indeterminate_steps, 1);
    let unreached_steps = mysql_scalar(
        database_url,
        &mysql_database_name(target_url),
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.migration_version = '{version}' \
               AND step.step_index > {failed_step_index}"
        ),
    )
    .expect("inspect unexecuted MySQL steps");
    assert_eq!(unreached_steps, 0);
    assert_eq!(
        mysql_scalar(
            database_url,
            &mysql_database_name(target_url),
            partial_effect_query
        )
        .expect("inspect committed effects before manual repair"),
        expected_partial_effect
    );

    mysql_execute(target_url, cleanup_sql).expect("manually remove partial MySQL effects");
    let reconciled = reconcile_mysql_run_as_unapplied(
        directory,
        environment_variable,
        target_url,
        &run_id,
        statements,
        "manually removed partial effects after inspecting each statement",
    );
    assert!(
        reconciled.status.success(),
        "MySQL partial-failure reconciliation failed: {}",
        String::from_utf8_lossy(&reconciled.stdout)
    );

    fs::write(
        &migration_path,
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\n{corrected_up_sql}\n-- dbm:down\n{down_sql}\n"
        ),
    )
    .expect("write the reviewed corrected MySQL migration source");
    let retried = run_with_environment(
        directory,
        &["--format", "json", "up"],
        environment_variable,
        target_url,
    );
    assert!(
        retried.status.success(),
        "MySQL migration did not run after explicit reconciliation: {}",
        String::from_utf8_lossy(&retried.stdout)
    );
    assert_eq!(
        mysql_scalar(
            database_url,
            &mysql_database_name(target_url),
            corrected_state_query
        )
        .expect("inspect corrected MySQL effects"),
        expected_corrected_state
    );
    let reverted = run_with_environment(
        directory,
        &["--format", "json", "down", "--steps", rollback_steps],
        environment_variable,
        target_url,
    );
    assert!(
        reverted.status.success(),
        "MySQL partial-failure scenario rollback failed: {}",
        String::from_utf8_lossy(&reverted.stdout)
    );
}

fn mysql_database_name(connection_url: &str) -> String {
    let parsed =
        mysql_async::Opts::from_url(connection_url).expect("isolated MySQL target URL is valid");
    parsed
        .db_name()
        .expect("isolated MySQL target URL selects a database")
        .to_owned()
}

fn run_mysql_cli_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(
        parsed.ssl_opts().is_none(),
        "disposable test server uses explicit loopback plaintext"
    );
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create Tokio runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read server version")
        .expect("server version row");
    let is_mariadb = version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(
            is_mariadb,
            "the configured integration server is not MariaDB"
        );
    } else {
        assert!(
            !is_mariadb && version.starts_with("8.4."),
            "the configured integration server is not MySQL 8.4"
        );
    }
    let database = format!("dbm_f5_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated test database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);
    let target_opts =
        mysql_async::Opts::from_url(&target_url).expect("build isolated database URL");
    assert_eq!(target_opts.db_name(), Some(database.as_str()));
    mysql_deadlock_preserves_named_lock(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary project directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = 0\n"
        ),
    )
    .expect("configure the disposable MySQL/MariaDB target");
    let migrations = directory.path().join("migrations/versioned");
    fs::write(
        migrations.join("20261024120000000000_create_cli_probe.sql"),
        "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE cli_probe (id BIGINT PRIMARY KEY, note VARCHAR(100));\nINSERT INTO cli_probe VALUES (1, 'one');\n-- dbm:down\nDELETE FROM cli_probe WHERE id = 1;\nDROP TABLE cli_probe;\n",
    )
    .expect("write MySQL migration");

    let lock_name = mysql_lock_name(&database);
    let lock_result: Option<(Option<i64>,)> = runtime
        .block_on(admin_connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
        .expect("acquire test-held session lock");
    assert_eq!(lock_result, Some((Some(1),)));
    let blocked = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        !blocked.status.success(),
        "concurrent MySQL writer must be blocked"
    );
    assert_eq!(
        mysql_scalar(&base_url, &database, "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name LIKE 'schema_migration_%'")
            .expect("inspect metadata after blocked lock"),
        0
    );
    runtime
        .block_on(admin_connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release test-held session lock");

    let planned = run_with_environment(
        directory.path(),
        &["--format", "json", "plan"],
        environment_variable,
        &target_url,
    );
    assert!(planned.status.success(), "MySQL plan failed");
    assert_eq!(json(&planned)["read_only"], true);
    assert_eq!(
        mysql_scalar(&base_url, &database, "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name LIKE 'schema_migration_%'")
            .expect("inspect metadata after planning"),
        0
    );

    let applied = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        applied.status.success(),
        "MySQL up failed: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    assert_eq!(json(&applied)["status"], "succeeded");
    assert_mysql_session_lock_available(&runtime, &mut admin_connection, &lock_name);
    assert_eq!(
        mysql_scalar(
            &base_url,
            &database,
            "SELECT COUNT(*) FROM cli_probe WHERE id = 1"
        )
        .expect("inspect applied MySQL DDL and DML"),
        1
    );
    assert_eq!(
        mysql_scalar(
            &base_url,
            &database,
            "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1"
        )
        .expect("inspect MySQL metadata format"),
        3
    );
    assert_eq!(
        mysql_scalar(&base_url, &database, "SELECT COUNT(*) FROM schema_migration_steps WHERE transaction_class = 'non_transactional' AND state = 'acknowledged'")
            .expect("inspect MySQL step acknowledgements"),
        2
    );
    let diagnosis = run_with_environment(
        directory.path(),
        &["--format", "json", "doctor"],
        environment_variable,
        &target_url,
    );
    assert!(diagnosis.status.success(), "MySQL doctor failed");
    assert_eq!(json(&diagnosis)["status"], "healthy");

    let reverted = run_with_environment(
        directory.path(),
        &["--format", "json", "down", "--steps", "1"],
        environment_variable,
        &target_url,
    );
    assert!(
        reverted.status.success(),
        "MySQL down failed: {}",
        String::from_utf8_lossy(&reverted.stdout)
    );
    assert_eq!(json(&reverted)["status"], "succeeded");
    assert_mysql_session_lock_available(&runtime, &mut admin_connection, &lock_name);
    assert_eq!(
        mysql_scalar(&base_url, &database, "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'cli_probe'")
            .expect("inspect rolled-back MySQL DDL"),
        0
    );

    let failed_sql = "CREATE TABLE cli_retry_probe (id BIGINT PRIMARY KEY);\n";
    fs::write(
        migrations.join("20261024120000000001_create_retry_probe.sql"),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\n{failed_sql}-- dbm:down\nDROP TABLE cli_retry_probe;\n"
        ),
    )
    .expect("write failure/reconciliation migration");
    mysql_execute(
        &target_url,
        "CREATE TABLE cli_retry_probe (id BIGINT PRIMARY KEY)",
    )
    .expect("create the conflicting object for deterministic statement failure");
    let failed = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        !failed.status.success(),
        "duplicate MySQL DDL must fail closed"
    );
    assert_mysql_session_lock_available(&runtime, &mut admin_connection, &lock_name);
    let history = run_with_environment(
        directory.path(),
        &["--format", "json", "history"],
        environment_variable,
        &target_url,
    );
    assert!(history.status.success(), "MySQL history failed");
    let run_id = json(&history)["runs"]
        .as_array()
        .expect("MySQL history rows")
        .iter()
        .find(|run| run["state"] == "indeterminate")
        .and_then(|run| run["run_id"].as_str())
        .expect("failed MySQL run is journaled as indeterminate")
        .to_owned();
    let mut step_hasher = Sha256::new();
    step_hasher.update(b"dbm-sql-step-v1\0");
    step_hasher.update(failed_sql.trim_end().as_bytes());
    let statement_hash = format!(
        "sha256:{}",
        step_hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    fs::write(
        directory.path().join("evidence.txt"),
        "pre-existing table confirmed",
    )
    .expect("write external evidence");
    fs::write(
        directory.path().join("repair-plan.json"),
        serde_json::json!({
            "schema_version": "1.0",
            "summary": "confirm the duplicate CREATE had no new effect",
            "steps": [{
                "step_index": 0,
                "statement_hash": statement_hash,
                "state": "confirmed_not_applied",
                "inspection": "the isolated database object existed before the failed run",
            }],
        })
        .to_string(),
    )
    .expect("write structured MySQL repair plan");
    let reconciled = run_with_environment(
        directory.path(),
        &[
            "--format",
            "json",
            "doctor",
            "reconcile",
            "--run-id",
            &run_id,
            "--result",
            "operation-not-applied",
            "--evidence",
            "evidence.txt",
            "--repair-plan",
            "repair-plan.json",
            "--reason",
            "confirmed the existing object predates this migration run",
            "--actor",
            "integration-test",
            "--approve",
        ],
        environment_variable,
        &target_url,
    );
    assert!(
        reconciled.status.success(),
        "MySQL explicit reconciliation failed: {}",
        String::from_utf8_lossy(&reconciled.stdout)
    );
    assert_eq!(json(&reconciled)["status"], "reconciled");
    mysql_execute(&target_url, "DROP TABLE cli_retry_probe")
        .expect("remove the conflicting object after reconciliation");
    let retried = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        retried.status.success(),
        "MySQL retry after explicit reconciliation failed: {}",
        String::from_utf8_lossy(&retried.stdout)
    );
    assert_eq!(json(&retried)["status"], "succeeded");
    let final_down = run_with_environment(
        directory.path(),
        &["--format", "json", "down", "--steps", "1"],
        environment_variable,
        &target_url,
    );
    assert!(final_down.status.success(), "MySQL final rollback failed");
    assert_mysql_session_lock_available(&runtime, &mut admin_connection, &lock_name);

    let second_first = "CREATE TABLE cli_partial_second (id BIGINT PRIMARY KEY);";
    let second_failure = "CREATE TABLE cli_partial_second (id BIGINT PRIMARY KEY);";
    let second_unreached = "CREATE TABLE cli_partial_second_unreached (id BIGINT PRIMARY KEY);";
    let second_statements = [second_first, second_failure, second_unreached];
    run_mysql_partial_failure_case(
        directory.path(),
        environment_variable,
        &target_url,
        &base_url,
        MySqlPartialFailureScenario {
            version: "20261024120000000002",
            filename: "20261024120000000002_partial_second.sql",
            initial_up_sql: &second_statements.concat(),
            corrected_up_sql: &[
                second_first,
                "INSERT INTO cli_partial_second VALUES (1);",
                second_unreached,
            ]
            .concat(),
            down_sql: "DROP TABLE cli_partial_second_unreached;DROP TABLE cli_partial_second;",
            statements: &second_statements,
            failed_step_index: 1,
            partial_effect_query: "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'cli_partial_second'",
            expected_partial_effect: 1,
            cleanup_sql: "DROP TABLE cli_partial_second",
            corrected_state_query: "SELECT COUNT(*) FROM cli_partial_second WHERE id = 1",
            expected_corrected_state: 1,
            rollback_steps: "2",
        },
    );

    let third_first = "CREATE TABLE cli_partial_third (id BIGINT PRIMARY KEY);";
    let third_second = "INSERT INTO cli_partial_third VALUES (1);";
    let third_failure = "INSERT INTO cli_partial_third VALUES (1);";
    let third_statements = [third_first, third_second, third_failure];
    run_mysql_partial_failure_case(
        directory.path(),
        environment_variable,
        &target_url,
        &base_url,
        MySqlPartialFailureScenario {
            version: "20261024120000000003",
            filename: "20261024120000000003_partial_third.sql",
            initial_up_sql: &third_statements.concat(),
            corrected_up_sql: &[
                third_first,
                third_second,
                "INSERT INTO cli_partial_third VALUES (2);",
            ]
            .concat(),
            down_sql: "DROP TABLE cli_partial_third",
            statements: &third_statements,
            failed_step_index: 2,
            partial_effect_query: "SELECT COUNT(*) FROM cli_partial_third WHERE id = 1",
            expected_partial_effect: 1,
            cleanup_sql: "DROP TABLE cli_partial_third",
            corrected_state_query: "SELECT COUNT(*) FROM cli_partial_third",
            expected_corrected_state: 2,
            rollback_steps: "3",
        },
    );

    let interrupted_sql = "SELECT SLEEP(3);";
    fs::write(
        migrations.join("20261024120000000004_interrupted_probe.sql"),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\n{interrupted_sql}\n-- dbm:down\nSELECT 0;\n"
        ),
    )
    .expect("write process-loss migration");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &target_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start non-transactional MySQL migration");
    let intent_deadline = Instant::now() + Duration::from_secs(10);
    let mut intent_recorded = false;
    while Instant::now() < intent_deadline {
        let intent_count = mysql_scalar(
            &base_url,
            &database,
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.migration_version = '20261024120000000004' \
               AND run.state = 'started' AND step.state = 'intent_recorded'",
        )
        .expect("inspect the pending MySQL journal intent");
        if intent_count > 0 {
            intent_recorded = true;
            break;
        }
        if child.try_wait().expect("poll migration child").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(intent_recorded, "MySQL statement intent was not persisted");
    child
        .kill()
        .expect("terminate MySQL migration while statement is running");
    assert!(
        !child.wait().expect("reap interrupted migration").success(),
        "interrupted migration process must not report success"
    );

    let lock_name = mysql_lock_name(&database);
    let lock_deadline = Instant::now() + Duration::from_secs(10);
    let mut test_connection_owns_lock = false;
    while Instant::now() < lock_deadline {
        let result: Option<(Option<i64>,)> = runtime
            .block_on(admin_connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
            .expect("probe release of the lost MySQL session lock");
        if result == Some((Some(1),)) {
            test_connection_owns_lock = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        test_connection_owns_lock,
        "lost MySQL session lock remained held"
    );
    runtime
        .block_on(admin_connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release the MySQL lock probe");

    let interrupted_history = run_with_environment(
        directory.path(),
        &["--format", "json", "history"],
        environment_variable,
        &target_url,
    );
    assert!(interrupted_history.status.success());
    let interrupted_run_id = json(&interrupted_history)["runs"]
        .as_array()
        .expect("MySQL history rows after process loss")
        .iter()
        .find(|run| run["migration_id"] == "20261024120000000004" && run["state"] == "started")
        .and_then(|run| run["run_id"].as_str())
        .expect("interrupted MySQL run remains unresolved")
        .to_owned();
    let diagnosis = run_with_environment(
        directory.path(),
        &["--format", "json", "doctor"],
        environment_variable,
        &target_url,
    );
    assert!(diagnosis.status.success());
    assert_eq!(json(&diagnosis)["status"], "lock_held");
    let blocked_before_reconciliation = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        !blocked_before_reconciliation.status.success(),
        "a new MySQL writer must not retry an interrupted run before reconciliation"
    );
    assert_eq!(
        mysql_scalar(
            &base_url,
            &database,
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE migration_version = '20261024120000000004' \
               AND state IN ('planned', 'started', 'indeterminate')",
        )
        .expect("verify the interrupted MySQL run was not automatically retried"),
        1
    );
    fs::write(
        directory.path().join("evidence.txt"),
        "the SELECT SLEEP statement was interrupted before completion",
    )
    .expect("write process-loss evidence");
    fs::write(
        directory.path().join("repair-plan.json"),
        mysql_repair_plan(
            &[interrupted_sql],
            "the session was terminated while the server was sleeping",
        ),
    )
    .expect("write process-loss repair plan");
    let reconciled_interruption = run_with_environment(
        directory.path(),
        &[
            "--format",
            "json",
            "doctor",
            "reconcile",
            "--run-id",
            &interrupted_run_id,
            "--result",
            "operation-not-applied",
            "--evidence",
            "evidence.txt",
            "--repair-plan",
            "repair-plan.json",
            "--reason",
            "confirmed no migration effect after terminating the in-flight statement",
            "--actor",
            "integration-test",
            "--approve",
        ],
        environment_variable,
        &target_url,
    );
    assert!(
        reconciled_interruption.status.success(),
        "MySQL process-loss reconciliation failed: {}",
        String::from_utf8_lossy(&reconciled_interruption.stdout)
    );
    let resumed = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        resumed.status.success(),
        "MySQL migration did not resume after explicit reconciliation: {}",
        String::from_utf8_lossy(&resumed.stdout)
    );
    let interrupted_down = run_with_environment(
        directory.path(),
        &["--format", "json", "down", "--steps", "1"],
        environment_variable,
        &target_url,
    );
    assert!(interrupted_down.status.success());
    database_cleanup
        .clean()
        .expect("drop isolated MySQL database");
}

fn postgres_batch_execute(connection_url: &str, sql: &str) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let result = client
            .batch_execute(sql)
            .await
            .map_err(|_| "execute isolated PostgreSQL test setup".to_owned());
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        result
    })
}

fn postgres_current_schema(connection_url: &str) -> Result<String, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let schema = client
            .query_one("SELECT current_schema()::text", &[])
            .await
            .map_err(|_| "read isolated PostgreSQL search path".to_owned())?
            .try_get(0)
            .map_err(|_| "decode isolated PostgreSQL schema".to_owned())?;
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        Ok(schema)
    })
}

fn postgres_metadata_exists(connection_url: &str) -> Result<bool, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let exists = client
            .query_one(
                "SELECT pg_catalog.to_regclass('schema_migration_meta') IS NOT NULL",
                &[],
            )
            .await
            .map_err(|_| "inspect isolated PostgreSQL metadata".to_owned())?
            .try_get(0)
            .map_err(|_| "decode isolated PostgreSQL metadata state".to_owned())?;
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        Ok(exists)
    })
}

fn postgres_metadata_format(connection_url: &str) -> Result<i64, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let format = client
            .query_one(
                "SELECT metadata_format_version::bigint \
                 FROM schema_migration_meta WHERE singleton = 1",
                &[],
            )
            .await
            .map_err(|_| "read PostgreSQL metadata format".to_owned())?
            .try_get(0)
            .map_err(|_| "decode PostgreSQL metadata format".to_owned())?;
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        Ok(format)
    })
}

struct PostgresSchemaCleanup {
    connection_url: String,
    schema: String,
    cleaned: bool,
}

impl PostgresSchemaCleanup {
    fn clean(&mut self) -> Result<(), String> {
        postgres_batch_execute(
            &self.connection_url,
            &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema),
        )?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for PostgresSchemaCleanup {
    fn drop(&mut self) {
        if !self.cleaned {
            let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.clean();
            }));
            let _ = cleanup;
        }
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout contains one valid JSON document")
}

#[test]
fn cli_completes_sqlite_migration_journey_with_machine_readable_output() {
    let directory = tempfile::tempdir().expect("temporary directory");

    let initialized = run(directory.path(), &["--format", "json", "init"]);
    assert!(initialized.status.success());
    assert_eq!(json(&initialized)["status"], "initialized");

    let created = run(
        directory.path(),
        &[
            "--format",
            "json",
            "new",
            "create_items",
            "--migration-format",
            "sql",
        ],
    );
    assert!(
        created.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created_json = json(&created);
    let files = created_json["files"].as_array().expect("created files");
    assert_eq!(files.len(), 1);
    let migration_path = files[0].as_str().expect("migration path");
    fs::write(
        migration_path,
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE items (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE items;\n",
    )
    .expect("fill migration template");

    let validated = run(directory.path(), &["--format", "json", "validate"]);
    assert!(validated.status.success());
    assert_eq!(json(&validated)["migration_count"], 1);

    let planned = run(directory.path(), &["--format", "json", "plan"]);
    assert!(planned.status.success());
    let plan_json = json(&planned);
    assert!(plan_json["read_only"].as_bool().expect("read-only flag"));
    assert_eq!(
        plan_json["operations"]
            .as_array()
            .expect("planned operations")
            .len(),
        1
    );
    assert!(!directory.path().join("dbm.sqlite").exists());

    let applied = run(directory.path(), &["--format", "json", "up"]);
    assert!(applied.status.success());
    assert_eq!(json(&applied)["status"], "succeeded");
    assert!(directory.path().join("dbm.sqlite").is_file());

    let current = run(directory.path(), &["--format", "json", "status"]);
    assert!(current.status.success());
    assert_eq!(json(&current)["status"], "up_to_date");

    let diagnosis = run(directory.path(), &["--format", "json", "doctor"]);
    assert!(diagnosis.status.success());
    assert_eq!(json(&diagnosis)["status"], "healthy");
    assert_eq!(json(&diagnosis)["read_only"], true);

    let reverted = run(directory.path(), &["--format", "json", "down"]);
    assert!(reverted.status.success());
    assert_eq!(
        json(&reverted)["migrations"]
            .as_array()
            .expect("reverted migrations")
            .len(),
        1
    );

    let history = run(directory.path(), &["--format", "json", "history"]);
    assert!(history.status.success());
    assert_eq!(
        json(&history)["runs"]
            .as_array()
            .expect("history runs")
            .len(),
        2
    );
}

#[test]
fn cli_json_errors_use_stderr_free_structured_output_and_stable_exit_code() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());

    fs::write(
        directory.path().join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"production\"\n\n[execution]\nbusy_timeout_seconds = 30\n",
    )
    .expect("mark target as production");

    let blocked = run(directory.path(), &["--format", "json", "up"]);
    assert_eq!(blocked.status.code(), Some(9));
    assert!(blocked.stderr.is_empty());
    assert_eq!(json(&blocked)["errors"][0]["code"], "SAFETY_BLOCKED");
    assert!(!directory.path().join("dbm.sqlite").exists());
}

#[test]
fn cli_reconciliation_requires_matching_state_evidence_and_approval() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let migrations = directory.path().join("migrations/versioned");
    fs::write(
        migrations.join("20261021162643981234_create_items.sql"),
        "-- dbm:up\nCREATE TABLE items (id INTEGER);\n-- dbm:down\nDROP TABLE items;\n",
    )
    .expect("write migration");
    let applied = run(directory.path(), &["up"]);
    assert!(applied.status.success());
    let history = run(directory.path(), &["--format", "json", "history"]);
    assert!(history.status.success());
    let run_id = json(&history)["runs"][0]["run_id"]
        .as_str()
        .expect("run id")
        .to_owned();

    fs::write(
        migrations.join("20261021162743981234_add_name.sql"),
        "-- dbm:up\nALTER TABLE items ADD COLUMN name TEXT;\n-- dbm:down\nALTER TABLE items DROP COLUMN name;\n",
    )
    .expect("write pending migration");
    let database = rusqlite::Connection::open(directory.path().join("dbm.sqlite"))
        .expect("open test database");
    database
        .execute(
            "UPDATE schema_migration_runs SET state = 'started', ended_at = NULL, duration_ms = NULL WHERE run_id = ?1",
            [&run_id],
        )
        .expect("simulate interrupted run");
    database
        .execute(
            "UPDATE schema_migration_locks SET state = 'observed_active', run_id = ?1 WHERE scope = 'database'",
            [&run_id],
        )
        .expect("simulate abandoned lock");
    drop(database);

    let blocked = run(directory.path(), &["--format", "json", "up"]);
    assert_eq!(blocked.status.code(), Some(9));
    assert_eq!(json(&blocked)["errors"][0]["code"], "SAFETY_BLOCKED");

    fs::write(
        directory.path().join("evidence.txt"),
        "SQLite schema inspected",
    )
    .expect("write evidence");
    let mut statement_hasher = Sha256::new();
    statement_hasher.update(b"dbm-sql-step-v1\0");
    statement_hasher.update(b"CREATE TABLE items (id INTEGER);");
    let statement_hash: String = statement_hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    fs::write(
        directory.path().join("repair-plan.json"),
        serde_json::json!({
            "schema_version": "1.0",
            "summary": "verify the committed transactional operation",
            "steps": [{
                "step_index": 0,
                "statement_hash": format!("sha256:{statement_hash}"),
                "state": "confirmed_applied",
                "inspection": "checked the applied migration record in SQLite",
            }],
        })
        .to_string(),
    )
    .expect("write structured repair plan");
    let rejected = run(
        directory.path(),
        &[
            "--format",
            "json",
            "doctor",
            "reconcile",
            "--run-id",
            &run_id,
            "--result",
            "operation-not-applied",
            "--evidence",
            "evidence.txt",
            "--repair-plan",
            "repair-plan.json",
            "--reason",
            "verified table exists",
            "--actor",
            "operator",
            "--approve",
        ],
    );
    assert_eq!(rejected.status.code(), Some(9));
    assert_eq!(json(&rejected)["errors"][0]["code"], "SAFETY_BLOCKED");

    let reconciled = run(
        directory.path(),
        &[
            "--format",
            "json",
            "doctor",
            "reconcile",
            "--run-id",
            &run_id,
            "--result",
            "operation-applied",
            "--evidence",
            "evidence.txt",
            "--repair-plan",
            "repair-plan.json",
            "--reason",
            "verified table exists",
            "--actor",
            "operator",
            "--approve",
        ],
    );
    assert!(
        reconciled.status.success(),
        "reconcile failed: {}",
        String::from_utf8_lossy(&reconciled.stdout)
    );
    assert_eq!(json(&reconciled)["status"], "reconciled");

    let resumed = run(directory.path(), &["up"]);
    assert!(resumed.status.success());
    assert_eq!(
        json(&run(directory.path(), &["--format", "json", "status"]))["status"],
        "up_to_date"
    );
}

#[test]
fn cli_metadata_upgrade_is_opt_in_and_preserves_legacy_history() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let database_path = directory.path().join("dbm.sqlite");
    let database = rusqlite::Connection::open(&database_path).expect("create legacy target");
    database
        .execute_batch(
            "CREATE TABLE schema_migration_meta (singleton INTEGER PRIMARY KEY, metadata_format_version INTEGER NOT NULL, installation_id TEXT NOT NULL, initialized_at TEXT NOT NULL);\
             CREATE TABLE schema_migrations (namespace TEXT NOT NULL, migration_key TEXT NOT NULL, kind TEXT NOT NULL, native_version TEXT NOT NULL, filename TEXT NOT NULL, source_path TEXT NOT NULL, description TEXT NOT NULL, authoring_type TEXT NOT NULL, content_checksum TEXT NOT NULL, apply_order INTEGER NOT NULL, applied_at TEXT NOT NULL, PRIMARY KEY (namespace, migration_key), UNIQUE (namespace, kind, native_version));\
             CREATE TABLE schema_migration_runs (run_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, namespace TEXT NOT NULL, migration_key TEXT NOT NULL, migration_version TEXT NOT NULL, direction TEXT NOT NULL CHECK (direction IN ('up', 'down')), state TEXT NOT NULL CHECK (state = 'succeeded'), started_at TEXT NOT NULL, ended_at TEXT NOT NULL, duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0), checksum_at_execution TEXT NOT NULL);\
             INSERT INTO schema_migration_meta VALUES (1, 1, 'legacy-installation', '2026-10-23T00:00:00.000000Z');\
             INSERT INTO schema_migration_runs VALUES ('legacy-run', 'legacy-batch', 'default', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '20261021162643981234', 'up', 'succeeded', '2026-10-23T00:00:01.000000Z', '2026-10-23T00:00:02.000000Z', 1000, 'sha256:raw:v1:checksum');",
        )
        .expect("create metadata v1 fixture");
    drop(database);

    let preview = run(
        directory.path(),
        &["--format", "json", "metadata", "upgrade"],
    );
    assert!(preview.status.success());
    assert_eq!(json(&preview)["status"], "requires_approval");
    let database = rusqlite::Connection::open(&database_path).expect("read legacy target");
    let version: i64 = database
        .query_row(
            "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read legacy format version");
    assert_eq!(version, 1);
    drop(database);

    let upgraded = run(
        directory.path(),
        &["--format", "json", "metadata", "upgrade", "--yes"],
    );
    assert!(upgraded.status.success());
    assert_eq!(json(&upgraded)["status"], "upgraded");
    assert_eq!(json(&upgraded)["runs_preserved"], 1);
    let diagnosis = run(directory.path(), &["--format", "json", "doctor"]);
    assert!(diagnosis.status.success());
    assert_eq!(json(&diagnosis)["metadata_format_version"], 5);
}

#[test]
fn cli_metadata_upgrade_v2_preserves_transactional_journal_steps() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261024120000000000_create_items.sql"),
        "-- dbm:up\nCREATE TABLE items (id INTEGER PRIMARY KEY);\n",
    )
    .expect("write migration fixture");
    let applied = run(directory.path(), &["up"]);
    assert!(applied.status.success());

    let database_path = directory.path().join("dbm.sqlite");
    let database = rusqlite::Connection::open(&database_path).expect("open metadata database");
    database
        .execute_batch(
            "BEGIN IMMEDIATE;\
             ALTER TABLE schema_migration_steps RENAME TO schema_migration_steps_v3;\
             CREATE TABLE schema_migration_steps (\
                 run_id TEXT NOT NULL REFERENCES schema_migration_runs(run_id),\
                 step_index INTEGER NOT NULL CHECK (step_index >= 0),\
                 step_key TEXT NOT NULL,\
                 direction TEXT NOT NULL CHECK (direction IN ('up', 'down')),\
                 statement_hash TEXT NOT NULL,\
                 statement_kind TEXT NOT NULL,\
                 transaction_class TEXT NOT NULL CHECK (transaction_class = 'transactional'),\
                 state TEXT NOT NULL CHECK (state IN ('intent_recorded', 'acknowledged', 'failed', 'indeterminate', 'reconciled')),\
                 started_at TEXT NOT NULL, acknowledged_at TEXT, error_code TEXT, evidence TEXT,\
                 attempt_id TEXT NOT NULL, PRIMARY KEY (run_id, step_index)\
             );\
             INSERT INTO schema_migration_steps SELECT * FROM schema_migration_steps_v3;\
             DROP TABLE schema_migration_steps_v3;\
             DROP TABLE schema_migration_adoptions;\
             DROP TABLE schema_migration_import_events;\
             DROP TABLE schema_migration_imports;\
             ALTER TABLE schema_migrations RENAME TO schema_migrations_v3;\
             CREATE TABLE schema_migrations (\
                 namespace TEXT NOT NULL, migration_key TEXT NOT NULL,\
                 kind TEXT NOT NULL CHECK (kind = 'versioned'),\
                 native_version TEXT NOT NULL, filename TEXT NOT NULL,\
                 source_path TEXT NOT NULL, description TEXT NOT NULL,\
                 authoring_type TEXT NOT NULL CHECK (authoring_type = 'sql'),\
                 content_checksum TEXT NOT NULL, apply_order INTEGER NOT NULL,\
                 applied_at TEXT NOT NULL, PRIMARY KEY (namespace, migration_key),\
                 UNIQUE (namespace, kind, native_version)\
             );\
             INSERT INTO schema_migrations (namespace, migration_key, kind, native_version,\
                 filename, source_path, description, authoring_type, content_checksum,\
                 apply_order, applied_at) \
             SELECT namespace, migration_key, kind, native_version, filename, source_path, \
                 description, authoring_type, content_checksum, apply_order, applied_at \
             FROM schema_migrations_v3;\
             DROP TABLE schema_migrations_v3;\
             UPDATE schema_migration_meta SET metadata_format_version = 2 WHERE singleton = 1;\
             COMMIT;",
        )
        .expect("convert fixture to metadata format 2");
    drop(database);

    let preview = run(
        directory.path(),
        &["--format", "json", "metadata", "upgrade"],
    );
    assert!(preview.status.success());
    assert_eq!(json(&preview)["from_version"], 2);
    assert_eq!(json(&preview)["status"], "requires_approval");

    let upgraded = run(
        directory.path(),
        &["--format", "json", "metadata", "upgrade", "--yes"],
    );
    assert!(upgraded.status.success());
    assert_eq!(json(&upgraded)["from_version"], 2);
    assert_eq!(json(&upgraded)["to_version"], 5);
    assert_eq!(json(&upgraded)["status"], "upgraded");

    let diagnosis = run(directory.path(), &["--format", "json", "doctor"]);
    assert!(diagnosis.status.success());
    assert_eq!(json(&diagnosis)["metadata_format_version"], 5);
    let database = rusqlite::Connection::open(&database_path).expect("read upgraded journal");
    let transaction_class: String = database
        .query_row(
            "SELECT transaction_class FROM schema_migration_steps WHERE run_id = \
             (SELECT run_id FROM schema_migration_runs ORDER BY created_at LIMIT 1)",
            [],
            |row| row.get(0),
        )
        .expect("read preserved transaction class");
    assert_eq!(transaction_class, "transactional");
}

#[cfg(unix)]
#[test]
fn killing_cli_during_a_transaction_keeps_the_next_write_blocked() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let migrations = directory.path().join("migrations/versioned");
    fs::write(
        migrations.join("20261021162643981234_crash_during_sql.sql"),
        "-- dbm:up\nCREATE TABLE crash_effect (id INTEGER);\nINSERT INTO crash_effect VALUES (1);\nWITH RECURSIVE sequence(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM sequence WHERE value < 1000000000) SELECT sum(value) FROM sequence;\n",
    )
    .expect("write slow transactional migration");

    let database_path = directory.path().join("dbm.sqlite");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["up"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start migration process");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_writer = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("check migration process") {
            panic!("slow migration exited before fault injection: {status}");
        }
        if database_path.exists() {
            let database = rusqlite::Connection::open(&database_path)
                .expect("open migration database while child runs");
            let run_exists: bool = database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migration_runs')",
                    [],
                    |row| row.get(0),
                )
                .expect("check metadata bootstrap");
            drop(database);
            if run_exists {
                let started_run: bool = rusqlite::Connection::open(&database_path)
                    .expect("open run-state observer")
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM schema_migration_runs WHERE state = 'started')",
                        [],
                        |row| row.get(0),
                    )
                    .expect("check durable started state");
                if started_run {
                    let mut probe = rusqlite::Connection::open(&database_path)
                        .expect("open lock probe connection");
                    probe
                        .busy_timeout(Duration::ZERO)
                        .expect("set non-blocking lock probe");
                    match probe.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    {
                        Ok(transaction) => {
                            transaction.rollback().expect("release unused lock probe");
                        }
                        Err(rusqlite::Error::SqliteFailure(failure, _))
                            if matches!(
                                failure.code,
                                rusqlite::ErrorCode::DatabaseBusy
                                    | rusqlite::ErrorCode::DatabaseLocked
                            ) =>
                        {
                            observed_writer = true;
                            break;
                        }
                        Err(error) => panic!("unexpected lock-probe failure: {error}"),
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        observed_writer,
        "did not observe the migration's SQLite writer lock"
    );
    thread::sleep(Duration::from_millis(100));
    child
        .kill()
        .expect("kill migration during the long SQL statement");
    let killed = child.wait().expect("reap killed migration process");
    assert!(!killed.success());

    let database = rusqlite::Connection::open(&database_path).expect("reopen recovered database");
    let applied_count: i64 = database
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("read applied migration state");
    let effect_table_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'crash_effect'",
            [],
            |row| row.get(0),
        )
        .expect("read rolled-back schema effects");
    let run_state: String = database
        .query_row(
            "SELECT state FROM schema_migration_runs ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("read interrupted run");
    let lock_state: String = database
        .query_row(
            "SELECT state FROM schema_migration_locks WHERE scope = 'database'",
            [],
            |row| row.get(0),
        )
        .expect("read retained diagnostic lock");
    assert_eq!(applied_count, 0);
    assert_eq!(effect_table_count, 0);
    assert_eq!(run_state, "started");
    assert_eq!(lock_state, "observed_active");
    drop(database);

    let retry = run(directory.path(), &["--format", "json", "up"]);
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
    let diagnosis = run(directory.path(), &["--format", "json", "doctor"]);
    assert!(diagnosis.status.success());
    let diagnosis = json(&diagnosis);
    assert_eq!(diagnosis["status"], "lock_held");
    assert_eq!(diagnosis["steps"].as_array().map(Vec::len), Some(3));
    assert!(
        diagnosis["steps"]
            .as_array()
            .expect("doctor statement steps")
            .iter()
            .all(|step| step["state"] == "indeterminate")
    );
}

#[test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn cli_runs_postgres_transactional_and_concurrent_index_migrations() {
    use std::net::IpAddr;
    use std::str::FromStr;

    use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

    let database_url = std::env::var("DBM_TEST_DATABASE_URL")
        .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
    let parsed = PostgresConfig::from_str(&database_url)
        .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
    assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
    assert!(!parsed.get_hosts().is_empty());
    assert!(parsed.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
        Host::Tcp(host) => IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        Host::Unix(_) => true,
    }));
    assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));
    assert!(
        !database_url.contains("options="),
        "the disposable PostgreSQL test URL must not already set connection options"
    );

    let schema = format!("dbm_cli_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated PostgreSQL schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read isolated current schema"),
        schema
    );

    let directory = tempfile::tempdir().expect("temporary project directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory.path().join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"postgres\"\nurl_env = \"DBM_TEST_DATABASE_URL\"\nenvironment = \"test\"\nallow_insecure_transport = true\n",
    )
    .expect("configure the disposable PostgreSQL target");
    let migrations = directory.path().join("migrations/versioned");
    fs::write(
        migrations.join("20261024120000000000_create_cli_probe.sql"),
        "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE cli_probe (email text NOT NULL);\n-- dbm:down\nDROP TABLE cli_probe;\n",
    )
    .expect("write transactional PostgreSQL migration");
    fs::write(
        migrations.join("20261024120000000001_add_cli_probe_index.sql"),
        "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE UNIQUE INDEX CONCURRENTLY cli_probe_email_idx ON cli_probe (email);\n-- dbm:down\nDROP INDEX CONCURRENTLY cli_probe_email_idx;\n",
    )
    .expect("write non-transactional PostgreSQL migration");

    let planned = run_with_environment(
        directory.path(),
        &["--format", "json", "plan"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        planned.status.success(),
        "PostgreSQL plan failed: {}",
        String::from_utf8_lossy(&planned.stdout)
    );
    assert_eq!(json(&planned)["read_only"], true);
    assert_eq!(
        json(&planned)["operations"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(!postgres_metadata_exists(&schema_url).expect("inspect PostgreSQL metadata"));

    let applied = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        applied.status.success(),
        "PostgreSQL up failed: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    assert_eq!(json(&applied)["status"], "succeeded");
    assert_eq!(
        json(&applied)["migrations"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(
        postgres_metadata_format(&schema_url).expect("read metadata format"),
        3
    );

    let diagnosis = run_with_environment(
        directory.path(),
        &["--format", "json", "doctor"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(diagnosis.status.success());
    assert_eq!(json(&diagnosis)["status"], "healthy");

    let reverted = run_with_environment(
        directory.path(),
        &["--format", "json", "down", "--steps", "2"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        reverted.status.success(),
        "PostgreSQL down failed: {}",
        String::from_utf8_lossy(&reverted.stdout)
    );
    assert_eq!(json(&reverted)["status"], "succeeded");
    assert_eq!(
        json(&reverted)["migrations"].as_array().map(Vec::len),
        Some(2)
    );

    let history = run_with_environment(
        directory.path(),
        &["--format", "json", "history"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(history.status.success());
    assert_eq!(json(&history)["runs"].as_array().map(Vec::len), Some(4));
    let current = run_with_environment(
        directory.path(),
        &["--format", "json", "status"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(current.status.success());
    assert_eq!(json(&current)["status"], "pending");
    schema_cleanup
        .clean()
        .expect("drop isolated PostgreSQL schema");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL 8.4 instance"]
fn cli_runs_mysql_nontransactional_migration_and_recovery_journey() {
    run_mysql_cli_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB LTS instance"]
fn cli_runs_mariadb_nontransactional_migration_and_recovery_journey() {
    run_mysql_cli_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires a disposable loopback TLS MySQL server and DBM_TEST_MYSQL_CA_FILE"]
fn mysql_tls_validates_the_configured_ca_and_hostname() {
    run_mysql_tls_validation("DBM_TEST_MYSQL_TLS_URL", "DBM_TEST_MYSQL_CA_FILE");
}

#[test]
#[ignore = "requires a disposable loopback TLS MariaDB server and DBM_TEST_MARIADB_CA_FILE"]
fn mariadb_tls_validates_the_configured_ca_and_hostname() {
    run_mysql_tls_validation("DBM_TEST_MARIADB_TLS_URL", "DBM_TEST_MARIADB_CA_FILE");
}

fn run_mysql_tls_validation(url_environment_variable: &str, ca_environment_variable: &str) {
    let connection_url = std::env::var(url_environment_variable).unwrap_or_else(|_| {
        panic!("{url_environment_variable} must identify a disposable TLS server")
    });
    let ca_file = std::env::var(ca_environment_variable).unwrap_or_else(|_| {
        panic!("{ca_environment_variable} must identify the local test CA file")
    });
    assert!(
        Path::new(&ca_file).is_file(),
        "configured test CA file must exist"
    );
    let parsed = mysql_async::Opts::from_url(&connection_url).expect("TLS MySQL test URL is valid");
    assert!(
        parsed.ssl_opts().is_some(),
        "TLS must be required by test URL"
    );
    assert_eq!(parsed.ip_or_hostname(), "127.0.0.1");

    let directory = tempfile::tempdir().expect("temporary TLS test project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{url_environment_variable}\"\ntls_ca_cert_env = \"{ca_environment_variable}\"\nenvironment = \"test\"\n"
        ),
    )
    .expect("configure the local TLS MySQL/MariaDB target");

    let trusted = run_with_environment(
        directory.path(),
        &["--format", "json", "plan"],
        url_environment_variable,
        &connection_url,
    );
    assert!(
        trusted.status.success(),
        "MySQL/MariaDB TLS with the configured CA failed: {}",
        String::from_utf8_lossy(&trusted.stdout)
    );
    assert_eq!(json(&trusted)["read_only"], true);

    let hostname_mismatch_url = mysql_url_with_hostname(&connection_url, "localhost");
    let rejected = run_with_environment(
        directory.path(),
        &["--format", "json", "plan"],
        url_environment_variable,
        &hostname_mismatch_url,
    );
    assert!(
        !rejected.status.success(),
        "MySQL/MariaDB TLS must reject a server certificate with a hostname mismatch"
    );
}
