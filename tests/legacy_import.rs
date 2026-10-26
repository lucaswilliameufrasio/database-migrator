use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::tempdir;

fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("start dbm")
}

fn configure_project(directory: &Path) {
    fs::create_dir_all(directory.join("migrations/versioned"))
        .expect("create empty migration source");
    fs::write(
        directory.join("dbm.toml"),
        "version = 1\nnamespace = \"default\"\nsource = [\"migrations/versioned\"]\n\n[target]\ndriver = \"sqlite\"\ndatabase = \"legacy.sqlite\"\nenvironment = \"test\"\n",
    )
    .expect("write isolated config");
}

fn create_legacy_fixtures(database: &Path) {
    let connection = Connection::open(database).expect("create legacy fixture database");
    connection
        .execute_batch(
            "CREATE TABLE knex_migrations (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 batch INTEGER NOT NULL,
                 migration_time TEXT
             );
             CREATE TABLE knex_migrations_lock (
                 \"index\" INTEGER PRIMARY KEY,
                 is_locked INTEGER NOT NULL
             );
             INSERT INTO knex_migrations VALUES
                 (1, '20261001000000000000_create_users.js', 1, '2026-10-01T00:00:00Z'),
                 (2, '20261002000000000000_create_orders.ts', 2, '2026-10-02T00:00:00Z');
             INSERT INTO knex_migrations_lock VALUES (1, 0);

             CREATE TABLE goose_db_version (
                 id INTEGER PRIMARY KEY,
                 version_id INTEGER NOT NULL,
                 is_applied INTEGER NOT NULL,
                 tstamp TEXT
             );
             INSERT INTO goose_db_version VALUES
                 (1, 0, 1, '2026-10-01T00:00:00Z'),
                 (2, 100, 1, '2026-10-01T00:01:00Z'),
                 (3, 200, 1, '2026-10-01T00:02:00Z'),
                 (4, 100, 0, '2026-10-01T00:03:00Z');

             CREATE TABLE _sqlx_migrations (
                 version INTEGER PRIMARY KEY,
                 description TEXT NOT NULL,
                 installed_on TEXT,
                 success INTEGER NOT NULL,
                 checksum BLOB NOT NULL,
                 execution_time INTEGER NOT NULL
             );
             INSERT INTO _sqlx_migrations VALUES
                 (42, 'create_accounts', '2026-10-03T00:00:00Z', 1, zeroblob(48), 19),
                 (43, 'create_invoices', NULL, 0, zeroblob(48), 0);",
        )
        .expect("insert representative legacy histories");
}

fn run_import(directory: &Path, action: &str, tool: &str) -> Output {
    run(
        directory,
        &["--format", "json", "import", action, "--from", tool],
    )
}

