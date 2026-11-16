use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const VERSION_ONE: &str = "20261115000000000001";
const VERSION_TWO: &str = "20261115000000000002";
const VERSION_THREE: &str = "20261115000000000003";

fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("dbm process starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

fn write_config(directory: &Path, target: &str) {
    fs::create_dir_all(directory.join("migrations/versioned")).expect("migration directory");
    fs::write(directory.join("dbm.toml"), target).expect("config");
}

fn sqlite_config(environment: &str) -> String {
    format!(
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"sqlite\"\ndatabase = \"app.sqlite\"\nenvironment = \"{environment}\"\n\n\
         [execution]\nbusy_timeout_seconds = 30\n"
    )
}

fn write_version(directory: &Path, version: &str, description: &str, body: &str) {
    let path = directory
        .join("migrations/versioned")
        .join(format!("{version}_{description}.sql"));
    fs::write(path, body).expect("migration");
}

fn note_chain(directory: &Path) {
    write_version(
        directory,
        VERSION_ONE,
        "create_note",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\n\
         CREATE TABLE note (id INTEGER PRIMARY KEY);\n\
         INSERT INTO note (id) VALUES (1);\n\n\
         -- dbm:down\nINSERT INTO down_was_executed (id) VALUES (1);\n",
    );
    write_version(
        directory,
        VERSION_TWO,
        "add_name",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\n\
         ALTER TABLE note ADD COLUMN name TEXT NOT NULL DEFAULT 'from-v2';\n\n\
         -- dbm:down\nINSERT INTO down_was_executed (id) VALUES (2);\n",
    );
    write_version(
        directory,
        VERSION_THREE,
        "keep_name",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\n\
         UPDATE note SET name = 'kept' WHERE id = 1;\n\n\
         -- dbm:down\nINSERT INTO down_was_executed (id) VALUES (3);\n",
    );
}

fn record(directory: &Path, name: &str, through: &str) -> Output {
    run(directory, &["chain", "record", name, "--through", through])
}

#[test]
fn chain_replays_each_published_schema_without_opening_the_configured_database() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());

    let first = record(directory.path(), "store-1.0", VERSION_ONE);
    assert!(
        first.status.success(),
        "record store-1.0: {}",
        stderr(&first)
    );
    let second = record(directory.path(), "store-1.1", VERSION_TWO);
    assert!(
        second.status.success(),
        "record store-1.1: {}",
        stderr(&second)
    );
    let published = fs::read_to_string(directory.path().join("published/store-1.0.txt"))
        .expect("published schema");
    assert!(published.contains(VERSION_ONE));
    assert!(published.contains("sha256:raw:v1:"));
    assert!(!published.contains("down_was_executed"));

    let replay = run(directory.path(), &["--format", "json", "chain"]);
    assert!(replay.status.success(), "chain: {}", stderr(&replay));
    let report = json(&replay);
    assert_eq!(report["command"], "chain");
    assert_eq!(report["status"], "ok");
    assert_eq!(
        report["fresh"]["applied"].as_array().expect("fresh").len(),
        3
    );
    assert_eq!(report["published"].as_array().expect("published").len(), 2);
    assert_eq!(report["published"][0]["name"], "store-1.0");
    assert_eq!(report["published"][0]["head"], VERSION_ONE);
    assert_eq!(report["published"][1]["head"], VERSION_TWO);
    assert!(!directory.path().join("app.sqlite").exists());
    assert!(!directory.path().join("app.sqlite.dbm-owner").exists());
}

#[test]
fn edited_shipped_sql_fails_closed_and_leaves_the_configured_database() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("production"));
    note_chain(directory.path());
    let marker = directory.path().join("app.sqlite");
    fs::write(&marker, b"marker").expect("marker");
    assert!(
        record(directory.path(), "store-1.0", VERSION_ONE)
            .status
            .success()
    );

    let migration = directory
        .path()
        .join("migrations/versioned")
        .join(format!("{VERSION_ONE}_create_note.sql"));
    let mut sql = fs::read_to_string(&migration).expect("read migration");
    sql = sql.replace("VALUES (1)", "VALUES (9)");
    fs::write(&migration, sql).expect("edit shipped sql");

    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(4));
    let error = stderr(&replay);
    assert!(error.contains("checksum mismatch"), "{error}");
    assert!(error.contains(VERSION_ONE), "{error}");
    assert_eq!(fs::read(&marker).expect("marker bytes"), b"marker");
}

#[test]
fn pending_sql_runs_and_a_failure_removes_the_disposable_file() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    write_version(
        directory.path(),
        VERSION_ONE,
        "create_note",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\n\
         CREATE TABLE note (id INTEGER PRIMARY KEY);\n\n\
         -- dbm:down\nINSERT INTO down_was_executed (id) VALUES (1);\n",
    );
    write_version(
        directory.path(),
        VERSION_TWO,
        "missing",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\n\
         INSERT INTO missing_table (id) VALUES (1);\n\n\
         -- dbm:down\nINSERT INTO down_was_executed (id) VALUES (2);\n",
    );
    assert!(
        record(directory.path(), "store-1.0", VERSION_ONE)
            .status
            .success()
    );
    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(6));
    let error = stderr(&replay);
    assert!(error.contains(VERSION_TWO), "{error}");
    assert!(error.contains("pending"), "{error}");
    assert!(!directory.path().join("app.sqlite").exists());
}

