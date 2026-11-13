use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
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
        let selected_database = connection
            .query_first::<Option<String>, _>("SELECT DATABASE()")
            .await
            .ok()
            .flatten();
        let value = connection
            .query_first::<i64, _>(sql)
            .await
            .map_err(|error| match error {
                mysql_async::Error::Server(server_error) => format!(
                    "read isolated MySQL/MariaDB state in database {selected_database:?} \
                     (server error {}, SQLSTATE {})",
                    server_error.code, server_error.state
                ),
                _ => format!(
                    "read isolated MySQL/MariaDB state in database {selected_database:?} \
                     (non-server driver error)"
                ),
            })?
            .ok_or_else(|| "MySQL/MariaDB returned no scalar row".to_owned())?;
        connection
            .disconnect()
            .await
            .map_err(|_| "close disposable MySQL/MariaDB connection")?;
        Ok(value)
    })
}

async fn mysql_test_check_constraint_name(
    connection: &mut mysql_async::Conn,
    table: &str,
    expected_clause: &str,
) -> String {
    use mysql_async::prelude::Queryable;

    let rows: Vec<(String, String)> = connection
        .exec(
            "SELECT table_constraints.CONSTRAINT_NAME, check_constraints.CHECK_CLAUSE \
             FROM information_schema.TABLE_CONSTRAINTS AS table_constraints \
             INNER JOIN information_schema.CHECK_CONSTRAINTS AS check_constraints \
                ON check_constraints.CONSTRAINT_SCHEMA = table_constraints.CONSTRAINT_SCHEMA \
               AND check_constraints.CONSTRAINT_NAME = table_constraints.CONSTRAINT_NAME \
             WHERE table_constraints.CONSTRAINT_SCHEMA = DATABASE() \
               AND table_constraints.TABLE_NAME = ? \
               AND table_constraints.CONSTRAINT_TYPE = 'CHECK'",
            (table,),
        )
        .await
        .expect("inspect test database metadata checks");
    let mut matches = Vec::new();
    let mut observed = Vec::new();
    for (name, clause) in rows {
        let lower = clause.to_ascii_lowercase();
        let normalized_clause = lower
            .replace("_utf8mb4", "")
            .replace("_utf8", "")
            .replace("_ascii", "")
            .replace("_latin1", "")
            .bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(char::from)
            .collect::<String>();
        observed.push(format!("{name}={normalized_clause}"));
        if normalized_clause == expected_clause {
            matches.push(name);
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "test fixture expected exactly one {expected_clause} check on {table}; observed: {}",
        observed.join(", ")
    );
    let name = matches.remove(0);
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "test fixture check constraint name must be a safe identifier"
    );
    name
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

enum MysqlProxyControl {
    Resume,
    Stop,
}

struct MysqlQueryHoldProxy {
    url: String,
    notification: std::sync::mpsc::Receiver<Result<(), String>>,
    control: std::sync::mpsc::SyncSender<MysqlProxyControl>,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

fn start_mysql_query_hold_proxy(connection_url: &str, query_fragment: &str) -> MysqlQueryHoldProxy {
    use std::io::{Read, Write};
    use std::net::{IpAddr, TcpListener, TcpStream};
    use std::str::FromStr;

    let target = mysql_async::Opts::from_url(connection_url)
        .expect("disposable MySQL connection URL is valid");
    let host = target.ip_or_hostname();
    let target_ip = if host.eq_ignore_ascii_case("localhost") {
        IpAddr::from_str("127.0.0.1").expect("IPv4 loopback address is valid")
    } else {
        IpAddr::from_str(host).expect("MySQL proxy target must use a literal loopback address")
    };
    assert!(target_ip.is_loopback());
    let target_address = std::net::SocketAddr::new(target_ip, target.tcp_port());

    let (scheme, remainder) = connection_url
        .split_once("://")
        .expect("MySQL test URL has a scheme");
    let (user_info, host_and_path) = remainder
        .split_once('@')
        .expect("MySQL test URL has a user identity");
    let (_, path) = host_and_path
        .split_once('/')
        .expect("MySQL test URL selects a database");
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("bind isolated loopback MySQL query proxy");
    listener
        .set_nonblocking(true)
        .expect("make MySQL query proxy listener nonblocking");
    let proxy_port = listener
        .local_addr()
        .expect("read MySQL query proxy address")
        .port();
    let proxy_url = format!("{scheme}://{user_info}@127.0.0.1:{proxy_port}/{path}");
    let query_fragment = query_fragment.to_ascii_lowercase();
    let (notification_sender, notification_receiver) = std::sync::mpsc::sync_channel(1);
    let (control_sender, control_receiver) = std::sync::mpsc::sync_channel(1);
    let proxy_error_sender = notification_sender.clone();
    let proxy_thread = std::thread::spawn(move || {
        let proxy_result = (|| -> Result<(), String> {
            let accept_deadline = Instant::now() + Duration::from_secs(30);
            let mut target_ddl_held = false;
            loop {
                let (mut client, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < accept_deadline =>
                        {
                            match control_receiver.try_recv() {
                                Ok(MysqlProxyControl::Stop) => return Ok(()),
                                Ok(MysqlProxyControl::Resume) => {
                                    return Err("received MySQL proxy resume without a held query"
                                        .to_owned());
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    thread::sleep(Duration::from_millis(10));
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    return Ok(());
                                }
                            }
                        }
                        Err(error) => {
                            return Err(format!("accept MySQL proxy client failed: {error}"));
                        }
                    }
                };
                client
                    .set_nodelay(true)
                    .map_err(|error| format!("configure MySQL proxy client: {error}"))?;
                let mut server = TcpStream::connect(target_address)
                    .map_err(|error| format!("connect proxy to disposable MySQL: {error}"))?;
                server
                    .set_nodelay(true)
                    .map_err(|error| format!("configure MySQL proxy server: {error}"))?;
                let mut client_writer = client
                    .try_clone()
                    .map_err(|error| format!("clone MySQL proxy client stream: {error}"))?;
                let mut server_reader = server
                    .try_clone()
                    .map_err(|error| format!("clone MySQL proxy server stream: {error}"))?;
                let server_to_client =
                    thread::spawn(move || std::io::copy(&mut server_reader, &mut client_writer));

                loop {
                    let mut header = [0_u8; 4];
                    match client.read(&mut header[..1]) {
                        Ok(0) => break,
                        Ok(1) => {}
                        Ok(_) => unreachable!("reading one byte cannot return more than one byte"),
                        Err(error) => {
                            return Err(format!("read MySQL client packet header: {error}"));
                        }
                    }
                    client
                        .read_exact(&mut header[1..])
                        .map_err(|error| format!("read MySQL client packet header: {error}"))?;
                    let payload_length = usize::from(header[0])
                        | (usize::from(header[1]) << 8)
                        | (usize::from(header[2]) << 16);
                    let mut payload = vec![0_u8; payload_length];
                    client
                        .read_exact(&mut payload)
                        .map_err(|error| format!("read MySQL client packet body: {error}"))?;
                    let is_target_query = !target_ddl_held
                        && matches!(payload.first(), Some(0x03 | 0x16))
                        && String::from_utf8_lossy(&payload[1..])
                            .to_ascii_lowercase()
                            .contains(&query_fragment);
                    if is_target_query {
                        notification_sender
                            .send(Ok(()))
                            .map_err(|_| "notify test that the MySQL DDL is held".to_owned())?;
                        match control_receiver.recv_timeout(Duration::from_secs(20)) {
                            Ok(MysqlProxyControl::Resume) => target_ddl_held = true,
                            Ok(MysqlProxyControl::Stop) => {
                                let _ = client.shutdown(std::net::Shutdown::Both);
                                let _ = server.shutdown(std::net::Shutdown::Both);
                                let _ = server_to_client.join();
                                return Ok(());
                            }
                            Err(_) => {
                                return Err("timed out waiting to release the MySQL DDL".to_owned());
                            }
                        }
                    }
                    server
                        .write_all(&header)
                        .and_then(|_| server.write_all(&payload))
                        .map_err(|error| format!("forward MySQL client packet: {error}"))?;
                }

                let _ = server.shutdown(std::net::Shutdown::Write);
                server_to_client
                    .join()
                    .map_err(|_| "MySQL server-to-client proxy thread panicked".to_owned())?
                    .map_err(|error| format!("forward MySQL server response: {error}"))?;
                if target_ddl_held {
                    return Ok(());
                }
            }
        })();
        if let Err(error) = &proxy_result {
            let _ = proxy_error_sender.send(Err(error.clone()));
        }
        proxy_result
    });
    MysqlQueryHoldProxy {
        url: proxy_url,
        notification: notification_receiver,
        control: control_sender,
        thread: proxy_thread,
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
            !is_mariadb && (version.starts_with("8.4.") || version.starts_with("9.7.")),
            "the configured integration server is not MySQL 8.4 or 9.7"
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
        5
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

fn run_mysql_cli_contention_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create MySQL contention runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
    } else {
        assert!(
            !is_mariadb && (version.starts_with("8.4.") || version.starts_with("9.7.")),
            "configured target is not a supported test MySQL line"
        );
    }

    let database = format!("dbm_g11_contention_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB contention database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary MySQL contention project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let namespace = format!("contention_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = 0\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB contention target");

    let version = "20261107130000000000";
    let probe_table = format!("cli_contention_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{version}_same_namespace.sql")),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nSELECT SLEEP(5);\nCREATE TABLE {probe_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {probe_table};\n"
        ),
    )
    .expect("write same-namespace MySQL/MariaDB contention migration");

    let mut first = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &target_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start first MySQL/MariaDB migration CLI");
    let active_deadline = Instant::now() + Duration::from_secs(15);
    let mut first_is_sleeping = false;
    while Instant::now() < active_deadline {
        let journal_table_count = mysql_scalar(
            &base_url,
            &database,
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() \
               AND table_name IN ('schema_migration_runs', 'schema_migration_steps')",
        );
        if journal_table_count == Ok(2) {
            let intent_count = mysql_scalar(
                &base_url,
                &database,
                &format!(
                    "SELECT COUNT(*) FROM schema_migration_steps AS step \
                     INNER JOIN schema_migration_runs AS run USING (run_id) \
                     WHERE run.namespace = '{namespace}' \
                       AND run.migration_version = '{version}' \
                       AND run.state = 'started' AND step.state = 'intent_recorded'"
                ),
            )
            .unwrap_or(0);
            let sleeping_session = mysql_scalar(
                &base_url,
                &database,
                "SELECT COUNT(*) FROM information_schema.processlist \
                 WHERE DB = DATABASE() AND INFO LIKE '%SELECT SLEEP(5)%'",
            )
            .unwrap_or(0);
            if intent_count == 1 && sleeping_session == 1 {
                first_is_sleeping = true;
                break;
            }
        }
        if !matches!(first.try_wait(), Ok(None)) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    if !first_is_sleeping {
        let _ = first.kill();
        let output = first
            .wait_with_output()
            .expect("reap first MySQL CLI after failed contention setup");
        database_cleanup
            .clean()
            .expect("remove isolated database after failed contention setup");
        panic!(
            "first CLI did not reach its sleeping statement; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut competing = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &target_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start competing MySQL/MariaDB migration CLI");
    let competing_deadline = Instant::now() + Duration::from_secs(10);
    let mut competing_completed = false;
    while Instant::now() < competing_deadline {
        if competing
            .try_wait()
            .expect("inspect competing MySQL/MariaDB CLI")
            .is_some()
        {
            competing_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !competing_completed {
        let _ = competing.kill();
    }
    let competing_output = competing
        .wait_with_output()
        .expect("collect competing MySQL/MariaDB CLI output");

    let first_deadline = Instant::now() + Duration::from_secs(12);
    let mut first_completed = false;
    while Instant::now() < first_deadline {
        if first
            .try_wait()
            .expect("inspect first MySQL/MariaDB CLI completion")
            .is_some()
        {
            first_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !first_completed {
        let _ = first.kill();
    }
    let first_output = first
        .wait_with_output()
        .expect("collect first MySQL/MariaDB CLI output");

    let run_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{namespace}' AND migration_version = '{version}'"
        ),
    )
    .expect("count same-namespace migration runs");
    let succeeded_run_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{namespace}' AND migration_version = '{version}' \
               AND state = 'succeeded'"
        ),
    )
    .expect("count successful same-namespace migration runs");
    let table_exists = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{probe_table}'"
        ),
    )
    .expect("inspect same-namespace migration effect");
    let cleanup = database_cleanup.clean();

    assert!(
        competing_completed,
        "competing CLI did not honor the zero-second lock timeout"
    );
    assert_eq!(competing_output.status.code(), Some(5));
    assert_eq!(
        json(&competing_output)["errors"][0]["code"],
        "LOCK_UNAVAILABLE"
    );
    assert!(
        first_completed,
        "first CLI did not finish after its sleeping statement"
    );
    assert!(
        first_output.status.success(),
        "first migration CLI failed: {}",
        String::from_utf8_lossy(&first_output.stdout)
    );
    assert_eq!(
        run_count, 1,
        "the competing CLI must not create another run"
    );
    assert_eq!(succeeded_run_count, 1);
    assert_eq!(
        table_exists, 1,
        "the first migration should apply exactly once"
    );
    cleanup.expect("drop isolated MySQL/MariaDB contention database");
}

fn is_tested_mariadb_line(server_version: &str) -> bool {
    for prefix in ["10.11.", "11.4.", "11.8.", "13.0."] {
        if server_version.starts_with(prefix) {
            return true;
        }
    }

    false
}