fn decode(output: &Output, label: &str) -> Value {
    assert!(
        output.status.success(),
        "{label} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("decode import JSON")
}

fn decode_any(output: &Output, label: &str) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{label} returned invalid JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn inspect_and_plan_preserve_legacy_history_without_mutating_the_database() {
    let directory = tempdir().expect("create import project");
    configure_project(directory.path());
    let database_path = directory.path().join("legacy.sqlite");
    create_legacy_fixtures(&database_path);

    let knex_inspection = decode(
        &run_import(directory.path(), "inspect", "knex"),
        "Knex inspect",
    );
    assert_eq!(
        knex_inspection["source"]["events"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(
        knex_inspection["source"]["current_migrations"][1]["filename"],
        "20261002000000000000_create_orders.ts"
    );
    assert_eq!(
        knex_inspection["source"]["current_migrations"][1]["batch_id"],
        "2"
    );
    assert!(
        knex_inspection["source"]["source_identity"]
            .as_str()
            .is_some_and(|identity| identity.starts_with("sha256:"))
    );

    let knex_plan = decode(&run_import(directory.path(), "plan", "knex"), "Knex plan");
    assert_eq!(knex_plan["status"], "ready");
    assert_eq!(
        knex_plan["local_migrations"].as_array().map(Vec::len),
        Some(0)
    );
    let digest = knex_plan["digest"].as_str().expect("plan digest");
    assert!(digest.starts_with("sha256:"));
    let repeated_plan = decode(
        &run_import(directory.path(), "plan", "knex"),
        "repeated Knex plan",
    );
    assert_eq!(repeated_plan["digest"], knex_plan["digest"]);

    let goose = decode(
        &run_import(directory.path(), "inspect", "goose"),
        "Goose inspect",
    );
    assert_eq!(goose["source"]["events"][0]["state"], "sentinel");
    assert_eq!(
        goose["source"]["current_migrations"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        goose["source"]["current_migrations"][0]["original_version"],
        "200"
    );

    let sqlx = decode(
        &run_import(directory.path(), "inspect", "sqlx"),
        "SQLx inspect",
    );
    assert_eq!(
        sqlx["source"]["events"][0]["checksum"]["algorithm"],
        "sha384"
    );
    assert_eq!(
        sqlx["source"]["current_migrations"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let sqlx_plan = decode(&run_import(directory.path(), "plan", "sqlx"), "SQLx plan");
    assert_eq!(sqlx_plan["status"], "blocked");
    assert_eq!(sqlx_plan["blockers"][0]["code"], "SQLX_MIGRATION_FAILED");

    let connection = Connection::open(&database_path).expect("inspect fixture database");
    let metadata_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name LIKE 'schema_migration_%'",
            [],
            |row| row.get(0),
        )
        .expect("count new metadata tables");
    assert_eq!(
        metadata_table_count, 0,
        "inspect/plan must not initialize metadata"
    );
    let original_knex_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM knex_migrations", [], |row| row.get(0))
        .expect("read original Knex history");
    assert_eq!(
        original_knex_rows, 2,
        "import planning must not rewrite source history"
    );
}

#[test]
fn import_plan_blocks_active_knex_lock_and_existing_new_metadata() {
    let directory = tempdir().expect("create lock test project");
    configure_project(directory.path());
    let database_path = directory.path().join("legacy.sqlite");
    create_legacy_fixtures(&database_path);
    let connection = Connection::open(&database_path).expect("open fixture database");
    connection
        .execute("UPDATE knex_migrations_lock SET is_locked = 1", [])
        .expect("mark legacy lock active");

    let locked = decode(
        &run_import(directory.path(), "plan", "knex"),
        "locked Knex plan",
    );
    assert_eq!(locked["status"], "blocked");
    assert_eq!(locked["blockers"][0]["code"], "LEGACY_LOCK_ACTIVE");

    drop(connection);

    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261024000000000001_create_native.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE native_migration (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE native_migration;\n",
    )
    .expect("write native migration to initialize metadata");
    let applied = run(directory.path(), &["up"]);
    assert!(
        applied.status.success(),
        "initialize metadata: {}",
        String::from_utf8_lossy(&applied.stderr)
    );

    let initialized = decode(
        &run_import(directory.path(), "plan", "knex"),
        "existing metadata plan",
    );
    assert_eq!(initialized["status"], "blocked");
    assert!(
        initialized["blockers"]
            .as_array()
            .expect("plan blockers")
            .iter()
            .any(|blocker| blocker["code"] == "TARGET_METADATA_ALREADY_INITIALIZED")
    );
}

#[test]
fn import_read_paths_refuse_to_create_a_missing_database() {
    let directory = tempdir().expect("create missing database project");
    configure_project(directory.path());
    let result = run_import(directory.path(), "inspect", "knex");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("does not exist"));
    assert!(!directory.path().join("legacy.sqlite").exists());
}

#[test]
fn import_plan_blocks_native_identity_collisions_and_timestamped_out_of_order() {
    let directory = tempdir().expect("create ordering test project");
    configure_project(directory.path());
    create_legacy_fixtures(&directory.path().join("legacy.sqlite"));

    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261001000000000000_create_users.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE local_users (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE local_users;\n",
    )
    .expect("write colliding native migration");
    let collision = decode(
        &run_import(directory.path(), "plan", "knex"),
        "colliding migration plan",
    );
    assert!(
        collision["blockers"]
            .as_array()
            .expect("collision blockers")
            .iter()
            .any(|blocker| blocker["code"] == "LEGACY_NATIVE_IDENTITY_COLLISION")
    );
    assert_eq!(
        collision["local_migrations"][0]["filename"],
        "20261001000000000000_create_users.sql"
    );

    fs::remove_file(
        directory
            .path()
            .join("migrations/versioned/20261001000000000000_create_users.sql"),
    )
    .expect("remove colliding test migration");
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20260901000000000000_create_earlier.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE local_earlier (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE local_earlier;\n",
    )
    .expect("write out-of-order native migration");
    let out_of_order = decode(
        &run_import(directory.path(), "plan", "knex"),
        "out-of-order migration plan",
    );
    assert!(
        out_of_order["blockers"]
            .as_array()
            .expect("ordering blockers")
            .iter()
            .any(|blocker| blocker["code"] == "NATIVE_MIGRATION_OUT_OF_ORDER")
    );
}

#[test]
fn import_apply_is_digest_gated_idempotent_and_preserves_only_metadata() {
    let directory = tempdir().expect("create import apply project");
    configure_project(directory.path());
    let database_path = directory.path().join("legacy.sqlite");
    create_legacy_fixtures(&database_path);

    let plan = decode(&run_import(directory.path(), "plan", "knex"), "Knex plan");
    let digest = plan["digest"].as_str().expect("approved plan digest");
    let missing_approval = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "apply",
            "--from",
            "knex",
            "--plan-digest",
            digest,
        ],
    );
    assert!(!missing_approval.status.success());

    let applied = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "apply",
            "--from",
            "knex",
            "--plan-digest",
            digest,
            "--yes",
        ],
    );
    let applied_json = decode(&applied, "Knex apply");
    assert_eq!(applied_json["status"], "applied");
    assert_eq!(applied_json["event_count"], 2);
    assert_eq!(applied_json["current_migration_count"], 2);
    let import_id = applied_json["import_id"]
        .as_str()
        .expect("import id")
        .to_owned();

    let repeated = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "apply",
            "--from",
            "knex",
            "--plan-digest",
            digest,
            "--yes",
        ],
    );
    let repeated_json = decode(&repeated, "idempotent Knex apply");
    assert_eq!(repeated_json["status"], "already_applied");
    assert_eq!(repeated_json["import_id"], import_id);

    let verified = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "verify",
            "--from",
            "knex",
            "--import-id",
            &import_id,
        ],
    );
    let verified_json = decode(&verified, "Knex verify");
    assert_eq!(verified_json["status"], "verified");
    assert_eq!(verified_json["plan_digest"], digest);

    let status = run(directory.path(), &["--format", "json", "status"]);
    let status_json = decode(&status, "status after legacy import");
    assert_eq!(status_json["status"], "legacy_adoption_required");
    assert_eq!(status_json["migrations"].as_array().map(Vec::len), Some(2));
    assert_eq!(status_json["migrations"][0]["state"], "legacy_unadopted");
    let doctor = run(directory.path(), &["--format", "json", "doctor"]);
    let doctor_json = decode(&doctor, "doctor after legacy import");
    assert_eq!(doctor_json["status"], "legacy_adoption_required");
    assert!(
        doctor_json["findings"]
            .as_array()
            .expect("doctor findings")
            .iter()
            .any(|finding| finding["code"] == "LEGACY_ROLLBACK_BLOCKED")
    );

    let down = run(directory.path(), &["--format", "json", "down"]);
    assert!(!down.status.success(), "legacy rollback must be blocked");
    assert_eq!(
        decode_any(&down, "blocked down")["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );

    let connection = Connection::open(&database_path).expect("inspect post-import database");
    let imported_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE kind = 'legacy'",
            [],
            |row| row.get(0),
        )
        .expect("count imported current migrations");
    assert_eq!(imported_rows, 2);
    let timeline_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_import_events",
            [],
            |row| row.get(0),
        )
        .expect("count preserved legacy events");
    assert_eq!(timeline_rows, 2);
    let legacy_sql_was_not_executed: bool = connection
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'create_users')",
            [],
            |row| row.get(0),
        )
        .expect("check no legacy migration SQL ran");
    assert!(legacy_sql_was_not_executed);
    let original_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM knex_migrations", [], |row| row.get(0))
        .expect("count source Knex history");
    assert_eq!(original_rows, 2, "the source history stays untouched");
}