#[test]
fn published_schema_must_be_a_prefix_of_the_current_chain() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());
    fs::create_dir(directory.path().join("published")).expect("published");
    fs::write(
        directory.path().join("published/skipped.txt"),
        format!(
            "{VERSION_TWO} sha256:raw:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
        ),
    )
    .expect("bad published schema");
    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(4));
    assert!(stderr(&replay).contains("not a prefix"));
    assert!(!directory.path().join("app.sqlite").exists());
}

#[test]
fn empty_published_directory_runs_the_fresh_chain_only() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());
    fs::create_dir(directory.path().join("published")).expect("published");
    let replay = run(directory.path(), &["chain"]);
    assert!(replay.status.success(), "{}", stderr(&replay));
    let text = stdout(&replay);
    assert!(text.contains("fresh applied 3"), "{text}");
    assert!(text.contains("published 0"), "{text}");
    assert!(!directory.path().join("app.sqlite").exists());
}

#[test]
fn missing_published_directory_is_refused() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());
    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(2));
    assert!(stderr(&replay).contains("published"));
    assert!(!directory.path().join("app.sqlite").exists());
}

#[test]
fn record_refuses_to_overwrite_a_published_schema() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());
    assert!(
        record(directory.path(), "store-1.0", VERSION_ONE)
            .status
            .success()
    );
    let path = directory.path().join("published/store-1.0.txt");
    let before = fs::read(&path).expect("published bytes");
    let again = record(directory.path(), "store-1.0", VERSION_TWO);
    assert!(!again.status.success());
    assert_eq!(again.status.code(), Some(2));
    assert!(stderr(&again).contains("overwrite"));
    assert_eq!(fs::read(&path).expect("published bytes again"), before);
}

#[test]
fn chain_refuses_a_server_driver_repeatable_sql_and_transaction_control() {
    let postgres = tempfile::tempdir().expect("postgres project");
    write_config(
        postgres.path(),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"postgres\"\nurl_env = \"DBM_CHAIN_URL\"\nenvironment = \"test\"\n\
         allow_insecure_transport = true\n\n[execution]\nbusy_timeout_seconds = 30\n",
    );
    fs::create_dir(postgres.path().join("published")).expect("published");
    let refused = run(postgres.path(), &["chain"]);
    assert!(!refused.status.success());
    assert_eq!(refused.status.code(), Some(13));
    assert!(stderr(&refused).contains("not sqlite"));

    let repeatable = tempfile::tempdir().expect("repeatable project");
    write_config(repeatable.path(), &sqlite_config("development"));
    fs::write(
        repeatable.path().join("migrations/versioned/R__seed.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nSELECT 1;\n",
    )
    .expect("repeatable");
    fs::create_dir(repeatable.path().join("published")).expect("published");
    let repeatable_run = run(repeatable.path(), &["chain"]);
    assert!(!repeatable_run.status.success());
    assert_eq!(repeatable_run.status.code(), Some(3));
    assert!(stderr(&repeatable_run).contains("versioned SQL only"));

    let controlled = tempfile::tempdir().expect("transaction project");
    write_config(controlled.path(), &sqlite_config("development"));
    write_version(
        controlled.path(),
        VERSION_ONE,
        "begin",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nBEGIN;\nSELECT 1;\n",
    );
    fs::create_dir(controlled.path().join("published")).expect("published");
    let controlled_run = run(controlled.path(), &["chain"]);
    assert!(!controlled_run.status.success());
    assert_eq!(controlled_run.status.code(), Some(9));
    assert!(stderr(&controlled_run).contains("begin"));

    let runner = tempfile::tempdir().expect("runner project");
    write_config(
        runner.path(),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"sqlite\"\ndatabase = \"app.sqlite\"\nenvironment = \"development\"\n\n\
         [execution]\nbusy_timeout_seconds = 30\nrunner = \"runner.sh\"\n",
    );
    write_version(
        runner.path(),
        VERSION_ONE,
        "create_note",
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nSELECT 1;\n",
    );
    fs::create_dir(runner.path().join("published")).expect("published");
    let runner_result = run(runner.path(), &["chain"]);
    assert!(!runner_result.status.success());
    assert_eq!(runner_result.status.code(), Some(13));
    assert!(stderr(&runner_result).contains("does not call the migration runner"));
    assert!(!runner.path().join("app.sqlite").exists());
}

#[test]
fn server_dialect_in_the_phone_directory_is_refused() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    write_version(
        directory.path(),
        VERSION_ONE,
        "server",
        "-- dbm:dialect postgres\n-- dbm:transaction never\n-- dbm:up\nSELECT 1;\n",
    );
    fs::create_dir(directory.path().join("published")).expect("published");
    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(3));
    assert!(stderr(&replay).contains("dialect"));
    assert!(!directory.path().join("app.sqlite").exists());
}

#[cfg(unix)]
#[test]
fn published_symlink_is_refused() {
    let directory = tempfile::tempdir().expect("project");
    write_config(directory.path(), &sqlite_config("development"));
    note_chain(directory.path());
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), directory.path().join("published"))
        .expect("symlink");
    let replay = run(directory.path(), &["chain"]);
    assert!(!replay.status.success());
    assert_eq!(replay.status.code(), Some(2));
    assert!(stderr(&replay).contains("published"));
    assert!(!directory.path().join("app.sqlite").exists());
}
