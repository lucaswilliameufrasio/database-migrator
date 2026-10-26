#![cfg(feature = "test-runner-fixture")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn configure_project(directory: &Path) {
    let source = directory.join("migrations/versioned");
    fs::create_dir_all(&source).expect("create SQL source directory");
    fs::write(
        source.join("20261024000000000001_filesystem.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE runner_filesystem (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE runner_filesystem;\n",
    )
    .expect("write filesystem migration");

    let runner_path = Path::new(env!("CARGO_BIN_EXE_dbm-test-runner"));
    let runner_name = runner_path
        .file_name()
        .expect("runner binary filename")
        .to_string_lossy();
    let runner_directory = directory.join("runner");
    fs::create_dir_all(&runner_directory).expect("create runner directory");
    fs::copy(runner_path, runner_directory.join(runner_name.as_ref()))
        .expect("copy compiled test runner into isolated project");

    fs::write(
        directory.join("dbm.toml"),
        format!(
            "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n\n[execution]\nrunner = \"runner/{runner_name}\"\nrunner_env = [\"DBM_RUNNER_FIXTURE_MODE\"]\nrunner_timeout_seconds = 30\n"
        ),
    )
    .expect("write runner config");
}

fn run_cli(directory: &Path, arguments: &[&str], runner_mode: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dbm"));
    command
        .current_dir(directory)
        .args(arguments)
        .env_remove("DBM_RUNNER_FIXTURE_MODE");
    if let Some(mode) = runner_mode {
        command.env("DBM_RUNNER_FIXTURE_MODE", mode);
    }
    command.output().expect("start dbm CLI")
}

fn table_exists(database: &Path, name: &str) -> bool {
    let connection = Connection::open(database).expect("open isolated SQLite database");
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .expect("check migration table")
}

#[test]
fn cli_runner_executes_one_mixed_filesystem_embedded_dsl_and_rust_plan() {
    let directory = tempdir().expect("create isolated project");
    configure_project(directory.path());

    let validate = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "validate"],
        None,
    );
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stdout)
    );
    let validate_json: Value = serde_json::from_slice(&validate.stdout).expect("decode validate");
    assert_eq!(validate_json["migration_count"], 4);

    assert!(
        !directory.path().join("dbm.sqlite").exists(),
        "runner validate must not create the target database"
    );

    let plan = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "plan"],
        None,
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stdout)
    );
    let plan_json: Value = serde_json::from_slice(&plan.stdout).expect("decode runner plan");
    assert_eq!(plan_json["operations"].as_array().map(Vec::len), Some(4));

    let up = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "up"],
        None,
    );
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stdout)
    );
    let up_json: Value = serde_json::from_slice(&up.stdout).expect("decode runner execution");
    assert_eq!(up_json["migrations"].as_array().map(Vec::len), Some(4));
    for table in [
        "runner_filesystem",
        "runner_embedded",
        "runner_dsl",
        "runner_rust",
    ] {
        assert!(table_exists(&directory.path().join("dbm.sqlite"), table));
    }

    let history = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "history"],
        None,
    );
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stdout)
    );
    let history_json: Value = serde_json::from_slice(&history.stdout).expect("decode history");
    assert_eq!(history_json["runs"].as_array().map(Vec::len), Some(4));

    let applied_status = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "status"],
        None,
    );
    assert!(applied_status.status.success());
    let applied_status_json: Value =
        serde_json::from_slice(&applied_status.stdout).expect("decode applied status");
    assert!(
        applied_status_json["migrations"]
            .as_array()
            .expect("status migration array")
            .iter()
            .all(|migration| migration["state"] == "applied")
    );

    let applied_doctor = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "doctor"],
        None,
    );
    assert!(applied_doctor.status.success());
    let applied_doctor_json: Value =
        serde_json::from_slice(&applied_doctor.stdout).expect("decode applied doctor");
    assert_eq!(applied_doctor_json["status"], "healthy");

    let down = run_cli(
        directory.path(),
        &[
            "--config", "dbm.toml", "--format", "json", "down", "--steps", "4",
        ],
        None,
    );
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stdout)
    );
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("decode runner rollback");
    assert_eq!(down_json["migrations"].as_array().map(Vec::len), Some(4));
    for table in [
        "runner_filesystem",
        "runner_embedded",
        "runner_dsl",
        "runner_rust",
    ] {
        assert!(!table_exists(&directory.path().join("dbm.sqlite"), table));
    }
}