fn run_mysql_shared_schema_namespaces_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create namespace-race runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
        assert!(
            is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_namespaces_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated shared-schema namespace database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary shared-schema namespace project");
    let first_project = directory.path().join("first");
    let second_project = directory.path().join("second");
    fs::create_dir(&first_project).expect("create first namespace project");
    fs::create_dir(&second_project).expect("create second namespace project");
    let first_initialized = run(&first_project, &["init"]);
    let second_initialized = run(&second_project, &["init"]);
    assert!(first_initialized.status.success());
    assert!(second_initialized.status.success());
    let first_namespace = format!("ns_a_{}", uuid::Uuid::new_v4().simple());
    let second_namespace = format!("ns_b_{}", uuid::Uuid::new_v4().simple());
    let first_config = format!(
        "version = 1\nnamespace = \"{first_namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = 15\n"
    );
    let second_config = format!(
        "version = 1\nnamespace = \"{second_namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = 15\n"
    );
    fs::write(first_project.join("dbm.toml"), first_config)
        .expect("configure first namespace project");
    fs::write(second_project.join("dbm.toml"), second_config)
        .expect("configure second namespace project");
    let migration_version = "20261107160000000000";
    let shared_table = format!("shared_namespace_object_{}", uuid::Uuid::new_v4().simple());
    let migration = format!(
        "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE {shared_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {shared_table};\n"
    );
    fs::write(
        first_project.join(format!(
            "migrations/versioned/{migration_version}_shared.sql"
        )),
        &migration,
    )
    .expect("write first shared-object migration");
    fs::write(
        second_project.join(format!(
            "migrations/versioned/{migration_version}_shared.sql"
        )),
        migration,
    )
    .expect("write second shared-object migration");

    let proxy = start_mysql_query_hold_proxy(&target_url, &shared_table);
    let mut first = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(&first_project)
        .args(["--format", "json", "up"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start first shared-schema namespace CLI");
    let notification = proxy.notification.recv_timeout(Duration::from_secs(15));
    let first_statement_held = matches!(&notification, Ok(Ok(())));
    if !first_statement_held {
        let _ = first.kill();
        let first_output = first
            .wait_with_output()
            .expect("collect first CLI output after proxy setup failure");
        let _ = proxy.control.send(MysqlProxyControl::Stop);
        let proxy_result = proxy
            .thread
            .join()
            .expect("shared-schema proxy thread did not panic");
        let cleanup = database_cleanup.clean();
        let notification_detail = match notification {
            Ok(Ok(())) => "held target SQL".to_owned(),
            Ok(Err(error)) => error,
            Err(error) => format!("no target statement notification: {error}"),
        };
        panic!(
            "first CLI did not reach shared-object DDL ({notification_detail}); stdout={}, stderr={}, proxy={proxy_result:?}, cleanup={cleanup:?}",
            String::from_utf8_lossy(&first_output.stdout),
            String::from_utf8_lossy(&first_output.stderr)
        );
    }

    let first_intent_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{first_namespace}' \
               AND run.migration_version = '{migration_version}' \
               AND run.state = 'started' AND step.state = 'intent_recorded'"
        ),
    )
    .expect("inspect first namespace intent while DDL is held");

    let mut second = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(&second_project)
        .args(["--format", "json", "up"])
        .env(environment_variable, &target_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start competing second namespace CLI");
    let lock_wait_deadline = Instant::now() + Duration::from_secs(8);
    let mut second_waiting_for_database_lock = false;
    let mut second_runs_before_release = -1;
    while Instant::now() < lock_wait_deadline {
        let lock_waiters = mysql_scalar(
            &base_url,
            &database,
            "SELECT COUNT(*) FROM information_schema.processlist \
             WHERE DB = DATABASE() AND INFO LIKE '%GET_LOCK%'",
        )
        .unwrap_or(0);
        second_runs_before_release = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_runs WHERE namespace = '{second_namespace}'"
            ),
        )
        .unwrap_or(-1);
        if lock_waiters > 0 {
            second_waiting_for_database_lock = true;
            break;
        }
        if second
            .try_wait()
            .expect("inspect second namespace CLI while the first DDL is held")
            .is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let _ = proxy.control.send(if second_waiting_for_database_lock {
        MysqlProxyControl::Resume
    } else {
        MysqlProxyControl::Stop
    });
    if !second_waiting_for_database_lock {
        let _ = first.kill();
        let _ = second.kill();
    }

    let first_deadline = Instant::now() + Duration::from_secs(15);
    let mut first_completed = false;
    while Instant::now() < first_deadline {
        if first
            .try_wait()
            .expect("inspect first namespace CLI completion")
            .is_some()
        {
            first_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !first_completed {
        let _ = first.kill();
    }
    let first_output = first
        .wait_with_output()
        .expect("collect first namespace CLI output");
    let second_deadline = Instant::now() + Duration::from_secs(20);
    let mut second_completed = false;
    while Instant::now() < second_deadline {
        if second
            .try_wait()
            .expect("inspect second namespace CLI completion")
            .is_some()
        {
            second_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !second_completed {
        let _ = second.kill();
    }
    let second_output = second
        .wait_with_output()
        .expect("collect second namespace CLI output");
    let proxy_result = proxy
        .thread
        .join()
        .expect("shared-schema proxy thread did not panic");

    let first_succeeded_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{first_namespace}' AND migration_version = '{migration_version}' \
               AND state = 'succeeded'"
        ),
    )
    .expect("inspect first namespace succeeded run");
    let second_indeterminate_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{second_namespace}' AND migration_version = '{migration_version}' \
               AND state = 'indeterminate'"
        ),
    )
    .expect("inspect second namespace indeterminate run");
    let second_indeterminate_step_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{second_namespace}' \
               AND run.migration_version = '{migration_version}' \
               AND step.state = 'indeterminate'"
        ),
    )
    .expect("inspect second namespace indeterminate step");
    let first_applied_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{first_namespace}' AND native_version = '{migration_version}'"
        ),
    )
    .expect("inspect first namespace applied row");
    let second_applied_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{second_namespace}' AND native_version = '{migration_version}'"
        ),
    )
    .expect("inspect second namespace applied row");
    let shared_table_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{shared_table}'"
        ),
    )
    .expect("inspect shared MySQL/MariaDB schema object");
    let cleanup = database_cleanup.clean();

    assert_eq!(first_intent_count, 1);
    assert!(
        second_waiting_for_database_lock,
        "second namespace CLI did not wait for the database GET_LOCK"
    );
    assert_eq!(second_runs_before_release, 0);
    assert!(first_completed, "first namespace CLI did not finish");
    assert!(first_output.status.success());
    assert!(second_completed, "second namespace CLI did not finish");
    assert_eq!(second_output.status.code(), Some(7));
    assert_eq!(
        json(&second_output)["errors"][0]["code"],
        "RECONCILIATION_REQUIRED"
    );
    assert_eq!(first_succeeded_count, 1);
    assert_eq!(second_indeterminate_count, 1);
    assert_eq!(second_indeterminate_step_count, 1);
    assert_eq!(first_applied_count, 1);
    assert_eq!(second_applied_count, 0);
    assert_eq!(shared_table_count, 1);
    proxy_result.expect("MySQL/MariaDB shared-schema proxy failed");
    cleanup.expect("drop isolated shared-schema namespace database");
}

fn run_mysql_metadata_bootstrap_failure_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create metadata-bootstrap runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
        assert!(
            is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_bootstrap_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB bootstrap database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary metadata-bootstrap project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let namespace = format!("bootstrap_failure_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB bootstrap target");
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261107140000000000_bootstrap_failure.sql"),
        "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE bootstrap_effect (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE bootstrap_effect;\n",
    )
    .expect("write metadata-bootstrap fault migration");

    let proxy =
        start_mysql_query_hold_proxy(&target_url, "CREATE TABLE schema_migration_import_events");
    let mut first = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MySQL/MariaDB metadata-bootstrap CLI");
    let ddl_notification = proxy.notification.recv_timeout(Duration::from_secs(20));
    let ddl_held = matches!(&ddl_notification, Ok(Ok(())));
    let notification_detail = match ddl_notification {
        Ok(Ok(())) => "held target DDL".to_owned(),
        Ok(Err(error)) => error,
        Err(error) => format!("no target DDL notification: {error}"),
    };
    let injected_conflict = if ddl_held {
        mysql_execute(
            &target_url,
            "CREATE TABLE schema_migration_import_events (id BIGINT PRIMARY KEY) ENGINE=InnoDB",
        )
    } else {
        Err("CLI did not reach the third metadata DDL".to_owned())
    };
    let control = if ddl_held {
        MysqlProxyControl::Resume
    } else {
        MysqlProxyControl::Stop
    };
    let _ = proxy.control.send(control);

    let first_deadline = Instant::now() + Duration::from_secs(20);
    let mut first_completed = false;
    while Instant::now() < first_deadline {
        if first
            .try_wait()
            .expect("inspect metadata-bootstrap CLI")
            .is_some()
        {
            first_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !first_completed {
        let _ = first.kill();
    }
    let failed = first
        .wait_with_output()
        .expect("collect metadata-bootstrap CLI output");
    let proxy_result = proxy
        .thread
        .join()
        .expect("MySQL metadata-bootstrap proxy thread did not panic");

    let failed_error_code = json(&failed)["errors"][0]["code"]
        .as_str()
        .unwrap_or("NO_ERROR")
        .to_owned();
    let failed_error_message = json(&failed)["errors"][0]["message"]
        .as_str()
        .unwrap_or("no error message")
        .to_owned();
    assert!(
        ddl_held,
        "proxy did not pause the third metadata DDL ({notification_detail}); CLI exit={:?}, code={failed_error_code}, message={failed_error_message}, proxy={proxy_result:?}",
        failed.status.code()
    );
    injected_conflict.expect("create deterministic conflict while third DDL is held");
    proxy_result.expect("MySQL metadata-bootstrap proxy failed");
    assert!(first_completed, "metadata-bootstrap CLI did not finish");

    let metadata_prefix_count = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() \
           AND table_name IN ('schema_migration_meta', 'schema_migration_imports')",
    )
    .expect("inspect completed prefix of metadata bootstrap");
    let reserved_table_count = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name IN (\
             'schema_migration_meta', 'schema_migration_imports', \
             'schema_migration_import_events', 'schema_migration_adoptions', \
             'schema_migrations', 'schema_migration_runs', \
             'schema_migration_steps', 'schema_migration_locks')",
    )
    .expect("count reserved metadata tables after bootstrap failure");
    let metadata_identity_count = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM schema_migration_meta",
    )
    .expect("inspect metadata identity after bootstrap failure");
    let migration_effect_count = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name = 'bootstrap_effect'",
    )
    .expect("inspect migration effect after bootstrap failure");

    let collision_table_count = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() \
           AND table_name = 'schema_migration_import_events'",
    )
    .expect("inspect externally-created third metadata table");
    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    let reserved_table_count_after_retry = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name IN (\
             'schema_migration_meta', 'schema_migration_imports', \
             'schema_migration_import_events', 'schema_migration_adoptions', \
             'schema_migrations', 'schema_migration_runs', \
             'schema_migration_steps', 'schema_migration_locks')",
    )
    .expect("count metadata tables after refused automatic retry");
    let cleanup = database_cleanup.clean();

    assert_eq!(failed.status.code(), Some(7));
    assert_eq!(
        json(&failed)["errors"][0]["code"],
        "RECONCILIATION_REQUIRED"
    );
    assert_eq!(metadata_prefix_count, 2);
    assert_eq!(reserved_table_count, 3);
    assert_eq!(metadata_identity_count, 0);
    assert_eq!(migration_effect_count, 0);
    assert_eq!(collision_table_count, 1);
    assert_eq!(retry.status.code(), Some(4));
    assert_eq!(json(&retry)["errors"][0]["code"], "STATE_DIVERGED");
    assert_eq!(reserved_table_count_after_retry, 3);
    cleanup.expect("drop isolated MySQL/MariaDB bootstrap database");
}

