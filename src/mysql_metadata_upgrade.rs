use database_migrator_core::config::{
    Config, Environment, ExecutionConfig, LoadedConfig, ProjectDirectory, TargetConfig,
};
use database_migrator_core::database::{DatabaseAdapter, SessionIntent};
use database_migrator_mysql::{
    MySqlAdapter, is_local_mysql_host, normalized_check_clause, prepare_connection_options,
};
use mysql_async::Conn;

fn test_config(
    environment: Environment,
    allow_insecure_transport: bool,
    tls_ca_cert_env: Option<&str>,
) -> LoadedConfig {
    LoadedConfig {
        config: Config {
            version: 1,
            namespace: "default".to_owned(),
            source: vec!["migrations".into()],
            target: TargetConfig {
                driver: "mysql".to_owned(),
                database: None,
                url_env: Some("DBM_TEST_MYSQL_URL".to_owned()),
                tls_ca_cert_env: tls_ca_cert_env.map(str::to_owned),
                allow_insecure_transport,
                environment,
            },
            execution: ExecutionConfig::default(),
            ai: database_migrator_core::config::AiConfig::default(),
        },
        source_dirs: Vec::new(),
        database_path: None,
        project_directory: ProjectDirectory::open_ambient(
            &std::env::current_dir().expect("current test directory"),
        )
        .expect("open test project directory"),
    }
}