#[test]
fn cli_namespace_override_survives_runner_handshake_and_filters_metadata() {
    let directory = tempdir().expect("create isolated project");
    configure_project(directory.path());

    let tenant_plan = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--namespace",
            "tenant_a",
            "--format",
            "json",
            "plan",
        ],
        None,
    );
    assert!(
        tenant_plan.status.success(),
        "{}",
        String::from_utf8_lossy(&tenant_plan.stdout)
    );
    let tenant_plan_json: Value =
        serde_json::from_slice(&tenant_plan.stdout).expect("decode tenant plan");
    assert_eq!(tenant_plan_json["namespace"], "tenant_a");

    let tenant_up = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--namespace",
            "tenant_a",
            "--format",
            "json",
            "up",
        ],
        None,
    );
    assert!(
        tenant_up.status.success(),
        "{}",
        String::from_utf8_lossy(&tenant_up.stdout)
    );
    let tenant_up_json: Value =
        serde_json::from_slice(&tenant_up.stdout).expect("decode tenant up");
    assert_eq!(tenant_up_json["namespace"], "tenant_a");

    let default_status = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "status"],
        None,
    );
    assert!(default_status.status.success());
    let default_status_json: Value =
        serde_json::from_slice(&default_status.stdout).expect("decode default status");
    assert_eq!(default_status_json["namespace"], "default");
    assert_eq!(default_status_json["migrations"][0]["state"], "pending");

    let tenant_status = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--namespace",
            "tenant_a",
            "--format",
            "json",
            "status",
        ],
        None,
    );
    assert!(tenant_status.status.success());
    let tenant_status_json: Value =
        serde_json::from_slice(&tenant_status.stdout).expect("decode tenant status");
    assert_eq!(tenant_status_json["namespace"], "tenant_a");
    assert_eq!(tenant_status_json["migrations"][0]["state"], "applied");

    let invalid_namespace = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--namespace",
            "INVALID/namespace",
            "--format",
            "json",
            "plan",
        ],
        None,
    );
    assert!(!invalid_namespace.status.success());
    assert!(!String::from_utf8_lossy(&invalid_namespace.stdout).contains("\"status\":\"ready\""));
}

#[test]
fn incompatible_runner_handshake_never_falls_back_to_partial_sql() {
    let directory = tempdir().expect("create isolated project");
    configure_project(directory.path());

    let result = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "up"],
        Some("bad_protocol"),
    );
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("protocol version is incompatible")
            || String::from_utf8_lossy(&result.stderr).contains("protocol version is incompatible")
    );
    assert!(
        !directory.path().join("dbm.sqlite").exists(),
        "CLI must not apply filesystem SQL before runner compatibility is verified"
    );
}