fn run_mysql_metadata_upgrade_failure_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create metadata-upgrade runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(
            is_mariadb && is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_upgrade_fault_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB metadata-upgrade database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary metadata-upgrade project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB metadata-upgrade target");
    let effect_table = format!("upgrade_fault_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261107180000000000_upgrade_fault.sql"),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE {effect_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {effect_table};\n"
        ),
    )
    .expect("write a migration to seed metadata and run history");

    let applied = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    assert!(
        applied.status.success(),
        "MySQL/MariaDB fixture migration failed: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    assert_eq!(
        mysql_scalar(
            &base_url,
            &database,
            "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1"
        )
        .expect("read initial v5 metadata identity"),
        5
    );

    runtime.block_on(async {
        let mut connection = mysql_async::Conn::new(mysql_options(&target_url, Some(&database)))
            .await
            .expect("connect to downgrade only the isolated v5 fixture");
        let kind_check = mysql_test_check_constraint_name(
            &mut connection,
            "schema_migrations",
            "kindinversionedrepeatablelegacy",
        )
        .await;
        let shape_check = mysql_test_check_constraint_name(
            &mut connection,
            "schema_migrations",
            "kindversionedandnativeversionisnotnullandrepeatablenameisnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindrepeatableandnativeversionisnullandrepeatablenameisnotnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindlegacyandnativeversionisnullandrepeatablenameisnullandoriginalversionisnotnullandlegacyimportidisnotnullandorigintoolisnotnullandorigindatabaseidentityisnotnullandoriginidentityisnotnullandverificationstatusisnotnullandobservedatisnotnull",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migrations \
                    DROP CONSTRAINT `{kind_check}`, \
                    DROP CONSTRAINT `{shape_check}`, \
                    DROP INDEX schema_migrations_namespace_repeatable_uq, \
                    DROP COLUMN repeatable_name, \
                    ADD CONSTRAINT schema_migrations_kind_check \
                        CHECK (kind IN ('versioned', 'legacy')), \
                    ADD CONSTRAINT schema_migrations_legacy_shape_check CHECK (\
                        (kind = 'versioned' AND native_version IS NOT NULL \
                         AND original_version IS NULL AND legacy_import_id IS NULL \
                         AND origin_tool IS NULL) OR \
                        (kind = 'legacy' AND native_version IS NULL \
                         AND original_version IS NOT NULL AND legacy_import_id IS NOT NULL \
                         AND origin_tool IS NOT NULL AND origin_database_identity IS NOT NULL \
                         AND origin_identity IS NOT NULL AND verification_status IS NOT NULL \
                         AND observed_at IS NOT NULL))"
            ))
            .await
            .expect("restore the v4 migration table contract");

        let direction_check = mysql_test_check_constraint_name(
            &mut connection,
            "schema_migration_runs",
            "directioninupdownrepeatreconcile",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migration_runs \
                    DROP CONSTRAINT `{direction_check}`, \
                    MODIFY COLUMN migration_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
                    ADD CONSTRAINT schema_migration_runs_direction_check \
                        CHECK (direction IN ('up', 'down', 'reconcile'))"
            ))
            .await
            .expect("restore the v4 run journal contract");

        let metadata_format_check = mysql_test_check_constraint_name(
            &mut connection,
            "schema_migration_meta",
            "metadataformatversion5",
        )
        .await;
        connection
            .query_drop(format!(
                "ALTER TABLE schema_migration_meta DROP CONSTRAINT `{metadata_format_check}`"
            ))
            .await
            .expect("remove the v5-only metadata format check");
        connection
            .query_drop(
                "UPDATE schema_migration_meta SET metadata_format_version = 4 WHERE singleton = 1",
            )
            .await
            .expect("set the isolated v4 metadata identity");
        connection
            .query_drop(
                "ALTER TABLE schema_migration_meta \
                    ADD CONSTRAINT schema_migration_meta_format_check \
                    CHECK (metadata_format_version = 4)",
            )
            .await
            .expect("restore the v4 metadata format check");
        connection
            .disconnect()
            .await
            .expect("close fixture downgrade connection");
    });

    assert_eq!(
        mysql_scalar(
            &base_url,
            &database,
            "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1"
        )
        .expect("verify v4 metadata fixture"),
        4
    );
    assert_eq!(
        mysql_scalar(&base_url, &database, "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = 'schema_migrations' AND column_name = 'repeatable_name'")
            .expect("verify the v4 fixture has no repeatable column"),
        0
    );

    let proxy = start_mysql_query_hold_proxy(&target_url, "alter table schema_migration_runs");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "metadata", "upgrade", "--yes"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start metadata-upgrade CLI behind loopback MySQL proxy");
    let notification = proxy.notification.recv_timeout(Duration::from_secs(30));
    let second_ddl_held = matches!(&notification, Ok(Ok(())));
    let _ = proxy.control.send(MysqlProxyControl::Stop);

    let process_deadline = Instant::now() + Duration::from_secs(30);
    let mut child_completed = false;
    while Instant::now() < process_deadline {
        if child
            .try_wait()
            .expect("inspect metadata-upgrade CLI after DDL interruption")
            .is_some()
        {
            child_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !child_completed {
        let _ = child.kill();
    }
    let failed = child
        .wait_with_output()
        .expect("collect interrupted metadata-upgrade CLI output");
    let proxy_result = proxy
        .thread
        .join()
        .expect("MySQL metadata-upgrade proxy thread did not panic");

    let metadata_version_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1",
    )
    .expect("read metadata identity after partial DDL upgrade");
    let repeatable_column_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = 'schema_migrations' \
           AND column_name = 'repeatable_name'",
    )
    .expect("inspect first committed metadata DDL");
    let migration_version_length_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT CHARACTER_MAXIMUM_LENGTH FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = 'schema_migration_runs' \
           AND column_name = 'migration_version'",
    )
    .expect("inspect the not-yet-upgraded run journal");
    let staging_table_count_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name IN \
           ('schema_migration_meta_v5_stage', 'schema_migration_meta_v4_backup')",
    )
    .expect("inspect v5 staging tables after DDL interruption");
    let applied_count_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM schema_migrations WHERE native_version = '20261107180000000000'",
    )
    .expect("verify the native migration row survives partial upgrade");
    let run_count_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM schema_migration_runs WHERE migration_version = '20261107180000000000'",
    )
    .expect("verify the migration run survives partial upgrade");
    let step_count_after_failure = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM schema_migration_steps AS step \
         INNER JOIN schema_migration_runs AS run USING (run_id) \
         WHERE run.migration_version = '20261107180000000000'",
    )
    .expect("verify the journal step survives partial upgrade");
    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "metadata", "upgrade", "--yes"],
        environment_variable,
        &target_url,
    );
    let metadata_version_after_retry = mysql_scalar(
        &base_url,
        &database,
        "SELECT metadata_format_version FROM schema_migration_meta WHERE singleton = 1",
    )
    .expect("verify retry did not change the metadata identity");
    let repeatable_column_after_retry = mysql_scalar(
        &base_url,
        &database,
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = 'schema_migrations' \
           AND column_name = 'repeatable_name'",
    )
    .expect("verify retry did not mutate the partial migration table");
    let cleanup = database_cleanup.clean();

    let notification_detail = match notification {
        Ok(Ok(())) => "second metadata DDL held before forwarding".to_owned(),
        Ok(Err(error)) => error,
        Err(error) => format!("no second metadata DDL notification: {error}"),
    };
    assert!(
        second_ddl_held,
        "proxy did not hold the run-journal DDL ({notification_detail}); proxy={proxy_result:?}"
    );
    assert!(
        child_completed,
        "metadata-upgrade CLI did not finish after disconnect"
    );
    proxy_result.expect("MySQL metadata-upgrade proxy failed");
    assert_eq!(failed.status.code(), Some(7));
    assert_eq!(
        json(&failed)["errors"][0]["code"],
        "RECONCILIATION_REQUIRED"
    );
    assert_eq!(metadata_version_after_failure, 4);
    assert_eq!(repeatable_column_after_failure, 1);
    assert_eq!(migration_version_length_after_failure, 20);
    assert_eq!(staging_table_count_after_failure, 0);
    assert_eq!(applied_count_after_failure, 1);
    assert_eq!(run_count_after_failure, 1);
    assert_eq!(step_count_after_failure, 1);
    assert!(
        !retry.status.success(),
        "partial metadata upgrade must not retry automatically"
    );
    assert_eq!(retry.status.code(), Some(4));
    assert_eq!(json(&retry)["errors"][0]["code"], "STATE_DIVERGED");
    assert_eq!(metadata_version_after_retry, 4);
    assert_eq!(repeatable_column_after_retry, 1);
    cleanup.expect("drop isolated MySQL/MariaDB metadata-upgrade database");
}

fn run_mysql_post_intent_before_sql_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create post-intent runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
        assert!(
            is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_intent_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB post-intent database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary post-intent project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let namespace = format!("post_intent_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB post-intent target");
    let migration_version = "20261107150000000000";
    let effect_table = format!("post_intent_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{migration_version}_post_intent.sql")),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE {effect_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {effect_table};\n"
        ),
    )
    .expect("write post-intent non-transactional migration");

    let proxy = start_mysql_query_hold_proxy(&target_url, &effect_table);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start MySQL/MariaDB CLI behind statement proxy");
    let notification = proxy.notification.recv_timeout(Duration::from_secs(15));
    let statement_held = matches!(&notification, Ok(Ok(())));
    let mut intent_recorded = false;
    let intent_deadline = Instant::now() + Duration::from_secs(10);
    while statement_held && Instant::now() < intent_deadline {
        let intent_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_steps AS step \
                 INNER JOIN schema_migration_runs AS run USING (run_id) \
                 WHERE run.namespace = '{namespace}' \
                   AND run.migration_version = '{migration_version}' \
                   AND run.state = 'started' AND step.state = 'intent_recorded'"
            ),
        )
        .expect("inspect durable MySQL/MariaDB statement intent");
        if intent_count == 1 {
            intent_recorded = true;
            break;
        }
        if child.try_wait().expect("poll post-intent CLI").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let child_was_running = child
        .try_wait()
        .expect("inspect post-intent CLI before termination")
        .is_none();
    if child_was_running {
        child.kill().expect("kill CLI while migration SQL is held");
    }
    let child_status = child.wait().expect("reap post-intent CLI");
    let _ = proxy.control.send(MysqlProxyControl::Stop);
    let proxy_result = proxy
        .thread
        .join()
        .expect("MySQL/MariaDB statement proxy thread did not panic");

    let lock_name = mysql_lock_name(&database);
    let lock_deadline = Instant::now() + Duration::from_secs(10);
    let mut test_connection_owns_lock = false;
    while Instant::now() < lock_deadline {
        let result: Option<(Option<i64>,)> = runtime
            .block_on(admin_connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
            .expect("probe release of the terminated MySQL session lock");
        if result == Some((Some(1),)) {
            test_connection_owns_lock = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        test_connection_owns_lock,
        "terminated MySQL session lock remained held"
    );
    runtime
        .block_on(admin_connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release the MySQL lock probe");

    let run_state = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{namespace}' AND migration_version = '{migration_version}' \
               AND state = 'started'"
        ),
    )
    .expect("inspect unresolved run after post-intent process death");
    let step_state = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace}' \
               AND run.migration_version = '{migration_version}' \
               AND run.state = 'started' AND step.state = 'intent_recorded'"
        ),
    )
    .expect("inspect durable intent after process death");
    let effect_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{effect_table}'"
        ),
    )
    .expect("inspect migration effect after SQL was withheld");
    let applied_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{namespace}' AND native_version = '{migration_version}'"
        ),
    )
    .expect("count applied migration records after process death");

    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    let cleanup = database_cleanup.clean();

    let notification_detail = match notification {
        Ok(Ok(())) => "statement held before forwarding".to_owned(),
        Ok(Err(error)) => error,
        Err(error) => format!("no statement notification: {error}"),
    };
    assert!(
        statement_held,
        "proxy did not hold migration SQL ({notification_detail}); proxy={proxy_result:?}"
    );
    assert!(
        intent_recorded,
        "statement intent was not durable before SQL"
    );
    assert!(
        child_was_running,
        "CLI exited before it could be terminated"
    );
    assert!(
        !child_status.success(),
        "killed CLI must not report success"
    );
    proxy_result.expect("MySQL/MariaDB statement proxy failed");
    assert_eq!(run_state, 1);
    assert_eq!(step_state, 1);
    assert_eq!(effect_count, 0);
    assert_eq!(applied_count, 0);
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
    cleanup.expect("drop isolated MySQL/MariaDB post-intent database");
}

fn run_mysql_before_intent_journey(environment_variable: &str, expected_vendor: &str) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create pre-intent runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
        assert!(
            is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_pre_intent_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB pre-intent database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary pre-intent project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let namespace = format!("pre_intent_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB pre-intent target");
    let migration_version = "20261107151000000000";
    let effect_table = format!("pre_intent_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{migration_version}_pre_intent.sql")),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE {effect_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {effect_table};\n"
        ),
    )
    .expect("write pre-intent non-transactional migration");

    let proxy = start_mysql_query_hold_proxy(&target_url, "INSERT INTO schema_migration_steps");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start MySQL/MariaDB CLI behind pre-intent proxy");
    let notification = proxy.notification.recv_timeout(Duration::from_secs(15));
    let insert_held = matches!(&notification, Ok(Ok(())));
    let mut started_run_without_step = false;
    let run_deadline = Instant::now() + Duration::from_secs(10);
    while insert_held && Instant::now() < run_deadline {
        let started_run_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_runs \
                 WHERE namespace = '{namespace}' \
                   AND migration_version = '{migration_version}' AND state = 'started'"
            ),
        )
        .expect("inspect started run before the intent insert");
        let step_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_steps AS step \
                 INNER JOIN schema_migration_runs AS run USING (run_id) \
                 WHERE run.namespace = '{namespace}' \
                   AND run.migration_version = '{migration_version}'"
            ),
        )
        .expect("inspect journal before the intent insert");
        if started_run_count == 1 && step_count == 0 {
            started_run_without_step = true;
            break;
        }
        if child.try_wait().expect("poll pre-intent CLI").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let child_was_running = child
        .try_wait()
        .expect("inspect pre-intent CLI before termination")
        .is_none();
    if child_was_running {
        child
            .kill()
            .expect("kill CLI before the intent insert is forwarded");
    }
    let child_status = child.wait().expect("reap pre-intent CLI");
    let _ = proxy.control.send(MysqlProxyControl::Stop);
    let proxy_result = proxy
        .thread
        .join()
        .expect("MySQL/MariaDB pre-intent proxy thread did not panic");

    let lock_name = mysql_lock_name(&database);
    let lock_deadline = Instant::now() + Duration::from_secs(10);
    let mut test_connection_owns_lock = false;
    while Instant::now() < lock_deadline {
        let result: Option<(Option<i64>,)> = runtime
            .block_on(admin_connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
            .expect("probe release of the terminated MySQL session lock");
        if result == Some((Some(1),)) {
            test_connection_owns_lock = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        test_connection_owns_lock,
        "terminated MySQL session lock remained held before intent"
    );
    runtime
        .block_on(admin_connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release the pre-intent MySQL lock probe");

    let run_state = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{namespace}' AND migration_version = '{migration_version}' \
               AND state = 'started'"
        ),
    )
    .expect("inspect unresolved pre-intent run");
    let step_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace}' \
               AND run.migration_version = '{migration_version}'"
        ),
    )
    .expect("count durable pre-intent steps");
    let effect_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{effect_table}'"
        ),
    )
    .expect("inspect pre-intent migration effect");
    let applied_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{namespace}' AND native_version = '{migration_version}'"
        ),
    )
    .expect("count applied migrations after pre-intent process death");
    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    let cleanup = database_cleanup.clean();

    let notification_detail = match notification {
        Ok(Ok(())) => "intent insert held before forwarding".to_owned(),
        Ok(Err(error)) => error,
        Err(error) => format!("no intent-insert notification: {error}"),
    };
    assert!(
        insert_held,
        "proxy did not hold the intent insert ({notification_detail}); proxy={proxy_result:?}"
    );
    assert!(
        started_run_without_step,
        "expected a started run and no durable step before forwarding the intent insert"
    );
    assert!(
        child_was_running,
        "CLI exited before it could be terminated"
    );
    assert!(
        !child_status.success(),
        "killed CLI must not report success"
    );
    proxy_result.expect("MySQL/MariaDB pre-intent proxy failed");
    assert_eq!(run_state, 1);
    assert_eq!(step_count, 0);
    assert_eq!(effect_count, 0);
    assert_eq!(applied_count, 0);
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
    cleanup.expect("drop isolated MySQL/MariaDB pre-intent database");
}