#[test]
fn import_verify_detects_source_drift_and_adoption_requires_a_verified_native_migration() {
    let directory = tempdir().expect("create verification project");
    configure_project(directory.path());
    let database_path = directory.path().join("legacy.sqlite");
    create_legacy_fixtures(&database_path);
    fs::write(
        directory
            .path()
            .join("migrations/versioned/20261003000000000000_create_native.sql"),
        "-- dbm:dialect sqlite\n-- dbm:transaction auto\n-- dbm:up\nCREATE TABLE native_migration (id INTEGER PRIMARY KEY);\n-- dbm:down\nDROP TABLE native_migration;\n",
    )
    .expect("write reversible native migration");

    let plan = decode(&run_import(directory.path(), "plan", "goose"), "Goose plan");
    assert_eq!(plan["status"], "ready");
    let digest = plan["digest"].as_str().expect("approved plan digest");
    let applied = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "apply",
            "--from",
            "goose",
            "--plan-digest",
            digest,
            "--yes",
        ],
    );
    let applied_json = decode(&applied, "Goose apply");
    let import_id = applied_json["import_id"]
        .as_str()
        .expect("import id")
        .to_owned();
    assert_eq!(applied_json["event_count"], 4);
    assert_eq!(applied_json["current_migration_count"], 1);

    let before_native = run(directory.path(), &["--format", "json", "down"]);
    assert!(!before_native.status.success());
    assert_eq!(
        decode_any(&before_native, "blocked rollback before native apply")["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );

    let native_up = run(directory.path(), &["up"]);
    assert!(
        native_up.status.success(),
        "apply native migration: {}",
        String::from_utf8_lossy(&native_up.stderr)
    );
    let verified = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "verify",
            "--from",
            "goose",
            "--import-id",
            &import_id,
        ],
    );
    assert_eq!(decode(&verified, "Goose verify")["status"], "verified");

    let evidence_path = directory.path().join("adoption-evidence.txt");
    fs::write(
        &evidence_path,
        "independent schema equivalence review passed",
    )
    .expect("write bounded adoption evidence");
    let evidence_path = evidence_path.display().to_string();
    let adopted = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "adopt",
            "--from",
            "goose",
            "--import-id",
            &import_id,
            "--legacy-identity",
            "3",
            "--version",
            "20261003000000000000",
            "--evidence",
            &evidence_path,
            "--reason",
            "native migration is verified equivalent",
            "--actor",
            "reviewer",
            "--yes",
        ],
    );
    let adopted_json = decode(&adopted, "Goose adoption");
    assert_eq!(adopted_json["status"], "adopted");

    let rolled_back = run(directory.path(), &["--format", "json", "down"]);
    assert!(
        rolled_back.status.success(),
        "adopted native rollback: {}",
        String::from_utf8_lossy(&rolled_back.stdout)
    );
    let connection = Connection::open(&database_path).expect("inspect adopted metadata");
    let adoption_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_adoptions WHERE import_id = ?1",
            [&import_id],
            |row| row.get(0),
        )
        .expect("count adoption audit records");
    assert_eq!(adoption_count, 1);
    let native_table_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'native_migration')",
            [],
            |row| row.get(0),
        )
        .expect("inspect native down effect");
    assert!(!native_table_exists);
}

