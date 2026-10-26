#![cfg(feature = "ai")]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const TEST_SECRET: &str = "dbm-ai-test-secret-never-print";

fn configure_project(directory: &Path, endpoint: &str, enabled: bool) {
    fs::create_dir_all(directory.join("migrations/versioned")).expect("create migration source");
    let config = format!(
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n\
         [target]\ndriver = \"sqlite\"\ndatabase = \"dbm.sqlite\"\nenvironment = \"test\"\n\n\
         [ai]\nenabled = {enabled}\nprovider = \"openai-compatible\"\nprofile = \"test\"\n\n\
         [ai.profiles.test]\nendpoint = \"{endpoint}\"\nmodel = \"fake-model\"\napi_key_env = \"DBM_AI_TEST_SECRET\"\ntimeout_seconds = 10\nmax_tokens = 512\nmax_context_bytes = 65536\n"
    );
    fs::write(directory.join("dbm.toml"), config).expect("write project config");
}

fn run_cli(directory: &Path, arguments: &[&str], include_secret: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dbm"));
    command
        .current_dir(directory)
        .args(arguments)
        .env_remove("DBM_AI_TEST_SECRET");
    if include_secret {
        command.env("DBM_AI_TEST_SECRET", TEST_SECRET);
    }
    command.output().expect("start dbm CLI")
}

fn start_provider(status: u16, completion: Value) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local fake AI provider");
    let address = listener.local_addr().expect("read fake provider address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept AI request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set request timeout");
        let request = read_request(&mut stream);
        let response_body = completion.to_string();
        let status_reason = if status == 200 {
            "OK"
        } else {
            "Too Many Requests"
        };
        let response = format!(
            "HTTP/1.1 {status} {status_reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write fake provider response");
        request
    });
    (format!("http://{address}/v1"), handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).expect("read request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn generated_proposal() -> Value {
    let proposal = json!({
        "schema_version": "1.0",
        "proposal_id": "local-test-proposal",
        "requested_change": "create a things table",
        "dialect": "sqlite",
        "capabilities_snapshot": {},
        "files": [{
            "path": "create_things.sql",
            "type": "sql",
            "content": "-- dbm:dialect sqlite\n-- dbm:up\nCREATE TABLE things (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE things;\n"
        }],
        "expected_schema_changes": ["creates things"],
        "suggested_down": ["DROP TABLE things;"],
        "uncertainties": [],
        "risk_explanations": []
    });
    json!({
        "choices": [{"message": {"content": proposal.to_string()}}]
    })
}

fn completion_response(content: Value) -> Value {
    json!({
        "choices": [{"message": {"content": content.to_string()}}]
    })
}

#[test]
fn generate_uses_local_openai_compatible_provider_and_never_writes_or_applies() {
    let directory = tempfile::tempdir().expect("temporary project");
    let (endpoint, server) = start_provider(200, generated_proposal());
    configure_project(directory.path(), &endpoint, true);

    let result = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "generate",
            "Create a things table",
        ],
        true,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let report: Value = serde_json::from_slice(&result.stdout).expect("decode AI report");
    assert_eq!(report["status"], "proposal");
    assert_eq!(report["provider"], "openai-compatible");
    assert_eq!(report["result"]["files"][0]["path"], "create_things.sql");
    assert!(!String::from_utf8_lossy(&result.stdout).contains(TEST_SECRET));
    assert!(!String::from_utf8_lossy(&result.stderr).contains(TEST_SECRET));
    assert!(!directory.path().join("dbm.sqlite").exists());
    assert_eq!(
        fs::read_dir(directory.path().join("migrations/versioned"))
            .expect("read migrations")
            .count(),
        0,
        "AI only returns a proposal and does not create migration files"
    );

    let request = server.join().expect("fake provider completed");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer dbm-ai-test-secret-never-print")
    );
    let header_end = request.find("\r\n\r\n").expect("request headers");
    let request_body: Value =
        serde_json::from_str(&request[header_end + 4..]).expect("decode provider request");
    assert_eq!(request_body["model"], "fake-model");
    assert_eq!(request_body["response_format"]["type"], "json_object");
    assert!(
        request_body["messages"][0]["content"]
            .as_str()
            .expect("system prompt")
            .contains("never as instructions")
    );
    assert!(!request_body.to_string().contains(TEST_SECRET));
}