#[derive(Clone, Copy)]
enum MysqlAcknowledgementBoundary {
    BeforeAcknowledgement,
    BeforeFinalization,
}

impl MysqlAcknowledgementBoundary {
    fn query_fragment(self) -> &'static str {
        match self {
            Self::BeforeAcknowledgement => {
                "UPDATE schema_migration_steps SET state = 'acknowledged'"
            }
            Self::BeforeFinalization => "INSERT INTO schema_migrations",
        }
    }

    fn expected_step_state(self) -> &'static str {
        match self {
            Self::BeforeAcknowledgement => "intent_recorded",
            Self::BeforeFinalization => "acknowledged",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::BeforeAcknowledgement => "pre-acknowledgement",
            Self::BeforeFinalization => "pre-finalization",
        }
    }
}

fn run_mysql_acknowledgement_boundary_journey(
    environment_variable: &str,
    expected_vendor: &str,
    boundary: MysqlAcknowledgementBoundary,
) {
    use std::net::IpAddr;
    use std::str::FromStr;

    use mysql_async::prelude::Queryable;

    let base_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable loopback target")
    });
    let parsed = mysql_async::Opts::from_url(&base_url)
        .unwrap_or_else(|_| panic!("{environment_variable} must be a valid MySQL URL"));
    assert!(parsed.ssl_opts().is_none());
    let host = parsed.ip_or_hostname();
    assert!(
        host.eq_ignore_ascii_case("localhost")
            || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        "MySQL/MariaDB integration tests require an explicit loopback host"
    );

    let runtime = tokio::runtime::Runtime::new().expect("create acknowledgement runtime");
    let mut admin_connection = runtime
        .block_on(mysql_async::Conn::new(mysql_options(&base_url, None)))
        .expect("connect to disposable MySQL/MariaDB");
    let server_version: String = runtime
        .block_on(admin_connection.query_first("SELECT VERSION()"))
        .expect("read disposable server version")
        .expect("server version row");
    let is_mariadb = server_version.to_ascii_lowercase().contains("mariadb");
    if expected_vendor.eq_ignore_ascii_case("mariadb") {
        assert!(is_mariadb, "configured target is not MariaDB");
        assert!(
            is_tested_mariadb_line(&server_version),
            "configured target is not a tested MariaDB line"
        );
    } else {
        assert!(
            !is_mariadb
                && (server_version.starts_with("8.4.") || server_version.starts_with("9.7.")),
            "configured target is not a tested MySQL line"
        );
    }

    let database = format!("dbm_g11_ack_{}", uuid::Uuid::new_v4().simple());
    runtime
        .block_on(admin_connection.query_drop(format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        )))
        .expect("create isolated MySQL/MariaDB acknowledgement database");
    let mut database_cleanup = MySqlDatabaseCleanup {
        connection_url: base_url.clone(),
        database: database.clone(),
        cleaned: false,
    };
    let target_url = mysql_database_url(&base_url, &database);

    let directory = tempfile::tempdir().expect("temporary acknowledgement project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let namespace = format!("ack_boundary_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"mysql\"\nurl_env = \"{environment_variable}\"\nenvironment = \"test\"\nallow_insecure_transport = true\n"
        ),
    )
    .expect("configure isolated MySQL/MariaDB acknowledgement target");
    let migration_version = "20261107152000000000";
    let effect_table = format!("ack_boundary_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{migration_version}_ack_boundary.sql")),
        format!(
            "-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE {effect_table} (id BIGINT PRIMARY KEY);\n-- dbm:down\nDROP TABLE {effect_table};\n"
        ),
    )
    .expect("write acknowledgement-boundary migration");

    let proxy = start_mysql_query_hold_proxy(&target_url, boundary.query_fragment());
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env(environment_variable, &proxy.url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start MySQL/MariaDB CLI behind acknowledgement proxy");
    let notification = proxy.notification.recv_timeout(Duration::from_secs(15));
    let statement_held = matches!(&notification, Ok(Ok(())));
    let mut boundary_state_observed = false;
    let state_deadline = Instant::now() + Duration::from_secs(10);
    while statement_held && Instant::now() < state_deadline {
        let run_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_runs \
                 WHERE namespace = '{namespace}' \
                   AND migration_version = '{migration_version}' AND state = 'started'"
            ),
        )
        .expect("inspect run at acknowledgement boundary");
        let expected_step_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migration_steps AS step \
                 INNER JOIN schema_migration_runs AS run USING (run_id) \
                 WHERE run.namespace = '{namespace}' \
                   AND run.migration_version = '{migration_version}' \
                   AND step.state = '{}'",
                boundary.expected_step_state()
            ),
        )
        .expect("inspect durable step at acknowledgement boundary");
        let effect_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name = '{effect_table}'"
            ),
        )
        .expect("inspect completed SQL at acknowledgement boundary");
        let applied_count = mysql_scalar(
            &base_url,
            &database,
            &format!(
                "SELECT COUNT(*) FROM schema_migrations \
                 WHERE namespace = '{namespace}' AND native_version = '{migration_version}'"
            ),
        )
        .expect("inspect applied row at acknowledgement boundary");
        if run_count == 1 && expected_step_count == 1 && effect_count == 1 && applied_count == 0 {
            boundary_state_observed = true;
            break;
        }
        if child
            .try_wait()
            .expect("poll acknowledgement-boundary CLI")
            .is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let child_was_running = child
        .try_wait()
        .expect("inspect acknowledgement-boundary CLI before termination")
        .is_none();
    if child_was_running {
        child
            .kill()
            .expect("kill CLI at the selected acknowledgement boundary");
    }
    let child_status = child.wait().expect("reap acknowledgement-boundary CLI");
    let _ = proxy.control.send(MysqlProxyControl::Stop);
    let proxy_result = proxy
        .thread
        .join()
        .expect("MySQL/MariaDB acknowledgement proxy thread did not panic");

    let lock_name = mysql_lock_name(&database);
    let lock_deadline = Instant::now() + Duration::from_secs(10);
    let mut test_connection_owns_lock = false;
    while Instant::now() < lock_deadline {
        let result: Option<(Option<i64>,)> = runtime
            .block_on(admin_connection.query_first(format!("SELECT GET_LOCK('{lock_name}', 0)")))
            .expect("probe release of the terminated MySQL session lock");
        if result == Some((Some(1),)) {
            test_connection_owns_lock = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        test_connection_owns_lock,
        "terminated MySQL session lock remained held at {} boundary",
        boundary.label()
    );
    runtime
        .block_on(admin_connection.query_drop(format!("SELECT RELEASE_LOCK('{lock_name}')")))
        .expect("release the acknowledgement-boundary MySQL lock probe");

    let run_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_runs \
             WHERE namespace = '{namespace}' AND migration_version = '{migration_version}' \
               AND state = 'started'"
        ),
    )
    .expect("inspect unresolved acknowledgement-boundary run");
    let step_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace}' \
               AND run.migration_version = '{migration_version}' \
               AND step.state = '{}'",
            boundary.expected_step_state()
        ),
    )
    .expect("inspect journal state after acknowledgement-boundary death");
    let effect_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{effect_table}'"
        ),
    )
    .expect("inspect SQL effect after acknowledgement-boundary death");
    let applied_count = mysql_scalar(
        &base_url,
        &database,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{namespace}' AND native_version = '{migration_version}'"
        ),
    )
    .expect("count applied rows after acknowledgement-boundary death");
    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        environment_variable,
        &target_url,
    );
    let cleanup = database_cleanup.clean();

    let notification_detail = match notification {
        Ok(Ok(())) => "target metadata statement held before forwarding".to_owned(),
        Ok(Err(error)) => error,
        Err(error) => format!("no target statement notification: {error}"),
    };
    assert!(
        statement_held,
        "proxy did not hold the {} statement ({notification_detail}); proxy={proxy_result:?}",
        boundary.label()
    );
    assert!(
        boundary_state_observed,
        "expected durable {} state was not visible before termination",
        boundary.label()
    );
    assert!(
        child_was_running,
        "CLI exited before it could be terminated"
    );
    assert!(
        !child_status.success(),
        "killed CLI must not report success"
    );
    proxy_result.expect("MySQL/MariaDB acknowledgement proxy failed");
    assert_eq!(run_count, 1);
    assert_eq!(step_count, 1);
    assert_eq!(effect_count, 1);
    assert_eq!(applied_count, 0);
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
    cleanup.expect("drop isolated MySQL/MariaDB acknowledgement database");
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

fn postgres_query_one(connection_url: &str, query: &str) -> Result<tokio_postgres::Row, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let result = client
            .query_one(query, &[])
            .await
            .map_err(|_| "query disposable PostgreSQL".to_owned());
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        result
    })
}

fn postgres_kill_cli_when_query_is_active(
    child: &mut Child,
    connection_url: &str,
    query_fragment: &str,
) -> Result<i32, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "create Tokio runtime")?;
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(connection_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| "connect to disposable PostgreSQL".to_owned())?;
        let connection_task = tokio::spawn(connection);
        let pattern = format!("%{query_fragment}%");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let backend_pid = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|_| "inspect migration CLI process".to_owned())?
            {
                let mut stdout = String::new();
                if let Some(mut stream) = child.stdout.take() {
                    let _ = stream.read_to_string(&mut stdout);
                }
                let mut stderr = String::new();
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_string(&mut stderr);
                }
                return Err(format!(
                    "migration CLI exited before PostgreSQL reached {query_fragment}: {status}; stdout={stdout}; stderr={stderr}"
                ));
            }
            let active_pid = client
                .query_opt(
                    "SELECT pid FROM pg_stat_activity \
                     WHERE datname = current_database() AND state = 'active' \
                       AND wait_event = 'PgSleep' AND query LIKE $1 \
                     ORDER BY query_start DESC LIMIT 1",
                    &[&pattern],
                )
                .await
                .map_err(|_| "inspect PostgreSQL activity".to_owned())?
                .map(|row| row.get::<_, i32>(0));
            if let Some(pid) = active_pid {
                break pid;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "migration CLI did not reach PostgreSQL query {query_fragment}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        child
            .kill()
            .map_err(|_| "kill migration CLI at the observed database boundary".to_owned())?;
        let status = child
            .wait()
            .map_err(|_| "reap killed migration CLI".to_owned())?;
        if status.success() {
            return Err("killed migration CLI unexpectedly reported success".to_owned());
        }

        let backend_terminated: bool = client
            .query_one("SELECT pg_terminate_backend($1)", &[&backend_pid])
            .await
            .map_err(|_| "terminate disposable PostgreSQL fault-injection backend".to_owned())?
            .get(0);
        if !backend_terminated {
            let backend_exists: bool = client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    &[&backend_pid],
                )
                .await
                .map_err(|_| "verify PostgreSQL fault-injection backend".to_owned())?
                .get(0);
            if backend_exists {
                return Err("could not terminate the disposable PostgreSQL test backend".to_owned());
            }
        }

        let disconnect_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let backend_exists: bool = client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    &[&backend_pid],
                )
                .await
                .map_err(|_| "verify migration backend disconnect".to_owned())?
                .get(0);
            if !backend_exists {
                break;
            }
            if tokio::time::Instant::now() >= disconnect_deadline {
                return Err("killed CLI backend did not disconnect in time".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        Ok(backend_pid)
    })
}