async fn check_constraint_name(
    connection: &mut Conn,
    table_name: &str,
    expected_clause: &str,
) -> String {
    use mysql_async::prelude::Queryable;

    assert!(
        table_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    let query = format!(
        "SELECT table_constraints.CONSTRAINT_NAME, check_constraints.CHECK_CLAUSE \
             FROM information_schema.TABLE_CONSTRAINTS AS table_constraints \
             INNER JOIN information_schema.CHECK_CONSTRAINTS AS check_constraints \
                ON check_constraints.CONSTRAINT_SCHEMA = table_constraints.CONSTRAINT_SCHEMA \
               AND check_constraints.CONSTRAINT_NAME = table_constraints.CONSTRAINT_NAME \
             WHERE table_constraints.CONSTRAINT_SCHEMA = DATABASE() \
               AND table_constraints.TABLE_NAME = '{table_name}' \
               AND table_constraints.CONSTRAINT_TYPE = 'CHECK'"
    );
    let rows: Vec<(String, String)> = connection
        .query(query)
        .await
        .expect("inspect metadata check constraints");
    let mut matches = Vec::new();
    for (name, clause) in rows {
        if normalized_check_clause(&clause) == expected_clause {
            matches.push(name);
        }
    }
    assert_eq!(matches.len(), 1, "expected one {expected_clause} check");
    let name = matches.remove(0);
    assert!(
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    name
}

#[tokio::test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL/MariaDB database"]
async fn local_mysql_metadata_v3_and_v4_upgrades_preserve_native_rows_and_run_journal() {
    use mysql_async::prelude::Queryable;

    let config = test_config(Environment::Test, true, None);
    let connection_string = std::env::var("DBM_TEST_MYSQL_URL")
        .expect("DBM_TEST_MYSQL_URL must identify a disposable local database");
    let options = prepare_connection_options(&connection_string, &config)
        .await
        .expect("prepare local MySQL/MariaDB integration connection");
    assert!(
        is_local_mysql_host(&options),
        "integration target must be loopback"
    );
    let mut connection = mysql_async::Conn::new(options)
        .await
        .expect("connect to disposable local MySQL/MariaDB database");
    let reserved_tables: u64 = connection
        .query_first(
            "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name IN (\
                    'schema_migration_meta', 'schema_migrations', \
                    'schema_migration_runs', 'schema_migration_steps', \
                    'schema_migration_locks', 'schema_migration_imports', \
                    'schema_migration_import_events', 'schema_migration_adoptions')",
        )
        .await
        .expect("inspect test target metadata tables")
        .expect("metadata table count row");
    assert_eq!(
        reserved_tables, 0,
        "test target must start without DBM metadata"
    );

    let adapter = MySqlAdapter;
    let mut session = adapter
        .connect(&config, SessionIntent::ReadWrite)
        .await
        .expect("connect DBM MySQL/MariaDB adapter");
    assert!(
        !session
            .read_snapshot("default")
            .await
            .expect("read empty test target")
            .initialized
    );
    session
        .acquire_lock(std::time::Duration::from_secs(2))
        .await
        .expect("acquire MySQL/MariaDB named lock");
    session.begin().await.expect("begin metadata bootstrap");
    session
        .initialize_metadata("2026-10-24T12:00:00.000000Z")
        .await
        .expect("bootstrap current metadata");
    session.commit().await.expect("commit metadata bootstrap");
    session
        .release_lock()
        .await
        .expect("release bootstrap session lock");
    session.close().await.expect("close bootstrap session");

    connection
        .query_drop(
            "INSERT INTO schema_migrations \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 VALUES ('default', REPEAT('a', 64), 'versioned', \
                         '20261024120000000000', '20261024120000000000_probe.sql', \
                         'migrations/versioned/20261024120000000000_probe.sql', \
                         'probe', 'sql', 'sha256:fixture', 1, CURRENT_TIMESTAMP(6))",
        )
        .await
        .expect("seed native migration row");
    connection
        .query_drop(
            "INSERT INTO schema_migration_runs \
                 (run_id, batch_id, namespace, migration_key, migration_version, direction, \
                  state, attempt, created_at, checksum_at_execution, plan_hash, actor_kind) \
                 VALUES ('upgrade-run', 'upgrade-batch', 'default', REPEAT('a', 64), \
                         '20261024120000000000', 'up', 'succeeded', 1, CURRENT_TIMESTAMP(6), \
                         'sha256:fixture', 'sha256:plan', 'human')",
        )
        .await
        .expect("seed migration run");
    connection
        .query_drop(
            "INSERT INTO schema_migration_steps \
                 (run_id, step_index, step_key, direction, statement_hash, statement_kind, \
                  transaction_class, state, started_at, attempt_id) \
                 VALUES ('upgrade-run', 0, REPEAT('b', 64), 'up', 'sha256:step', 'CREATE', \
                         'transactional', 'acknowledged', CURRENT_TIMESTAMP(6), 'upgrade-attempt')",
        )
        .await
        .expect("seed migration journal step");

    connection
        .query_drop("DROP TABLE schema_migration_adoptions")
        .await
        .expect("drop v4 adoption table in isolated fixture");
    let direction_constraint = check_constraint_name(
        &mut connection,
        "schema_migration_runs",
        "directioninupdownrepeatreconcile",
    )
    .await;
    connection
            .query_drop(
                format!(
                    "ALTER TABLE schema_migration_runs \
                        DROP CONSTRAINT `{direction_constraint}`, \
                        MODIFY COLUMN migration_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
                        ADD CONSTRAINT schema_migration_runs_direction_check \
                            CHECK (direction IN ('up', 'down', 'reconcile'))"
                ),
            )
            .await
            .expect("restore the v3 run direction and version contract");
    connection
        .query_drop("DROP TABLE schema_migration_import_events")
        .await
        .expect("drop v4 event table in isolated fixture");
    connection
            .query_drop(
                "CREATE TABLE schema_migrations_v3 (\
                    namespace VARCHAR(128) NOT NULL,\
                    migration_key CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    kind VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    native_version CHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    filename LONGTEXT NOT NULL,\
                    source_path LONGTEXT NOT NULL,\
                    description LONGTEXT NOT NULL,\
                    authoring_type VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    content_checksum VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    apply_order BIGINT NOT NULL,\
                    applied_at DATETIME(6) NOT NULL,\
                    PRIMARY KEY (namespace, migration_key),\
                    UNIQUE KEY schema_migrations_namespace_version_uq (namespace, kind, native_version),\
                    CONSTRAINT schema_migrations_v3_kind_check CHECK (kind = 'versioned'),\
                    CONSTRAINT schema_migrations_v3_authoring_check CHECK (authoring_type = 'sql')\
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
            )
            .await
            .expect("create v3 migration table fixture");
    connection
        .query_drop(
            "INSERT INTO schema_migrations_v3 \
                 (namespace, migration_key, kind, native_version, filename, source_path, \
                  description, authoring_type, content_checksum, apply_order, applied_at) \
                 SELECT namespace, migration_key, kind, native_version, filename, source_path, \
                        description, authoring_type, content_checksum, apply_order, applied_at \
                 FROM schema_migrations WHERE kind = 'versioned'",
        )
        .await
        .expect("copy native state into v3 fixture");
    connection
        .query_drop("DROP TABLE schema_migrations")
        .await
        .expect("replace v4 migration table with v3 fixture");
    connection
        .query_drop("RENAME TABLE schema_migrations_v3 TO schema_migrations")
        .await
        .expect("install v3 migration table fixture");
    connection
        .query_drop("DROP TABLE schema_migration_imports")
        .await
        .expect("drop v4 import table in isolated fixture");
    connection
            .query_drop(
                "CREATE TABLE schema_migration_meta_v3 (\
                    singleton TINYINT UNSIGNED NOT NULL PRIMARY KEY CHECK (singleton = 1),\
                    metadata_format_version INT NOT NULL,\
                    installation_id CHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,\
                    initialized_at DATETIME(6) NOT NULL,\
                    upgraded_at DATETIME(6) NULL,\
                    CONSTRAINT schema_migration_meta_v3_format_check CHECK (metadata_format_version = 3)\
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_bin",
            )
            .await
            .expect("create v3 metadata identity fixture");
    connection
            .query_drop(
                "INSERT INTO schema_migration_meta_v3 \
                 (singleton, metadata_format_version, installation_id, initialized_at, upgraded_at) \
                 SELECT singleton, 3, installation_id, initialized_at, upgraded_at \
                 FROM schema_migration_meta",
            )
            .await
            .expect("copy metadata identity into v3 fixture");
    connection
        .query_drop("DROP TABLE schema_migration_meta")
        .await
        .expect("replace v4 metadata identity with v3 fixture");
    connection
        .query_drop("RENAME TABLE schema_migration_meta_v3 TO schema_migration_meta")
        .await
        .expect("install v3 metadata identity fixture");

    let mut before_session = adapter
        .connect(&config, SessionIntent::ReadOnly)
        .await
        .expect("connect to inspect v3 metadata");
    let before = before_session
        .read_snapshot("default")
        .await
        .expect("read v3 metadata fixture");
    assert_eq!(before.metadata_format_version, Some(3));
    assert_eq!(before.applied.len(), 1);
    assert_eq!(before.runs.len(), 1);
    assert_eq!(before.steps.len(), 1);
    before_session
        .close()
        .await
        .expect("close pre-upgrade inspection session");

    let preview = crate::engine::upgrade_metadata_async(&config, false, None)
        .await
        .expect("preview explicit MySQL/MariaDB metadata upgrade");
    assert_eq!(preview.status, "requires_approval");
    assert!(preview.read_only);
    assert_eq!(preview.from_version, Some(3));

    let report = crate::engine::upgrade_metadata_async(&config, true, None)
        .await
        .expect("upgrade MySQL/MariaDB metadata through the engine");
    assert_eq!(report.status, "upgraded");
    assert_eq!(report.runs_preserved, 1);
    let mut session = adapter
        .connect(&config, SessionIntent::ReadOnly)
        .await
        .expect("connect to inspect upgraded metadata");
    let after = session
        .read_snapshot("default")
        .await
        .expect("read upgraded metadata");
    assert_eq!(
        after.metadata_format_version,
        Some(database_migrator_core::storage::METADATA_FORMAT_VERSION)
    );
    assert_eq!(after.applied.len(), 1);
    assert_eq!(after.runs.len(), 1);
    assert_eq!(after.steps.len(), 1);
    assert_eq!(after.applied[0].version, "20261024120000000000");
    assert_eq!(after.runs[0].run_id, "upgrade-run");
    session.close().await.expect("close post-upgrade session");

    let kind_check = check_constraint_name(
        &mut connection,
        "schema_migrations",
        "kindinversionedrepeatablelegacy",
    )
    .await;
    let shape_check = check_constraint_name(
            &mut connection,
            "schema_migrations",
            "kindversionedandnativeversionisnotnullandrepeatablenameisnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindrepeatableandnativeversionisnullandrepeatablenameisnotnullandoriginalversionisnullandlegacyimportidisnullandorigintoolisnullorkindlegacyandnativeversionisnullandrepeatablenameisnullandoriginalversionisnotnullandlegacyimportidisnotnullandorigintoolisnotnullandorigindatabaseidentityisnotnullandoriginidentityisnotnullandverificationstatusisnotnullandobservedatisnotnull",
        )
        .await;
    let repeatable_name_check = check_constraint_name(
        &mut connection,
        "schema_migrations",
        "repeatablenameisnullorcharlengthrepeatablenamebetween1and128",
    )
    .await;
    connection
        .query_drop(format!(
            "ALTER TABLE schema_migrations \
                    DROP CONSTRAINT `{kind_check}`, \
                    DROP CONSTRAINT `{shape_check}`, \
                    DROP CONSTRAINT `{repeatable_name_check}`, \
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

    let direction_check = check_constraint_name(
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

    let metadata_format_check = check_constraint_name(
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
        .expect("set the v4 metadata identity");
    connection
        .query_drop(
            "ALTER TABLE schema_migration_meta \
                    ADD CONSTRAINT schema_migration_meta_format_check \
                    CHECK (metadata_format_version = 4)",
        )
        .await
        .expect("restore the v4 metadata format check");

    let preview_v4 = crate::engine::upgrade_metadata_async(&config, false, None)
        .await
        .expect("preview explicit v4-to-v5 metadata upgrade");
    assert_eq!(preview_v4.status, "requires_approval");
    assert!(preview_v4.read_only);
    assert_eq!(preview_v4.from_version, Some(4));
    let report_v4 = crate::engine::upgrade_metadata_async(&config, true, None)
        .await
        .expect("upgrade format 4 metadata through the engine");
    assert_eq!(report_v4.status, "upgraded");
    assert_eq!(report_v4.from_version, Some(4));
    assert_eq!(report_v4.runs_preserved, 1);

    let mut session_v4 = adapter
        .connect(&config, SessionIntent::ReadOnly)
        .await
        .expect("connect to inspect direct v4 upgrade");
    let after_v4 = session_v4
        .read_snapshot("default")
        .await
        .expect("read direct v4 upgrade result");
    assert_eq!(
        after_v4.metadata_format_version,
        Some(database_migrator_core::storage::METADATA_FORMAT_VERSION)
    );
    assert_eq!(after_v4.applied.len(), 1);
    assert_eq!(after_v4.runs.len(), 1);
    assert_eq!(after_v4.steps.len(), 1);
    assert_eq!(after_v4.applied[0].version, "20261024120000000000");
    assert_eq!(after_v4.runs[0].run_id, "upgrade-run");
    session_v4
        .close()
        .await
        .expect("close direct v4 inspection");

    for cleanup in [
        "DROP TABLE IF EXISTS schema_migration_steps",
        "DROP TABLE IF EXISTS schema_migration_locks",
        "DROP TABLE IF EXISTS schema_migrations",
        "DROP TABLE IF EXISTS schema_migration_adoptions",
        "DROP TABLE IF EXISTS schema_migration_import_events",
        "DROP TABLE IF EXISTS schema_migration_imports",
        "DROP TABLE IF EXISTS schema_migration_runs",
        "DROP TABLE IF EXISTS schema_migration_meta",
    ] {
        connection
            .query_drop(cleanup)
            .await
            .expect("remove only the DBM tables created by this isolated test");
    }
    connection
        .disconnect()
        .await
        .expect("close disposable MySQL/MariaDB connection");
}