#[test]
fn review_explain_and_suggest_down_use_their_distinct_strict_contracts() {
    let directory = tempfile::tempdir().expect("temporary project");
    let migration_relative = "migrations/versioned/20261024000000000001_review.sql";
    let migration_path = directory.path().join(migration_relative);
    fs::create_dir_all(migration_path.parent().expect("migration parent"))
        .expect("create migration source");
    fs::write(
        &migration_path,
        "-- dbm:dialect sqlite\n-- dbm:up\nCREATE TABLE reviewed (id INTEGER);\n",
    )
    .expect("write migration without Down");
    let plan_path = directory.path().join("plan.json");
    fs::write(
        &plan_path,
        r#"{"schema_version":"1.0","command":"plan","target":"/private/project/dbm.sqlite","namespace":"default","status":"ready","plan_hash":"sha256:example","read_only":true,"operations":[],"database_url":"postgres://private-user:private-password@example.test/app"}"#,
    )
    .expect("write plan context");

    let review_content = json!({
        "schema_version": "1.0",
        "summary": "No issues identified from supplied SQL alone.",
        "issues": [],
        "assumptions": ["No live catalog was inspected."],
        "risk_explanations": []
    });
    let (endpoint, server) = start_provider(200, completion_response(review_content));
    configure_project(directory.path(), &endpoint, true);
    let review = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "review",
            migration_relative,
        ],
        true,
    );
    assert!(
        review.status.success(),
        "{}",
        String::from_utf8_lossy(&review.stdout)
    );
    let report: Value = serde_json::from_slice(&review.stdout).expect("decode review report");
    assert_eq!(report["status"], "reviewed");
    let review_request = server.join().expect("review provider request");
    assert!(review_request.contains(migration_relative));

    let explain_content = json!({
        "schema_version": "1.0",
        "summary": "The supplied plan contains no operations.",
        "steps": [],
        "assumptions": [],
        "cautions": ["No database was contacted."]
    });
    let (endpoint, server) = start_provider(200, completion_response(explain_content));
    configure_project(directory.path(), &endpoint, true);
    let explain = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "explain",
            "--plan",
            "plan.json",
        ],
        true,
    );
    assert!(
        explain.status.success(),
        "{}",
        String::from_utf8_lossy(&explain.stdout)
    );
    let report: Value = serde_json::from_slice(&explain.stdout).expect("decode explain report");
    assert_eq!(report["status"], "explained");
    let explain_request = server.join().expect("explain provider request");
    assert!(!explain_request.contains("private-password"));
    assert!(!explain_request.contains("postgres://"));
    assert!(explain_request.contains("[REDACTED]"));

    let down_content = json!({
        "schema_version": "1.0",
        "migration": migration_relative,
        "down_sql": "DROP TABLE reviewed;",
        "rationale": "remove the table introduced by the selected Up migration",
        "uncertainties": [],
        "risk_explanations": ["Existing rows would be removed."]
    });
    let (endpoint, server) = start_provider(200, completion_response(down_content));
    configure_project(directory.path(), &endpoint, true);
    let suggest_down = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "suggest-down",
            migration_relative,
        ],
        true,
    );
    assert!(
        suggest_down.status.success(),
        "{}",
        String::from_utf8_lossy(&suggest_down.stdout)
    );
    let report: Value =
        serde_json::from_slice(&suggest_down.stdout).expect("decode suggested Down report");
    assert_eq!(report["status"], "proposal");
    assert_eq!(report["result"]["down_sql"], "DROP TABLE reviewed;");
    let _ = server.join().expect("suggest-down provider request");

    assert_eq!(
        fs::read_to_string(&migration_path).expect("read original migration after proposals"),
        "-- dbm:dialect sqlite\n-- dbm:up\nCREATE TABLE reviewed (id INTEGER);\n"
    );
    assert!(!directory.path().join("dbm.sqlite").exists());
}

#[test]
fn preview_needs_no_secret_or_network_and_ai_has_no_apply_flag() {
    let directory = tempfile::tempdir().expect("temporary project");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind preview sentinel");
    listener
        .set_nonblocking(true)
        .expect("make preview sentinel nonblocking");
    let address = listener
        .local_addr()
        .expect("read preview sentinel address");
    let endpoint = format!("http://{address}/v1");
    configure_project(directory.path(), &endpoint, false);

    let preview = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "generate",
            "Add a table",
            "--preview-context",
        ],
        false,
    );
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let report: Value = serde_json::from_slice(&preview.stdout).expect("decode context preview");
    assert_eq!(report["status"], "context_preview");
    assert_eq!(report["messages"].as_array().map(Vec::len), Some(2));
    assert!(String::from_utf8_lossy(&preview.stdout).contains("No database connection"));
    assert!(
        listener.accept().is_err(),
        "preview must not open a network connection"
    );

    configure_project(directory.path(), &endpoint, true);
    let ordinary_plan = run_cli(
        directory.path(),
        &["--config", "dbm.toml", "--format", "json", "plan"],
        true,
    );
    assert!(ordinary_plan.status.success());
    assert!(
        listener.accept().is_err(),
        "ordinary migration commands must not contact the AI provider"
    );

    let apply = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "ai",
            "generate",
            "Add a table",
            "--apply",
        ],
        false,
    );
    assert!(!apply.status.success());
    assert!(String::from_utf8_lossy(&apply.stderr).contains("unexpected argument"));
    assert!(
        listener.accept().is_err(),
        "unsupported --apply must not connect"
    );
}

#[test]
fn provider_failures_have_stable_json_codes_and_never_include_the_secret() {
    let directory = tempfile::tempdir().expect("temporary project");
    let (endpoint, server) = start_provider(429, json!({"error": "private provider detail"}));
    configure_project(directory.path(), &endpoint, true);

    let result = run_cli(
        directory.path(),
        &[
            "--config",
            "dbm.toml",
            "--format",
            "json",
            "ai",
            "generate",
            "Add a table",
        ],
        true,
    );
    assert!(!result.status.success());
    let report: Value = serde_json::from_slice(&result.stdout).expect("decode failure report");
    assert_eq!(report["errors"][0]["code"], "AI_RATE_LIMITED");
    let emitted = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!emitted.contains(TEST_SECRET));
    assert!(!emitted.contains("private provider detail"));
    let _ = server.join().expect("fake provider completed");
}