fn start_postgres_statement_hold_proxy(
    database_url: &str,
    schema: &str,
    query_fragment: &str,
) -> (
    String,
    std::sync::mpsc::Receiver<Result<(), String>>,
    std::thread::JoinHandle<()>,
) {
    use std::net::{IpAddr, TcpListener};
    use std::str::FromStr;
    use tokio_postgres::config::{Config as PostgresConfig, Host};

    let target = PostgresConfig::from_str(database_url)
        .expect("disposable PostgreSQL connection URL is valid");
    assert!(target.get_password().is_none());
    let user = target.get_user().expect("test database URL selects a user");
    let database = target
        .get_dbname()
        .expect("test database URL selects a database");
    assert!(
        user.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    );
    assert!(
        database
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    );
    let target_host = target
        .get_hosts()
        .first()
        .expect("test database URL selects one loopback host");
    let Host::Tcp(target_host) = target_host else {
        panic!("statement proxy requires a TCP loopback test target");
    };
    let target_ip = IpAddr::from_str(target_host)
        .expect("statement proxy test target must use a literal loopback address");
    assert!(target_ip.is_loopback());
    let target_port = target.get_ports().first().copied().unwrap_or(5432);
    let target_address = std::net::SocketAddr::new(target_ip, target_port);

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .expect("bind isolated loopback PostgreSQL statement proxy");
    listener
        .set_nonblocking(true)
        .expect("make PostgreSQL statement proxy listener nonblocking");
    let proxy_port = listener
        .local_addr()
        .expect("read PostgreSQL statement proxy address")
        .port();
    let proxy_url = format!(
        "postgresql://{user}@127.0.0.1:{proxy_port}/{database}?sslmode=disable&options=-c%20search_path%3D{schema}%2Cpublic"
    );

    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(2);
    let proxy_error_sender = ready_sender.clone();
    let query_fragment = query_fragment.to_owned();
    let proxy_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create PostgreSQL statement-proxy runtime");
        let proxy_result = runtime.block_on(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::from_std(listener)
                .map_err(|_| "initialize loopback PostgreSQL statement proxy".to_owned())?;
            let fragment = query_fragment.as_bytes();
            loop {
                let (client, _) = listener
                    .accept()
                    .await
                    .map_err(|_| "accept migration CLI connection at proxy".to_owned())?;
                let server = tokio::net::TcpStream::connect(target_address)
                    .await
                    .map_err(|_| "connect statement proxy to disposable PostgreSQL".to_owned())?;
                let (mut client_reader, mut client_writer) = client.into_split();
                let (mut server_reader, mut server_writer) = server.into_split();
                let server_to_client = tokio::spawn(async move {
                    tokio::io::copy(&mut server_reader, &mut client_writer).await
                });
                let mut intercepted = false;

                loop {
                    let mut tag = [0_u8; 1];
                    if client_reader.read_exact(&mut tag).await.is_err() {
                        break;
                    }
                    if tag[0] == 0 {
                        let mut length_tail = [0_u8; 3];
                        client_reader
                            .read_exact(&mut length_tail)
                            .await
                            .map_err(|_| "read PostgreSQL startup packet length".to_owned())?;
                        let packet_length = u32::from_be_bytes([
                            tag[0],
                            length_tail[0],
                            length_tail[1],
                            length_tail[2],
                        ]) as usize;
                        if packet_length < 4 {
                            return Err("invalid PostgreSQL startup packet length".to_owned());
                        }
                        let mut body = vec![0_u8; packet_length - 4];
                        client_reader
                            .read_exact(&mut body)
                            .await
                            .map_err(|_| "read PostgreSQL startup packet".to_owned())?;
                        server_writer
                            .write_all(&tag)
                            .await
                            .map_err(|_| "forward PostgreSQL startup packet".to_owned())?;
                        server_writer
                            .write_all(&length_tail)
                            .await
                            .map_err(|_| "forward PostgreSQL startup length".to_owned())?;
                        server_writer
                            .write_all(&body)
                            .await
                            .map_err(|_| "forward PostgreSQL startup contents".to_owned())?;
                        continue;
                    }

                    let mut length_bytes = [0_u8; 4];
                    client_reader
                        .read_exact(&mut length_bytes)
                        .await
                        .map_err(|_| "read PostgreSQL frontend message length".to_owned())?;
                    let message_length = u32::from_be_bytes(length_bytes) as usize;
                    if message_length < 4 {
                        return Err("invalid PostgreSQL frontend message length".to_owned());
                    }
                    let mut body = vec![0_u8; message_length - 4];
                    client_reader
                        .read_exact(&mut body)
                        .await
                        .map_err(|_| "read PostgreSQL frontend message".to_owned())?;
                    if tag[0] == b'Q'
                        && body
                            .windows(fragment.len())
                            .any(|window| window == fragment)
                    {
                        ready_sender
                            .send(Ok(()))
                            .map_err(|_| "notify test that migration SQL is held".to_owned())?;
                        intercepted = true;
                        let mut discarded = [0_u8; 4096];
                        loop {
                            match client_reader.read(&mut discarded).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                        break;
                    }
                    server_writer
                        .write_all(&tag)
                        .await
                        .map_err(|_| "forward PostgreSQL frontend message tag".to_owned())?;
                    server_writer
                        .write_all(&length_bytes)
                        .await
                        .map_err(|_| "forward PostgreSQL frontend message length".to_owned())?;
                    server_writer
                        .write_all(&body)
                        .await
                        .map_err(|_| "forward PostgreSQL frontend message body".to_owned())?;
                }

                drop(server_writer);
                let _ = server_to_client.await;
                if intercepted {
                    return Ok(());
                }
            }
        });
        if let Err(error) = proxy_result {
            let _ = proxy_error_sender.send(Err(error));
        }
    });
    (proxy_url, ready_receiver, proxy_thread)
}

fn configure_postgres_cli_project(directory: &Path, namespace: &str) {
    configure_postgres_cli_project_with_busy_timeout(directory, namespace, 30);
}

