use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const VERSION_ONE: &str = "20261116000000000001";
const VERSION_TWO: &str = "20261116000000000002";

fn project() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/local-ledger")
}

fn up_section(migration: &str) -> &str {
    let after = migration.split_once("-- dbm:up\n").expect("up marker").1;
    after.split_once("-- dbm:down\n").expect("down marker").0
}

#[test]
fn local_ledger_chain_matches_the_phone_sql_and_skips_the_configured_database() {
    let root = project();
    let first = fs::read_to_string(
        root.join("migrations/versioned")
            .join(format!("{VERSION_ONE}_create_expense.sql")),
    )
    .expect("first migration");
    let second = fs::read_to_string(
        root.join("migrations/versioned")
            .join(format!("{VERSION_TWO}_add_category.sql")),
    )
    .expect("second migration");
    let phone_one = fs::read_to_string(root.join("phone").join(format!("{VERSION_ONE}.sql")))
        .expect("first phone sql");
    let phone_two = fs::read_to_string(root.join("phone").join(format!("{VERSION_TWO}.sql")))
        .expect("second phone sql");
    assert_eq!(up_section(&first), phone_one);
    assert_eq!(up_section(&second), phone_two);
    assert!(phone_one.contains("CREATE TABLE expense"));
    assert!(phone_two.contains("ADD COLUMN category"));
    assert!(!phone_one.contains("-- dbm:"));
    assert!(!phone_two.contains("-- dbm:"));

    let output = Command::new(env!("CARGO_BIN_EXE_dbm"))
        .current_dir(&root)
        .args(["--format", "json", "chain"])
        .output()
        .expect("dbm process starts");
    assert!(
        output.status.success(),
        "chain: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("json stdout");
    assert_eq!(report["command"], "chain");
    assert_eq!(report["status"], "ok");
    assert_eq!(
        report["fresh"]["applied"],
        serde_json::json!([VERSION_ONE, VERSION_TWO])
    );
    assert_eq!(report["published"][0]["name"], "ledger-1");
    assert_eq!(report["published"][0]["head"], VERSION_ONE);
    assert_eq!(report["published"][1]["name"], "ledger-head");
    assert_eq!(report["published"][1]["head"], VERSION_TWO);
    assert!(!root.join("ledger.sqlite").exists());
    assert!(!root.join("ledger.sqlite.dbm-owner").exists());
}