#[test]
fn import_apply_rejects_a_stale_digest_without_bootstrapping_metadata() {
    let directory = tempdir().expect("create stale plan project");
    configure_project(directory.path());
    let database_path = directory.path().join("legacy.sqlite");
    create_legacy_fixtures(&database_path);
    let plan = decode(&run_import(directory.path(), "plan", "knex"), "Knex plan");
    let digest = plan["digest"].as_str().expect("approved plan digest");
    let connection = Connection::open(&database_path).expect("open source history");
    connection
        .execute(
            "UPDATE knex_migrations SET batch = batch + 1 WHERE id = 1",
            [],
        )
        .expect("change source history after approval");
    drop(connection);

    let rejected = run(
        directory.path(),
        &[
            "--format",
            "json",
            "import",
            "apply",
            "--from",
            "knex",
            "--plan-digest",
            digest,
            "--yes",
        ],
    );
    assert!(!rejected.status.success());
    assert_eq!(
        decode_any(&rejected, "stale digest rejection")["errors"][0]["code"],
        "SAFETY_BLOCKED"
    );
    let connection = Connection::open(&database_path).expect("check rejected apply");
    let metadata_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'schema_migration_%'",
            [],
            |row| row.get(0),
        )
        .expect("count DBM metadata tables");
    assert_eq!(metadata_count, 0);
}