fn configure_postgres_cli_project_with_busy_timeout(
    directory: &Path,
    namespace: &str,
    busy_timeout_seconds: u64,
) {
    fs::write(
        directory.join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"postgres\"\nurl_env = \"DBM_TEST_DATABASE_URL\"\nenvironment = \"test\"\nallow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = {busy_timeout_seconds}\n"
        ),
    )
    .expect("configure disposable PostgreSQL target");
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    let mut competing = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start competing migration process");
    thread::sleep(Duration::from_millis(100));
    assert!(
        competing
            .try_wait()
            .expect("check competing process while first writer holds the lock")
            .is_none(),
        "competing CLI must not finish while the first migration owns the SQLite writer lock"
    );

    thread::sleep(Duration::from_millis(100));
    child
        .kill()
        .expect("kill migration during the long SQL statement");
    let killed = child.wait().expect("reap killed migration process");
    assert!(!killed.success());
    let competing = competing
        .wait_with_output()
        .expect("reap competing migration process");
    assert_eq!(competing.status.code(), Some(9));
    assert_eq!(json(&competing)["errors"][0]["code"], "SAFETY_BLOCKED");

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
    let run_count: i64 = database
        .query_row("SELECT COUNT(*) FROM schema_migration_runs", [], |row| {
            row.get(0)
        })
        .expect("count runs after competing CLI was rejected");
    assert_eq!(applied_count, 0);
    assert_eq!(effect_table_count, 0);
    assert_eq!(run_count, 1);
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
fn sqlite_crash_after_rollback_before_failure_record_keeps_run_unresolved() {
    let directory = tempfile::tempdir().expect("temporary rollback-crash project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let migration_version = "20261107172000000001";
    let baseline_version = "20261107172000000000";
    let baseline_table = format!("rollback_crash_baseline_{}", uuid::Uuid::new_v4().simple());
    let effect_table = format!("rollback_crash_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n\n[execution]\nbusy_timeout_seconds = 30\n",
    )
    .expect("configure SQLite rollback-crash project");
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{baseline_version}_baseline.sql")),
        format!(
            "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE {baseline_table} (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE {baseline_table};\n"
        ),
    )
    .expect("write baseline migration to bootstrap metadata");

    let database_path = directory.path().join("dbm.sqlite");
    let baseline = run(directory.path(), &["--format", "json", "up"]);
    assert!(
        baseline.status.success(),
        "baseline migration failed: {}",
        String::from_utf8_lossy(&baseline.stdout)
    );
    let setup = rusqlite::Connection::open(&database_path)
        .expect("open initialized SQLite target for rollback trigger");
    setup
        .execute_batch(
            "CREATE TABLE failed_run_trigger_sink (value INTEGER NOT NULL);\
             CREATE TRIGGER hold_failed_run_persistence \
             AFTER UPDATE OF state ON schema_migration_runs \
             WHEN NEW.state = 'failed' BEGIN \
                 INSERT INTO failed_run_trigger_sink VALUES (0);\
                 INSERT INTO failed_run_trigger_sink \
                 SELECT sum(value) FROM (\
                     WITH RECURSIVE sequence(value) AS (\
                         SELECT 1 UNION ALL SELECT value + 1 FROM sequence \
                         WHERE value < 100000000\
                     ) SELECT value FROM sequence\
                 );\
             END;",
        )
        .expect("install slow trigger for failed-run persistence");
    drop(setup);
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{migration_version}_rollback_crash.sql")),
        format!(
            "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE {effect_table} (id INTEGER PRIMARY KEY);\nWITH RECURSIVE sequence(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM sequence WHERE value < 1000000) SELECT sum(value) FROM sequence;\nSELECT * FROM deliberately_missing_rollback_crash_table;\n-- dbm:down\nDROP TABLE {effect_table};\n"
        ),
    )
    .expect("write transactional migration that fails after earlier DDL");
    let journal_path = directory.path().join("dbm.sqlite-journal");
    assert!(
        !journal_path.exists(),
        "SQLite baseline must be committed before the crash-window fixture"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start SQLite migration process");

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut durable_started_run = false;
    let mut migration_journal_seen = false;
    let mut rollback_seen = false;
    let mut failed_run_persistence_observed = false;
    let mut failed_run_commit_blocker = None;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll SQLite migration process") {
            let output = child
                .wait_with_output()
                .expect("collect SQLite migration output after early exit");
            panic!(
                "SQLite migration exited before rollback persistence window: {status}; started={durable_started_run}, migration journal={migration_journal_seen}, rollback={rollback_seen}; stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        if !durable_started_run {
            let observer =
                rusqlite::Connection::open(&database_path).expect("open SQLite run-state observer");
            durable_started_run = observer
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM schema_migration_runs \
                     WHERE migration_version = ?1 AND state = 'started')",
                    [migration_version],
                    |row| row.get(0),
                )
                .expect("observe this migration's durable started run");
            drop(observer);
        }

        if durable_started_run {
            if journal_path.exists() {
                migration_journal_seen = true;
            } else if migration_journal_seen {
                rollback_seen = true;
                if failed_run_commit_blocker.is_none() {
                    let blocker = rusqlite::Connection::open(&database_path)
                        .expect("open SQLite rollback observer");
                    blocker
                        .execute_batch("BEGIN DEFERRED")
                        .expect("begin SQLite rollback observation transaction");
                    let _: i64 = blocker
                        .query_row("SELECT COUNT(*) FROM schema_migration_runs", [], |row| {
                            row.get(0)
                        })
                        .expect("hold a SQLite read lock after migration rollback");
                    failed_run_commit_blocker = Some(blocker);
                }
            }
            if rollback_seen && journal_path.exists() {
                failed_run_persistence_observed = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }

    if !failed_run_persistence_observed {
        let _ = child.kill();
        let output = child
            .wait_with_output()
            .expect("collect SQLite process after rollback-window timeout");
        panic!(
            "did not observe SQLite rollback followed by failed-run persistence; started={durable_started_run}, migration journal={migration_journal_seen}, rollback={rollback_seen}; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    child
        .kill()
        .expect("kill SQLite process before failed-run transaction commits");
    let killed = child
        .wait()
        .expect("reap SQLite process killed after rollback");
    assert!(!killed.success());
    drop(failed_run_commit_blocker);

    let database = rusqlite::Connection::open(&database_path)
        .expect("inspect SQLite state after rollback-window crash");
    let applied_count: i64 = database
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count applied migrations after crash");
    let effect_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [&effect_table],
            |row| row.get(0),
        )
        .expect("inspect transactional DDL after crash");
    let run_count: i64 = database
        .query_row("SELECT COUNT(*) FROM schema_migration_runs", [], |row| {
            row.get(0)
        })
        .expect("count persisted runs after crash");
    let (run_id, run_state): (String, String) = database
        .query_row(
            "SELECT run_id, state FROM schema_migration_runs \
             WHERE migration_version = ?1",
            [migration_version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read failed migration run after rollback-window crash");
    let step_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_steps WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .expect("count steps for the rolled-back migration");
    let (lock_state, lock_run_id): (String, Option<String>) = database
        .query_row(
            "SELECT state, run_id FROM schema_migration_locks WHERE scope = 'database'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read diagnostic lock after rollback-window crash");
    let baseline_applied: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = 'default' AND kind = 'versioned' AND native_version = ?1",
            [baseline_version],
            |row| row.get(0),
        )
        .expect("verify the already-committed baseline remains applied");

    assert_eq!(applied_count, 1);
    assert_eq!(baseline_applied, 1);
    assert_eq!(effect_count, 0);
    assert_eq!(run_count, 2);
    assert_eq!(run_state, "started");
    assert_eq!(step_count, 0);
    assert_eq!(lock_state, "observed_active");
    assert_eq!(lock_run_id.as_deref(), Some(run_id.as_str()));
    drop(database);

    let retry = run(directory.path(), &["--format", "json", "up"]);
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
}

#[test]
fn cli_serializes_sqlite_writers_across_namespaces_sharing_schema_objects() {
    let directory = tempfile::tempdir().expect("temporary shared-schema SQLite project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());

    let first_namespace = format!("sqlite_ns_a_{}", uuid::Uuid::new_v4().simple());
    let second_namespace = format!("sqlite_ns_b_{}", uuid::Uuid::new_v4().simple());
    let migration_version = "20261107171000000000";
    let shared_table = format!("shared_sqlite_object_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        directory.path().join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"{first_namespace}\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n\n[execution]\nbusy_timeout_seconds = 30\n"
        ),
    )
    .expect("configure shared-schema SQLite project");
    fs::write(
        directory
            .path()
            .join(format!("migrations/versioned/{migration_version}_shared.sql")),
        format!(
            "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE {shared_table} (id INTEGER PRIMARY KEY);\nWITH RECURSIVE sequence(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM sequence WHERE value < 5000000) SELECT sum(value) FROM sequence;\n-- dbm:down\nDROP TABLE {shared_table};\n"
        ),
    )
    .expect("write shared-schema SQLite migration");

    let database_path = directory.path().join("dbm.sqlite");
    let mut first = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--namespace", &first_namespace, "--format", "json", "up"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start first SQLite namespace CLI");

    let writer_deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_writer = false;
    while Instant::now() < writer_deadline {
        if let Some(status) = first.try_wait().expect("poll first SQLite CLI") {
            let output = first
                .wait_with_output()
                .expect("collect first SQLite CLI output after early exit");
            panic!(
                "first SQLite CLI exited before lock observation: {status}; stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if database_path.exists() {
            let observer = rusqlite::Connection::open(&database_path)
                .expect("open SQLite namespace-race observer");
            let run_table_exists: bool = observer
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'table' AND name = 'schema_migration_runs')",
                    [],
                    |row| row.get(0),
                )
                .expect("check for bootstrapped SQLite run table");
            let started_run = if run_table_exists {
                observer
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM schema_migration_runs \
                         WHERE namespace = ?1 AND state = 'started')",
                        [&first_namespace],
                        |row| row.get::<_, bool>(0),
                    )
                    .expect("inspect first namespace started run")
            } else {
                false
            };

            if started_run {
                drop(observer);
                let mut lock_probe = rusqlite::Connection::open(&database_path)
                    .expect("open SQLite namespace-race lock probe");
                lock_probe
                    .busy_timeout(Duration::ZERO)
                    .expect("make SQLite namespace-race lock probe non-blocking");
                match lock_probe.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                {
                    Ok(transaction) => {
                        transaction
                            .rollback()
                            .expect("release unused SQLite namespace-race lock probe");
                    }
                    Err(rusqlite::Error::SqliteFailure(failure, _))
                        if matches!(
                            failure.code,
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        ) =>
                    {
                        observed_writer = true;
                        break;
                    }
                    Err(error) => panic!("unexpected SQLite lock-probe failure: {error}"),
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }

    if !observed_writer {
        let _ = first.kill();
        let output = first
            .wait_with_output()
            .expect("collect first SQLite CLI after lock-observation failure");
        panic!(
            "did not observe the first SQLite writer; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut second = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--namespace", &second_namespace, "--format", "json", "up"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start competing SQLite namespace CLI");

    let contention_deadline = Instant::now() + Duration::from_millis(400);
    let mut first_finished_during_contention = false;
    let mut second_finished_during_contention = false;
    while Instant::now() < contention_deadline {
        if first
            .try_wait()
            .expect("poll first SQLite CLI during contention")
            .is_some()
        {
            first_finished_during_contention = true;
            break;
        }
        if second
            .try_wait()
            .expect("poll second SQLite CLI during contention")
            .is_some()
        {
            second_finished_during_contention = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let second_runs_before_first_finishes: i64 = rusqlite::Connection::open(&database_path)
        .expect("open journal observer during namespace contention")
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_runs WHERE namespace = ?1",
            [&second_namespace],
            |row| row.get(0),
        )
        .expect("count second namespace runs while first writer is active");

    let first_deadline = Instant::now() + Duration::from_secs(30);
    let mut first_completed = false;
    while Instant::now() < first_deadline {
        if first
            .try_wait()
            .expect("poll first SQLite CLI completion")
            .is_some()
        {
            first_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !first_completed {
        let _ = first.kill();
        let _ = second.kill();
        let _ = first.wait();
        let _ = second.wait();
        panic!("first SQLite namespace migration exceeded its execution deadline");
    }
    let first_output = first
        .wait_with_output()
        .expect("collect first SQLite namespace CLI output");

    let second_deadline = Instant::now() + Duration::from_secs(30);
    let mut second_completed = false;
    while Instant::now() < second_deadline {
        if second
            .try_wait()
            .expect("poll second SQLite CLI completion")
            .is_some()
        {
            second_completed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !second_completed {
        let _ = second.kill();
    }
    let second_output = second
        .wait_with_output()
        .expect("collect second SQLite namespace CLI output");

    let database = rusqlite::Connection::open(&database_path).expect("inspect SQLite journal");
    let first_run_state: String = database
        .query_row(
            "SELECT state FROM schema_migration_runs WHERE namespace = ?1",
            [&first_namespace],
            |row| row.get(0),
        )
        .expect("read first namespace run state");
    let second_run_state: String = database
        .query_row(
            "SELECT state FROM schema_migration_runs WHERE namespace = ?1",
            [&second_namespace],
            |row| row.get(0),
        )
        .expect("read second namespace run state");
    let first_applied_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = ?1 AND native_version = ?2",
            [&first_namespace, migration_version],
            |row| row.get(0),
        )
        .expect("count first namespace applied rows");
    let second_applied_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = ?1 AND native_version = ?2",
            [&second_namespace, migration_version],
            |row| row.get(0),
        )
        .expect("count second namespace applied rows");
    let shared_table_count: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [&shared_table],
            |row| row.get(0),
        )
        .expect("inspect shared SQLite schema object");

    assert!(
        !first_finished_during_contention,
        "first SQLite writer finished before contention was observed"
    );
    assert!(
        !second_finished_during_contention,
        "second SQLite namespace completed while first writer held the lock"
    );
    assert_eq!(second_runs_before_first_finishes, 0);
    assert!(
        first_completed,
        "first SQLite namespace CLI did not complete"
    );
    assert!(
        first_output.status.success(),
        "first SQLite namespace CLI failed: {}",
        String::from_utf8_lossy(&first_output.stdout)
    );
    assert!(
        second_completed,
        "second SQLite namespace CLI did not complete"
    );
    assert_eq!(second_output.status.code(), Some(10));
    assert_eq!(json(&second_output)["errors"][0]["code"], "DATABASE_ERROR");
    assert_eq!(first_run_state, "succeeded");
    assert_eq!(second_run_state, "failed");
    assert_eq!(first_applied_count, 1);
    assert_eq!(second_applied_count, 0);
    assert_eq!(shared_table_count, 1);
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
        5
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
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn cli_process_death_at_postgres_recovery_boundaries_keeps_runs_unresolved() {
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
    assert!(!database_url.contains("options="));

    let schema = format!("dbm_cli_recovery_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated PostgreSQL recovery schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read isolated PostgreSQL schema"),
        schema
    );

    let directory = tempfile::tempdir().expect("temporary PostgreSQL test project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    configure_postgres_cli_project(directory.path(), "bootstrap");
    let migrations = directory.path().join("migrations/versioned");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        bootstrapped.status.success(),
        "initialize isolated PostgreSQL metadata: {}",
        String::from_utf8_lossy(&bootstrapped.stdout)
    );
    fs::remove_file(&bootstrap_migration).expect("remove bootstrap-only migration source");

    let target_table = format!("recovery_target_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!("CREATE TABLE {schema}.{target_table} (id bigint PRIMARY KEY)"),
    )
    .expect("create isolated target table for concurrent indexes");

    let namespace_before_intent = format!("before_intent_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project(directory.path(), &namespace_before_intent);
    let before_intent_effect = format!("before_intent_effect_{}", uuid::Uuid::new_v4().simple());
    let before_intent_migration = migrations.join("20261107120000000001_before_intent.sql");
    fs::write(
        &before_intent_migration,
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE {schema}.{before_intent_effect} (id integer);\n-- dbm:down\nDROP TABLE {schema}.{before_intent_effect};\n"
        ),
    )
    .expect("write pre-intent crash migration");
    let pause_before_intent_function =
        format!("pause_before_intent_{}", uuid::Uuid::new_v4().simple());
    let pause_before_intent_trigger =
        format!("pause_before_intent_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!(
            "CREATE FUNCTION {schema}.{pause_before_intent_function}() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.state = 'intent_recorded' THEN PERFORM pg_sleep(30); END IF; \
             RETURN NEW; END $$; \
             CREATE TRIGGER {pause_before_intent_trigger} BEFORE INSERT \
             ON schema_migration_steps FOR EACH ROW \
             EXECUTE FUNCTION {schema}.{pause_before_intent_function}()"
        ),
    )
    .expect("install isolated pre-intent pause trigger");
    let mut before_intent_child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &schema_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI for pre-intent process-death test");
    postgres_kill_cli_when_query_is_active(
        &mut before_intent_child,
        &schema_url,
        "INSERT INTO schema_migration_steps",
    )
    .expect("kill CLI before the intent insert can complete");
    let before_intent_run: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT state FROM schema_migration_runs WHERE namespace = '{namespace_before_intent}'"
        ),
    )
    .expect("read pre-intent run")
    .try_get(0)
    .expect("decode pre-intent run state");
    let before_intent_step_count: i64 = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace_before_intent}'"
        ),
    )
    .expect("count durable pre-intent steps")
    .try_get(0)
    .expect("decode pre-intent step count");
    assert_eq!(before_intent_run, "started");
    assert_eq!(before_intent_step_count, 0);
    assert!(
        !postgres_query_one(
            &schema_url,
            &format!(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name = '{before_intent_effect}')"
            ),
        )
        .expect("inspect pre-intent migration effect")
        .try_get::<_, bool>(0)
        .expect("decode pre-intent effect state")
    );
    let blocked_before_intent = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert_eq!(blocked_before_intent.status.code(), Some(9));
    assert_eq!(
        json(&blocked_before_intent)["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );
    postgres_batch_execute(
        &schema_url,
        &format!(
            "DROP TRIGGER {pause_before_intent_trigger} ON schema_migration_steps; \
             DROP FUNCTION {schema}.{pause_before_intent_function}()"
        ),
    )
    .expect("remove pre-intent pause trigger");
    fs::remove_file(&before_intent_migration).expect("remove pre-intent migration source");
    schema_cleanup
        .clean()
        .expect("drop isolated pre-intent PostgreSQL schema");

    let schema = format!("dbm_cli_pre_ack_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated pre-ack PostgreSQL schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read pre-ack PostgreSQL schema"),
        schema
    );
    configure_postgres_cli_project(directory.path(), "bootstrap");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write pre-ack metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(bootstrapped.status.success());
    fs::remove_file(&bootstrap_migration).expect("remove pre-ack bootstrap migration");
    let target_table = format!("recovery_target_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!("CREATE TABLE {schema}.{target_table} (id bigint PRIMARY KEY)"),
    )
    .expect("create pre-ack isolated target table");

    let namespace_before_ack = format!("before_ack_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project(directory.path(), &namespace_before_ack);
    let index_before_ack = format!("before_ack_idx_{}", uuid::Uuid::new_v4().simple());
    let before_ack_migration = migrations.join("20261107120000000002_before_ack.sql");
    fs::write(
        &before_ack_migration,
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY {index_before_ack} ON {target_table} (id);\n-- dbm:down\nDROP INDEX CONCURRENTLY {index_before_ack};\n"
        ),
    )
    .expect("write SQL-complete-before-ack migration");
    let pause_ack_function = format!("pause_ack_{}", uuid::Uuid::new_v4().simple());
    let pause_ack_trigger = format!("pause_ack_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!(
            "CREATE FUNCTION {schema}.{pause_ack_function}() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.state = 'acknowledged' AND EXISTS ( \
                 SELECT 1 FROM schema_migration_runs \
                 WHERE run_id = NEW.run_id AND namespace = '{namespace_before_ack}' \
             ) THEN PERFORM pg_sleep(30); END IF; \
             RETURN NEW; END $$; \
             CREATE TRIGGER {pause_ack_trigger} BEFORE UPDATE ON schema_migration_steps \
             FOR EACH ROW EXECUTE FUNCTION {schema}.{pause_ack_function}()"
        ),
    )
    .expect("install isolated acknowledgement pause trigger");
    let mut before_ack_child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &schema_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI for post-SQL/pre-ack process-death test");
    postgres_kill_cli_when_query_is_active(
        &mut before_ack_child,
        &schema_url,
        "UPDATE schema_migration_steps",
    )
    .expect("kill CLI after index creation and before acknowledgement commit");
    let before_ack_run: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT state FROM schema_migration_runs WHERE namespace = '{namespace_before_ack}'"
        ),
    )
    .expect("read post-SQL/pre-ack run")
    .try_get(0)
    .expect("decode post-SQL/pre-ack run state");
    let before_ack_step: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT step.state FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace_before_ack}'"
        ),
    )
    .expect("read post-SQL/pre-ack step")
    .try_get(0)
    .expect("decode post-SQL/pre-ack step state");
    let index_before_ack_valid: bool = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_index AS index \
             INNER JOIN pg_class AS relation ON relation.oid = index.indexrelid \
             INNER JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace \
             WHERE namespace.nspname = '{schema}' \
               AND relation.relname = '{index_before_ack}' AND index.indisvalid)"
        ),
    )
    .expect("inspect index created before acknowledgement")
    .try_get(0)
    .expect("decode pre-ack index validity");
    assert_eq!(before_ack_run, "started");
    assert_eq!(before_ack_step, "intent_recorded");
    assert!(index_before_ack_valid);
    let blocked_before_ack = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert_eq!(blocked_before_ack.status.code(), Some(9));
    assert_eq!(
        json(&blocked_before_ack)["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );
    postgres_batch_execute(
        &schema_url,
        &format!(
            "DROP TRIGGER {pause_ack_trigger} ON schema_migration_steps; \
             DROP FUNCTION {schema}.{pause_ack_function}()"
        ),
    )
    .expect("remove acknowledgement pause trigger");
    fs::remove_file(&before_ack_migration).expect("remove pre-ack migration source");
    schema_cleanup
        .clean()
        .expect("drop isolated pre-ack PostgreSQL schema");

    let schema = format!("dbm_cli_pre_final_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated pre-finalization PostgreSQL schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read pre-finalization PostgreSQL schema"),
        schema
    );
    configure_postgres_cli_project(directory.path(), "bootstrap");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write pre-finalization metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(bootstrapped.status.success());
    fs::remove_file(&bootstrap_migration).expect("remove pre-finalization bootstrap migration");
    let target_table = format!("recovery_target_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!("CREATE TABLE {schema}.{target_table} (id bigint PRIMARY KEY)"),
    )
    .expect("create pre-finalization isolated target table");

    let namespace_before_finalization =
        format!("before_finalization_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project(directory.path(), &namespace_before_finalization);
    let index_before_finalization = format!("before_final_idx_{}", uuid::Uuid::new_v4().simple());
    let before_finalization_migration =
        migrations.join("20261107120000000003_before_finalization.sql");
    fs::write(
        &before_finalization_migration,
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY {index_before_finalization} ON {target_table} (id);\n-- dbm:down\nDROP INDEX CONCURRENTLY {index_before_finalization};\n"
        ),
    )
    .expect("write acknowledged-before-finalization migration");
    let pause_finalization_function =
        format!("pause_finalization_{}", uuid::Uuid::new_v4().simple());
    let pause_finalization_trigger =
        format!("pause_finalization_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!(
            "CREATE FUNCTION {schema}.{pause_finalization_function}() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.namespace = '{namespace_before_finalization}' THEN \
                 PERFORM pg_sleep(30); END IF; RETURN NEW; END $$; \
             CREATE TRIGGER {pause_finalization_trigger} BEFORE INSERT ON schema_migrations \
             FOR EACH ROW EXECUTE FUNCTION {schema}.{pause_finalization_function}()"
        ),
    )
    .expect("install isolated finalization pause trigger");
    let mut before_finalization_child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &schema_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start CLI for post-ack/pre-finalization process-death test");
    postgres_kill_cli_when_query_is_active(
        &mut before_finalization_child,
        &schema_url,
        "INSERT INTO schema_migrations",
    )
    .expect("kill CLI after acknowledgement commit and before finalization");
    let before_finalization_run: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT state FROM schema_migration_runs \
             WHERE namespace = '{namespace_before_finalization}'"
        ),
    )
    .expect("read post-ack/pre-finalization run")
    .try_get(0)
    .expect("decode post-ack/pre-finalization run state");
    let before_finalization_step: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT step.state FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace_before_finalization}'"
        ),
    )
    .expect("read post-ack/pre-finalization step")
    .try_get(0)
    .expect("decode post-ack/pre-finalization step state");
    let before_finalization_applied_count: i64 = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations \
             WHERE namespace = '{namespace_before_finalization}'"
        ),
    )
    .expect("count applied records before finalization")
    .try_get(0)
    .expect("decode pre-finalization applied count");
    let index_before_finalization_valid: bool = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_index AS index \
             INNER JOIN pg_class AS relation ON relation.oid = index.indexrelid \
             INNER JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace \
             WHERE namespace.nspname = '{schema}' \
               AND relation.relname = '{index_before_finalization}' AND index.indisvalid)"
        ),
    )
    .expect("inspect index after acknowledgement")
    .try_get(0)
    .expect("decode post-ack index validity");
    assert_eq!(before_finalization_run, "started");
    assert_eq!(before_finalization_step, "acknowledged");
    assert_eq!(before_finalization_applied_count, 0);
    assert!(index_before_finalization_valid);
    let blocked_before_finalization = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert_eq!(blocked_before_finalization.status.code(), Some(9));
    assert_eq!(
        json(&blocked_before_finalization)["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );
    postgres_batch_execute(
        &schema_url,
        &format!(
            "DROP TRIGGER {pause_finalization_trigger} ON schema_migrations; \
             DROP FUNCTION {schema}.{pause_finalization_function}()"
        ),
    )
    .expect("remove finalization pause trigger");
    fs::remove_file(&before_finalization_migration)
        .expect("remove pre-finalization migration source");
    schema_cleanup
        .clean()
        .expect("drop isolated pre-finalization PostgreSQL schema");

    let schema = format!("dbm_cli_rollback_record_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated rollback-record PostgreSQL schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read rollback-record PostgreSQL schema"),
        schema
    );
    configure_postgres_cli_project(directory.path(), "bootstrap");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write rollback-record metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(bootstrapped.status.success());
    fs::remove_file(&bootstrap_migration).expect("remove rollback-record bootstrap source");

    let namespace_after_rollback = format!("after_rollback_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project(directory.path(), &namespace_after_rollback);
    let rolled_back_effect = format!("rolled_back_effect_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        migrations.join("20261107120000000004_rollback_before_record.sql"),
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE {schema}.{rolled_back_effect} (id integer);\nSELECT 1 / 0;\n-- dbm:down\nDROP TABLE {schema}.{rolled_back_effect};\n"
        ),
    )
    .expect("write failure-after-rollback migration");
    let pause_failed_run_function = format!("pause_failed_run_{}", uuid::Uuid::new_v4().simple());
    let pause_failed_run_trigger = format!("pause_failed_run_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!(
            "CREATE FUNCTION {schema}.{pause_failed_run_function}() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.state = 'failed' AND NEW.namespace = '{namespace_after_rollback}' THEN \
                 PERFORM pg_sleep(30); END IF; RETURN NEW; END $$; \
             CREATE TRIGGER {pause_failed_run_trigger} BEFORE UPDATE ON schema_migration_runs \
             FOR EACH ROW EXECUTE FUNCTION {schema}.{pause_failed_run_function}()"
        ),
    )
    .expect("install isolated failed-run persistence pause trigger");
    let mut after_rollback_child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &schema_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI for rollback-before-record process-death test");
    postgres_kill_cli_when_query_is_active(
        &mut after_rollback_child,
        &schema_url,
        "UPDATE schema_migration_runs",
    )
    .expect("kill CLI while persisting the post-rollback failed run");

    let after_rollback_run: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT state FROM schema_migration_runs \
             WHERE namespace = '{namespace_after_rollback}'"
        ),
    )
    .expect("read run after rollback and process death")
    .try_get(0)
    .expect("decode post-rollback run state");
    let after_rollback_step_count: i64 = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT COUNT(*) FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace_after_rollback}'"
        ),
    )
    .expect("count steps after rollback and process death")
    .try_get(0)
    .expect("decode post-rollback step count");
    let after_rollback_effect_exists: bool = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name = '{rolled_back_effect}')"
        ),
    )
    .expect("inspect effect after transactional rollback")
    .try_get(0)
    .expect("decode post-rollback effect state");
    let after_rollback_applied_count: i64 = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT COUNT(*) FROM schema_migrations WHERE namespace = '{namespace_after_rollback}'"
        ),
    )
    .expect("count applied migrations after rollback and process death")
    .try_get(0)
    .expect("decode post-rollback applied count");
    assert_eq!(after_rollback_run, "started");
    assert_eq!(after_rollback_step_count, 0);
    assert!(!after_rollback_effect_exists);
    assert_eq!(after_rollback_applied_count, 0);

    let blocked_after_rollback = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert_eq!(blocked_after_rollback.status.code(), Some(9));
    assert_eq!(
        json(&blocked_after_rollback)["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );
    postgres_batch_execute(
        &schema_url,
        &format!(
            "DROP TRIGGER {pause_failed_run_trigger} ON schema_migration_runs; \
             DROP FUNCTION {schema}.{pause_failed_run_function}()"
        ),
    )
    .expect("remove post-rollback persistence trigger");
    schema_cleanup
        .clean()
        .expect("drop isolated rollback-record PostgreSQL schema");
}

#[test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn cli_process_death_after_postgres_intent_before_sql_keeps_the_intent() {
    use std::net::IpAddr;
    use std::str::FromStr;

    use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

    let database_url = std::env::var("DBM_TEST_DATABASE_URL")
        .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
    let parsed = PostgresConfig::from_str(&database_url)
        .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
    assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
    assert!(parsed.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
        Host::Tcp(host) => IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        Host::Unix(_) => true,
    }));
    assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));
    assert!(!database_url.contains("options="));

    let schema = format!("dbm_cli_post_intent_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated post-intent PostgreSQL schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read isolated post-intent schema"),
        schema
    );

    let directory = tempfile::tempdir().expect("temporary post-intent test project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    configure_postgres_cli_project(directory.path(), "bootstrap");
    let migrations = directory.path().join("migrations/versioned");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write post-intent metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        bootstrapped.status.success(),
        "initialize post-intent test metadata: {}",
        String::from_utf8_lossy(&bootstrapped.stdout)
    );
    fs::remove_file(&bootstrap_migration).expect("remove post-intent bootstrap source");

    let target_table = format!("post_intent_target_{}", uuid::Uuid::new_v4().simple());
    postgres_batch_execute(
        &schema_url,
        &format!("CREATE TABLE {schema}.{target_table} (id bigint PRIMARY KEY)"),
    )
    .expect("create isolated post-intent target table");

    let namespace = format!("post_intent_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project(directory.path(), &namespace);
    let index_name = format!("post_intent_idx_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        migrations.join("20261107120000000001_post_intent.sql"),
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nCREATE INDEX CONCURRENTLY {index_name} ON {target_table} (id);\n-- dbm:down\nDROP INDEX CONCURRENTLY {index_name};\n"
        ),
    )
    .expect("write post-intent non-transactional migration");

    let (proxy_url, proxy_notification, proxy_thread) =
        start_postgres_statement_hold_proxy(&database_url, &schema, &index_name);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &proxy_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI behind the PostgreSQL statement proxy");
    match proxy_notification
        .recv_timeout(Duration::from_secs(15))
        .expect("wait for migration SQL to reach the hold proxy")
    {
        Ok(()) => {}
        Err(error) => panic!("PostgreSQL statement proxy failed: {error}"),
    }
    child
        .kill()
        .expect("kill CLI while target SQL is held before forwarding");
    let killed = child.wait().expect("reap post-intent killed CLI");
    assert!(!killed.success());
    proxy_thread
        .join()
        .expect("join PostgreSQL statement proxy");

    let run_state: String = postgres_query_one(
        &schema_url,
        &format!("SELECT state FROM schema_migration_runs WHERE namespace = '{namespace}'"),
    )
    .expect("read run after post-intent process death")
    .try_get(0)
    .expect("decode post-intent run state");
    let step_state: String = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT step.state FROM schema_migration_steps AS step \
             INNER JOIN schema_migration_runs AS run USING (run_id) \
             WHERE run.namespace = '{namespace}'"
        ),
    )
    .expect("read durable post-intent step")
    .try_get(0)
    .expect("decode post-intent step state");
    let index_exists: bool = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_class AS relation \
             INNER JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace \
             WHERE namespace.nspname = '{schema}' AND relation.relname = '{index_name}')"
        ),
    )
    .expect("inspect index after SQL was withheld")
    .try_get(0)
    .expect("decode withheld-index state");
    let applied_count: i64 = postgres_query_one(
        &schema_url,
        &format!("SELECT COUNT(*) FROM schema_migrations WHERE namespace = '{namespace}'"),
    )
    .expect("count applied records after post-intent process death")
    .try_get(0)
    .expect("decode post-intent applied count");
    assert_eq!(run_state, "started");
    assert_eq!(step_state, "intent_recorded");
    assert!(!index_exists);
    assert_eq!(applied_count, 0);

    let retry = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert_eq!(retry.status.code(), Some(9));
    assert_eq!(json(&retry)["errors"][0]["code"], "SAFETY_BLOCKED");
    schema_cleanup
        .clean()
        .expect("drop isolated post-intent PostgreSQL schema");
}