#[test]
fn missing_runner_timeout_and_extra_frames_fail_without_partial_sql() {
    let missing_runner_project = tempdir().expect("create missing-runner project");
    configure_project(missing_runner_project.path());
    let config_path = missing_runner_project.path().join("dbm.toml");
    let config = fs::read_to_string(&config_path).expect("read runner config");
    fs::write(
        &config_path,
        config.replace("runner/dbm-test-runner", "runner/not-installed"),
    )
    .expect("configure absent runner");
    let missing = run_cli(
        missing_runner_project.path(),
        &["--config", "dbm.toml", "--format", "json", "up"],
        None,
    );
    assert!(!missing.status.success());
    assert!(!missing_runner_project.path().join("dbm.sqlite").exists());

    let timeout_project = tempdir().expect("create timeout project");
    configure_project(timeout_project.path());
    let config_path = timeout_project.path().join("dbm.toml");
    let config = fs::read_to_string(&config_path).expect("read timeout config");
    fs::write(
        &config_path,
        config.replace("runner_timeout_seconds = 30", "runner_timeout_seconds = 1"),
    )
    .expect("set short runner timeout");
    let timeout = run_cli(
        timeout_project.path(),
        &["--config", "dbm.toml", "--format", "json", "plan"],
        Some("sleep"),
    );
    assert!(!timeout.status.success());
    assert!(!timeout_project.path().join("dbm.sqlite").exists());

    let extra_frame_project = tempdir().expect("create extra-frame project");
    configure_project(extra_frame_project.path());
    let extra = run_cli(
        extra_frame_project.path(),
        &["--config", "dbm.toml", "--format", "json", "plan"],
        Some("extra_plan_frame"),
    );
    assert!(!extra.status.success());
    assert!(!extra_frame_project.path().join("dbm.sqlite").exists());
}

#[test]
fn runner_rejects_divergent_manifest_fingerprint_before_mutation() {
    let directory = tempdir().expect("create fingerprint test project");
    configure_project(directory.path());
    let config_path =
        fs::canonicalize(directory.path().join("dbm.toml")).expect("canonical config path");
    let runner = Path::new(env!("CARGO_BIN_EXE_dbm-test-runner"));
    let mut child = Command::new(runner)
        .current_dir(directory.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start compiled fixture runner");
    let mut input = child.stdin.take().expect("runner stdin");
    let stdout = child.stdout.take().expect("runner stdout");
    let mut output = BufReader::new(stdout);
    let hello = serde_json::json!({
        "kind": "hello",
        "protocol_version": 2,
        "engine_version": env!("CARGO_PKG_VERSION"),
        "metadata_schema_version": 5,
        "runner_fingerprint": test_runner_fingerprint(),
        "config_path": config_path,
        "namespace": "default",
        "action": "up",
        "down_steps": null
    });
    writeln!(input, "{hello}").expect("send runner handshake");
    input.flush().expect("flush runner handshake");
    let mut ready_line = String::new();
    output
        .read_line(&mut ready_line)
        .expect("read runner handshake response");
    let ready: Value = serde_json::from_str(&ready_line).expect("decode ready response");
    assert_eq!(ready["kind"], "ready");

    let mut execute = serde_json::json!({
        "kind": "execute",
        "protocol_version": 2,
        "runner_fingerprint": ready["runner_fingerprint"],
        "namespace": "default",
        "action": "up",
        "down_steps": null,
        "manifest_fingerprint": ready["manifest_fingerprint"],
        "plan_hash": ready["plan_hash"],
        "target_fingerprint": ready["target_fingerprint"]
    });
    execute["manifest_fingerprint"] = Value::String("sha256:deliberately-divergent".to_owned());
    writeln!(input, "{execute}").expect("send execution request with bad fingerprint");
    input.flush().expect("flush execution request");
    drop(input);

    let mut error_line = String::new();
    output
        .read_line(&mut error_line)
        .expect("read fingerprint rejection");
    let response: Value = serde_json::from_str(&error_line).expect("decode error response");
    assert_eq!(response["kind"], "error");
    assert_eq!(response["stage"], "execute_validation");
    assert!(child.wait().expect("wait for runner").success());
    assert!(
        !directory.path().join("dbm.sqlite").exists(),
        "runner must reject manifest divergence before target mutation"
    );
}

fn test_runner_fingerprint() -> String {
    let bytes = fs::read(env!("CARGO_BIN_EXE_dbm-test-runner"))
        .expect("read compiled test runner for fingerprint");
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("write fingerprint hex");
    }
    format!("sha256:raw:v1:{hex}")
}