#[test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn cli_postgres_competing_writers_same_namespace_are_serialized() {
    use std::net::IpAddr;
    use std::str::FromStr;

    use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

    let database_url = std::env::var("DBM_TEST_DATABASE_URL")
        .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
    let parsed = PostgresConfig::from_str(&database_url)
        .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
    assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
    assert!(parsed.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
        Host::Tcp(host) => IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        Host::Unix(_) => true,
    }));
    assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));
    assert!(!database_url.contains("options="));

    let schema = format!("dbm_cli_contention_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated PostgreSQL CLI-contention schema");
    assert_eq!(
        postgres_current_schema(&schema_url).expect("read isolated CLI-contention schema"),
        schema
    );

    let directory = tempfile::tempdir().expect("temporary PostgreSQL CLI-contention project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    configure_postgres_cli_project_with_busy_timeout(directory.path(), "bootstrap", 2);
    let migrations = directory.path().join("migrations/versioned");
    let bootstrap_migration = migrations.join("20261107120000000000_bootstrap.sql");
    fs::write(
        &bootstrap_migration,
        "-- dbm:dialect postgres\n-- dbm:up\nSELECT 1;\n-- dbm:down\nSELECT 1;\n",
    )
    .expect("write CLI-contention metadata-bootstrap migration");
    let bootstrapped = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(
        bootstrapped.status.success(),
        "initialize CLI-contention metadata: {}",
        String::from_utf8_lossy(&bootstrapped.stdout)
    );
    fs::remove_file(&bootstrap_migration).expect("remove CLI-contention bootstrap source");

    let namespace = format!("competing_{}", uuid::Uuid::new_v4().simple());
    configure_postgres_cli_project_with_busy_timeout(directory.path(), &namespace, 2);
    let table = format!("competing_table_{}", uuid::Uuid::new_v4().simple());
    let migration = migrations.join("20261107120000000001_competing_writer.sql");
    fs::write(
        &migration,
        format!(
            "-- dbm:dialect postgres\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE {schema}.{table} (id integer);\n-- dbm:down\nDROP TABLE {schema}.{table};\n"
        ),
    )
    .expect("write competing-writer migration");

    let (proxy_url, proxy_notification, proxy_thread) =
        start_postgres_statement_hold_proxy(&database_url, &schema, &table);
    let mut first = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory.path())
        .args(["--format", "json", "up"])
        .env("DBM_TEST_DATABASE_URL", &proxy_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start first PostgreSQL migration CLI");
    let first_reached_statement = match proxy_notification.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => true,
        Ok(Err(_)) | Err(_) => false,
    };

    let mut competing_status = None;
    let mut competing_output = None;
    if first_reached_statement {
        let mut competing = Command::new(env!("CARGO_BIN_EXE_dbm"))
            .current_dir(directory.path())
            .args(["--format", "json", "up"])
            .env("DBM_TEST_DATABASE_URL", &schema_url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start competing PostgreSQL migration CLI");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = competing
                .try_wait()
                .expect("inspect competing PostgreSQL CLI")
            {
                competing_status = Some(status);
                competing_output = Some(
                    competing
                        .wait_with_output()
                        .expect("collect competing PostgreSQL CLI output"),
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = competing.kill();
                let _ = competing.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let _ = first.kill();
    let _ = first.wait();
    let proxy_joined = proxy_thread.join().is_ok();

    let run_count: i64 = postgres_query_one(
        &schema_url,
        &format!("SELECT COUNT(*) FROM schema_migration_runs WHERE namespace = '{namespace}'"),
    )
    .expect("count same-namespace PostgreSQL CLI runs")
    .try_get(0)
    .expect("decode same-namespace run count");
    let migration_table_exists: bool = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name = '{table}')"
        ),
    )
    .expect("inspect table while first CLI SQL is held")
    .try_get(0)
    .expect("decode competing migration effect");
    schema_cleanup
        .clean()
        .expect("drop isolated PostgreSQL CLI-contention schema");

    assert!(
        first_reached_statement,
        "first CLI never reached its held migration SQL"
    );
    assert!(
        proxy_joined,
        "PostgreSQL statement proxy did not shut down cleanly"
    );
    assert_eq!(run_count, 1, "competing CLI must not create a second run");
    assert!(
        !migration_table_exists,
        "held migration SQL must not have reached PostgreSQL"
    );
    let competing_status = competing_status.expect("competing CLI must exit before its deadline");
    let competing_output = competing_output.expect("collect competing CLI result");
    assert_eq!(competing_status.code(), Some(5));
    assert_eq!(
        json(&competing_output)["errors"][0]["code"],
        "LOCK_UNAVAILABLE"
    );
}

#[test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn cli_postgres_metadata_bootstrap_failure_rolls_back_all_reserved_tables() {
    use std::net::IpAddr;
    use std::str::FromStr;

    use tokio_postgres::config::{Config as PostgresConfig, Host, SslMode};

    let database_url = std::env::var("DBM_TEST_DATABASE_URL")
        .expect("DBM_TEST_DATABASE_URL must identify a disposable loopback target");
    let parsed = PostgresConfig::from_str(&database_url)
        .expect("DBM_TEST_DATABASE_URL must be a valid PostgreSQL connection string");
    assert_eq!(parsed.get_ssl_mode(), SslMode::Disable);
    assert!(parsed.get_hosts().iter().all(|host| match host {
        Host::Tcp(host) if host.eq_ignore_ascii_case("localhost") => true,
        Host::Tcp(host) => IpAddr::from_str(host).is_ok_and(|address| address.is_loopback()),
        Host::Unix(_) => true,
    }));
    assert!(parsed.get_hostaddrs().iter().all(IpAddr::is_loopback));
    assert!(!database_url.contains("options="));

    let schema = format!("dbm_bootstrap_failure_{}", uuid::Uuid::new_v4().simple());
    let schema_url = format!("{database_url}&options=-c%20search_path%3D{schema}%2Cpublic");
    let mut schema_cleanup = PostgresSchemaCleanup {
        connection_url: database_url.clone(),
        schema: schema.clone(),
        cleaned: false,
    };
    postgres_batch_execute(&database_url, &format!("CREATE SCHEMA {schema}"))
        .expect("create isolated metadata-bootstrap schema");
    let function = format!(
        "reject_metadata_bootstrap_{}",
        uuid::Uuid::new_v4().simple()
    );
    let event_trigger = format!(
        "reject_metadata_bootstrap_{}",
        uuid::Uuid::new_v4().simple()
    );
    postgres_batch_execute(
        &schema_url,
        &format!(
            "CREATE FUNCTION {schema}.{function}() RETURNS event_trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             IF position('CREATE TABLE schema_migration_runs' IN current_query()) > 0 THEN \
                 RAISE EXCEPTION 'intentional metadata bootstrap fault injection'; \
             END IF; END $$; \
             CREATE EVENT TRIGGER {event_trigger} ON ddl_command_start \
             WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION {schema}.{function}()"
        ),
    )
    .expect("install isolated metadata-bootstrap failure event trigger");

    let directory = tempfile::tempdir().expect("temporary metadata-bootstrap project");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    configure_postgres_cli_project(directory.path(), "bootstrap_failure");
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261107120000000000_bootstrap_failure.sql"),
        "-- dbm:dialect postgres\n-- dbm:up\nCREATE TABLE bootstrap_effect (id integer);\n-- dbm:down\nDROP TABLE bootstrap_effect;\n",
    )
    .expect("write metadata-bootstrap fault migration");
    let failed = run_with_environment(
        directory.path(),
        &["--format", "json", "up"],
        "DBM_TEST_DATABASE_URL",
        &schema_url,
    );
    assert!(!failed.status.success());
    assert!(!postgres_metadata_exists(&schema_url).expect("inspect rolled-back metadata"));
    let reserved_table_count: i64 = postgres_query_one(
        &schema_url,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = '{schema}'"
        ),
    )
    .expect("count tables after metadata bootstrap failure")
    .try_get(0)
    .expect("decode reserved table count");
    assert_eq!(reserved_table_count, 0);
    postgres_batch_execute(&schema_url, &format!("DROP EVENT TRIGGER {event_trigger}"))
        .expect("remove metadata-bootstrap event trigger");
    schema_cleanup
        .clean()
        .expect("drop isolated metadata-bootstrap schema");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_runs_mysql_nontransactional_migration_and_recovery_journey() {
    run_mysql_cli_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_competing_writers_same_namespace_are_serialized() {
    run_mysql_cli_contention_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_serializes_namespaces_sharing_database_objects() {
    run_mysql_shared_schema_namespaces_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_metadata_bootstrap_failure_preserves_partial_state_for_reconciliation() {
    run_mysql_metadata_bootstrap_failure_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_metadata_upgrade_failure_preserves_partial_ddl_for_reconciliation() {
    run_mysql_metadata_upgrade_failure_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_process_death_after_intent_before_sql_keeps_the_intent() {
    run_mysql_post_intent_before_sql_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_process_death_before_intent_keeps_run_unresolved() {
    run_mysql_before_intent_journey("DBM_TEST_MYSQL_URL", "mysql");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_process_death_after_sql_before_ack_keeps_intent() {
    run_mysql_acknowledgement_boundary_journey(
        "DBM_TEST_MYSQL_URL",
        "mysql",
        MysqlAcknowledgementBoundary::BeforeAcknowledgement,
    );
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn cli_mysql_process_death_after_ack_before_finalization_keeps_ack() {
    run_mysql_acknowledgement_boundary_journey(
        "DBM_TEST_MYSQL_URL",
        "mysql",
        MysqlAcknowledgementBoundary::BeforeFinalization,
    );
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB LTS instance"]
fn cli_runs_mariadb_nontransactional_migration_and_recovery_journey() {
    run_mysql_cli_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_competing_writers_same_namespace_are_serialized() {
    run_mysql_cli_contention_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_serializes_namespaces_sharing_database_objects() {
    run_mysql_shared_schema_namespaces_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_metadata_bootstrap_failure_preserves_partial_state_for_reconciliation() {
    run_mysql_metadata_bootstrap_failure_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_metadata_upgrade_failure_preserves_partial_ddl_for_reconciliation() {
    run_mysql_metadata_upgrade_failure_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_process_death_after_intent_before_sql_keeps_the_intent() {
    run_mysql_post_intent_before_sql_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_process_death_before_intent_keeps_run_unresolved() {
    run_mysql_before_intent_journey("DBM_TEST_MARIADB_URL", "mariadb");
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_process_death_after_sql_before_ack_keeps_intent() {
    run_mysql_acknowledgement_boundary_journey(
        "DBM_TEST_MARIADB_URL",
        "mariadb",
        MysqlAcknowledgementBoundary::BeforeAcknowledgement,
    );
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn cli_mariadb_process_death_after_ack_before_finalization_keeps_ack() {
    run_mysql_acknowledgement_boundary_journey(
        "DBM_TEST_MARIADB_URL",
        "mariadb",
        MysqlAcknowledgementBoundary::BeforeFinalization,
    );
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

#[test]
fn cli_production_up_and_down_require_the_current_plan_and_target() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let initialized = run(directory.path(), &["init"]);
    assert!(initialized.status.success());
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261021162643981234_create_items.sql"),
        "-- dbm:up\nCREATE TABLE items (id INTEGER);\n-- dbm:down\nDROP TABLE items;\n",
    )
    .expect("write migration");
    let config_path = directory.path().join("dbm.toml");
    let config = fs::read_to_string(&config_path).expect("read generated config");
    assert!(config.contains("environment = \"development\""));
    fs::write(
        &config_path,
        config.replace(
            "environment = \"development\"",
            "environment = \"production\"",
        ),
    )
    .expect("mark the target as production");

    let planned = run(directory.path(), &["--format", "json", "plan"]);
    assert!(
        planned.status.success(),
        "production plan failed: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan = json(&planned);
    let plan_hash = plan["plan_hash"].as_str().expect("plan hash").to_owned();
    let target_fingerprint = plan["target_fingerprint"]
        .as_str()
        .expect("target fingerprint")
        .to_owned();
    assert_eq!(plan["approval_plan_hash"], plan_hash);
    assert!(!directory.path().join("dbm.sqlite").exists());

    let text_plan = run(directory.path(), &["plan"]);
    assert!(text_plan.status.success());
    let text = String::from_utf8_lossy(&text_plan.stdout);
    assert!(text.contains(&format!("Plan {plan_hash}")));
    assert!(text.contains(&format!("Target {target_fingerprint}")));

    let blocked = run(directory.path(), &["--format", "json", "up"]);
    assert_eq!(blocked.status.code(), Some(9));
    assert_eq!(json(&blocked)["errors"][0]["code"], "SAFETY_BLOCKED");
    assert!(!directory.path().join("dbm.sqlite").exists());

    let one_flag = run(
        directory.path(),
        &["--format", "json", "up", "--approve-plan", &plan_hash],
    );
    assert_eq!(one_flag.status.code(), Some(2));
    assert!(!directory.path().join("dbm.sqlite").exists());

    let applied = run(
        directory.path(),
        &[
            "--format",
            "json",
            "up",
            "--approve-plan",
            &plan_hash,
            "--approve-target",
            &target_fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "approved production up failed: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(json(&applied)["status"], "succeeded");
    assert!(directory.path().join("dbm.sqlite").is_file());

    let down_plan = run(
        directory.path(),
        &["--format", "json", "plan", "--down", "--steps", "1"],
    );
    assert!(
        down_plan.status.success(),
        "production rollback plan failed: {}",
        String::from_utf8_lossy(&down_plan.stderr)
    );
    let down_json = json(&down_plan);
    let down_hash = down_json["plan_hash"]
        .as_str()
        .expect("down approval hash")
        .to_owned();
    let down_target = down_json["target_fingerprint"]
        .as_str()
        .expect("down target fingerprint")
        .to_owned();
    assert_ne!(down_hash, plan_hash);
    assert_eq!(down_json["approval_plan_hash"], down_hash);

    let blocked_down = run(directory.path(), &["--format", "json", "down"]);
    assert_eq!(blocked_down.status.code(), Some(9));
    assert_eq!(json(&blocked_down)["errors"][0]["code"], "SAFETY_BLOCKED");

    let up_hash_cannot_roll_back = run(
        directory.path(),
        &[
            "--format",
            "json",
            "down",
            "--approve-plan",
            &plan_hash,
            "--approve-target",
            &down_target,
        ],
    );
    assert_eq!(up_hash_cannot_roll_back.status.code(), Some(9));
    let database = rusqlite::Connection::open(directory.path().join("dbm.sqlite"))
        .expect("open database after rejected rollback");
    let table_exists: bool = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'items')",
            [],
            |row| row.get(0),
        )
        .expect("items table remains");
    assert!(table_exists);
    drop(database);

    let reverted = run(
        directory.path(),
        &[
            "--format",
            "json",
            "down",
            "--approve-plan",
            &down_hash,
            "--approve-target",
            &down_target,
        ],
    );
    assert!(
        reverted.status.success(),
        "approved production down failed: {}",
        String::from_utf8_lossy(&reverted.stderr)
    );
    assert_eq!(
        json(&reverted)["migrations"]
            .as_array()
            .expect("reverted")
            .len(),
        1
    );
}
