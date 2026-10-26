use database_migrator::dsl::{
    CheckConstraint, CheckExpression, ColumnDefault, ColumnType, ComparisonOperator, ForeignKey,
    ForeignKeyAction, LiteralValue, SafetyClass, SchemaBuilder, SchemaDialect, SchemaMigration,
};

fn migration(table: &str, index: &str) -> database_migrator::Result<SchemaMigration> {
    let child = format!("{table}_children");
    let mut up = SchemaBuilder::new();
    up.create_table(table, |definition| {
        definition
            .column("id", ColumnType::BigInt)?
            .primary_key()
            .not_null();
        definition
            .column("slug", ColumnType::Varchar(96))?
            .not_null()
            .unique();
        definition
            .column("enabled", ColumnType::Boolean)?
            .not_null()
            .default(ColumnDefault::Literal(LiteralValue::Boolean(true)))?;
        definition.check(CheckConstraint::new(
            "dsl_positive_id",
            CheckExpression::column("id")?.compare(
                ComparisonOperator::GreaterThan,
                CheckExpression::value(LiteralValue::Integer(0))?,
            )?,
        )?)?;
        Ok(())
    })?;
    up.create_index(index, table, &["slug"], true)?;
    up.create_table(&child, |definition| {
        definition.column("id", ColumnType::BigInt)?.primary_key();
        definition
            .column("parent_id", ColumnType::BigInt)?
            .not_null();
        definition.foreign_key(
            ForeignKey::new(&["parent_id"], table, &["id"])?.on_delete(ForeignKeyAction::Cascade),
        )?;
        Ok(())
    })?;

    let mut down = SchemaBuilder::new();
    down.drop_table(&child, false)?;
    down.drop_index(index, table)?;
    down.drop_table(table, false)?;
    SchemaMigration::new(up.finish()?, down.finish()?)
}

fn unique_names() -> (String, String) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    (format!("dbm_dsl_{suffix}"), format!("dbm_idx_{suffix}"))
}

#[test]
fn sqlite_schema_plan_executes_up_and_explicit_down() {
    let (table, index) = unique_names();
    let child = format!("{table}_children");
    let migration = migration(&table, &index).expect("build explicit DSL migration");
    let up = migration
        .up()
        .compile(SchemaDialect::Sqlite)
        .expect("compile SQLite Up");
    let down = migration
        .down()
        .compile(SchemaDialect::Sqlite)
        .expect("compile SQLite Down");
    assert!(
        up.statements()
            .iter()
            .all(|statement| statement.safety_class() == SafetyClass::Transactional)
    );

    let connection = rusqlite::Connection::open_in_memory().expect("open in-memory SQLite");
    let result = (|| -> rusqlite::Result<()> {
        connection.execute_batch("PRAGMA foreign_keys = ON")?;
        for statement in up.statements() {
            connection.execute(statement.sql(), [])?;
        }
        connection.execute(
            &format!(
                "INSERT INTO \"{table}\" (\"id\", \"slug\", \"enabled\") VALUES (1, 'one', TRUE)"
            ),
            [],
        )?;
        let count: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM \"{table}\" WHERE \"enabled\" = 1"),
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        let invalid_foreign_key = connection.execute(
            &format!("INSERT INTO \"{child}\" (\"id\", \"parent_id\") VALUES (1, 404)"),
            [],
        );
        assert!(invalid_foreign_key.is_err(), "foreign key must be enforced");
        connection.execute(
            &format!("INSERT INTO \"{child}\" (\"id\", \"parent_id\") VALUES (1, 1)"),
            [],
        )?;
        let duplicate_unique = connection.execute(
            &format!("INSERT INTO \"{table}\" (\"id\", \"slug\") VALUES (2, 'one')"),
            [],
        );
        assert!(
            duplicate_unique.is_err(),
            "unique constraint must be enforced"
        );
        let invalid_check = connection.execute(
            &format!("INSERT INTO \"{table}\" (\"id\", \"slug\") VALUES (0, 'zero')"),
            [],
        );
        assert!(invalid_check.is_err(), "CHECK constraint must be enforced");
        for statement in down.statements() {
            connection.execute(statement.sql(), [])?;
        }
        let remaining: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN (?1, ?2)",
            rusqlite::params![table, child],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0);
        Ok(())
    })();
    result.expect("execute SQLite schema migration and explicit Down");
}

#[test]
#[ignore = "requires DBM_TEST_DATABASE_URL for a disposable loopback PostgreSQL instance"]
fn postgres_schema_plan_executes_up_and_explicit_down() {
    run_postgres_schema_plan();
}

fn run_postgres_schema_plan() {
    let connection_url = std::env::var("DBM_TEST_DATABASE_URL")
        .expect("DBM_TEST_DATABASE_URL must identify a disposable PostgreSQL database");
    let (table, index) = unique_names();
    let child = format!("{table}_children");
    let migration = migration(&table, &index).expect("build explicit DSL migration");
    let up = migration
        .up()
        .compile(SchemaDialect::Postgres)
        .expect("compile PostgreSQL Up");
    let down = migration
        .down()
        .compile(SchemaDialect::Postgres)
        .expect("compile PostgreSQL Down");
    let runtime = tokio::runtime::Runtime::new().expect("create PostgreSQL test runtime");

    let result = runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(
            &connection_url,
            tokio_postgres::NoTls,
        )
        .await
        .expect("connect to disposable PostgreSQL");
        let connection_task = tokio::spawn(connection);
        let result = async {
            for statement in up.statements() {
                client
                    .batch_execute(statement.sql())
                    .await
                    .map_err(|_| "execute PostgreSQL DSL Up statement".to_owned())?;
            }
            client
                .batch_execute(&format!(
                    "INSERT INTO \"{table}\" (\"id\", \"slug\", \"enabled\") VALUES (1, 'one', TRUE)"
                ))
                .await
                .map_err(|_| "insert PostgreSQL DSL test row".to_owned())?;
            if client
                .batch_execute(&format!(
                    "INSERT INTO \"{child}\" (\"id\", \"parent_id\") VALUES (1, 404)"
                ))
                .await
                .is_ok()
            {
                return Err("PostgreSQL did not enforce the DSL foreign key".to_owned());
            }
            client
                .batch_execute(&format!(
                    "INSERT INTO \"{child}\" (\"id\", \"parent_id\") VALUES (1, 1)"
                ))
                .await
                .map_err(|_| "insert PostgreSQL DSL child row".to_owned())?;
            if client
                .batch_execute(&format!(
                    "INSERT INTO \"{table}\" (\"id\", \"slug\") VALUES (2, 'one')"
                ))
                .await
                .is_ok()
            {
                return Err("PostgreSQL did not enforce the DSL unique constraint".to_owned());
            }
            if client
                .batch_execute(&format!(
                    "INSERT INTO \"{table}\" (\"id\", \"slug\") VALUES (0, 'zero')"
                ))
                .await
                .is_ok()
            {
                return Err("PostgreSQL did not enforce the DSL CHECK constraint".to_owned());
            }
            let count: i64 = client
                .query_one(&format!("SELECT COUNT(*) FROM \"{table}\""), &[])
                .await
                .map_err(|_| "query PostgreSQL DSL test row".to_owned())?
                .get(0);
            if count != 1 {
                return Err(format!("expected one PostgreSQL DSL row, got {count}"));
            }
            for statement in down.statements() {
                client
                    .batch_execute(statement.sql())
                    .await
                    .map_err(|_| "execute PostgreSQL DSL Down statement".to_owned())?;
            }
            Ok::<(), String>(())
        }
        .await;
        let cleanup = client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS \"{child}\" CASCADE; DROP TABLE IF EXISTS \"{table}\" CASCADE"
            ))
            .await
            .map_err(|_| "clean PostgreSQL DSL test table".to_owned());
        drop(client);
        connection_task.abort();
        let _ = connection_task.await;
        match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    });
    result.expect("execute PostgreSQL schema migration and explicit Down");
}

#[test]
#[ignore = "requires DBM_TEST_MYSQL_URL for a disposable loopback MySQL instance"]
fn mysql_schema_plan_executes_up_and_explicit_down() {
    run_mysql_schema_plan("DBM_TEST_MYSQL_URL", SchemaDialect::Mysql);
}

#[test]
#[ignore = "requires DBM_TEST_MARIADB_URL for a disposable loopback MariaDB instance"]
fn mariadb_schema_plan_executes_up_and_explicit_down() {
    run_mysql_schema_plan("DBM_TEST_MARIADB_URL", SchemaDialect::MariaDb);
}

fn run_mysql_schema_plan(environment_variable: &str, dialect: SchemaDialect) {
    use mysql_async::prelude::Queryable;

    let connection_url = std::env::var(environment_variable).unwrap_or_else(|_| {
        panic!("{environment_variable} must identify a disposable MySQL server")
    });
    let (table, index) = unique_names();
    let child = format!("{table}_children");
    let migration = migration(&table, &index).expect("build explicit DSL migration");
    let up = migration
        .up()
        .compile(dialect)
        .expect("compile MySQL-family Up");
    let down = migration
        .down()
        .compile(dialect)
        .expect("compile MySQL-family Down");
    assert!(
        up.statements()
            .iter()
            .all(|statement| statement.safety_class() == SafetyClass::NonTransactional)
    );
    let options =
        mysql_async::Opts::from_url(&connection_url).expect("MySQL-family test URL is valid");
    let options: mysql_async::Opts = mysql_async::OptsBuilder::from_opts(options)
        .prefer_socket(Some(false))
        .into();
    let runtime = tokio::runtime::Runtime::new().expect("create MySQL-family test runtime");

    let result = runtime.block_on(async {
        let mut connection = mysql_async::Conn::new(options)
            .await
            .expect("connect to disposable MySQL-family target");
        let result = async {
            for statement in up.statements() {
                connection
                    .query_drop(statement.sql())
                    .await
                    .map_err(|_| "execute MySQL-family DSL Up statement".to_owned())?;
            }
            connection
                .query_drop(format!(
                    "INSERT INTO `{table}` (`id`, `slug`, `enabled`) VALUES (1, 'one', TRUE)"
                ))
                .await
                .map_err(|_| "insert MySQL-family DSL test row".to_owned())?;
            if connection
                .query_drop(format!(
                    "INSERT INTO `{child}` (`id`, `parent_id`) VALUES (1, 404)"
                ))
                .await
                .is_ok()
            {
                return Err("MySQL-family server did not enforce the DSL foreign key".to_owned());
            }
            connection
                .query_drop(format!(
                    "INSERT INTO `{child}` (`id`, `parent_id`) VALUES (1, 1)"
                ))
                .await
                .map_err(|_| "insert MySQL-family DSL child row".to_owned())?;
            if connection
                .query_drop(format!(
                    "INSERT INTO `{table}` (`id`, `slug`) VALUES (2, 'one')"
                ))
                .await
                .is_ok()
            {
                return Err(
                    "MySQL-family server did not enforce the DSL unique constraint".to_owned(),
                );
            }
            if connection
                .query_drop(format!(
                    "INSERT INTO `{table}` (`id`, `slug`) VALUES (0, 'zero')"
                ))
                .await
                .is_ok()
            {
                return Err(
                    "MySQL-family server did not enforce the DSL CHECK constraint".to_owned(),
                );
            }
            let count: Option<(i64,)> = connection
                .query_first(format!("SELECT COUNT(*) FROM `{table}`"))
                .await
                .map_err(|_| "query MySQL-family DSL test row".to_owned())?;
            if count != Some((1,)) {
                return Err(format!("expected one MySQL-family DSL row, got {count:?}"));
            }
            for statement in down.statements() {
                connection
                    .query_drop(statement.sql())
                    .await
                    .map_err(|_| "execute MySQL-family DSL Down statement".to_owned())?;
            }
            Ok::<(), String>(())
        }
        .await;
        let cleanup = connection
            .query_drop(format!(
                "DROP TABLE IF EXISTS `{child}`; DROP TABLE IF EXISTS `{table}`"
            ))
            .await
            .map_err(|_| "clean MySQL-family DSL test table".to_owned());
        let disconnect = connection
            .disconnect()
            .await
            .map_err(|_| "disconnect MySQL-family test connection".to_owned());
        match (result, cleanup, disconnect) {
            (Err(error), _, _) => Err(error),
            (Ok(()), Err(error), _) => Err(error),
            (Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(()), Ok(())) => Ok(()),
        }
    });
    result.expect("execute MySQL-family schema migration and explicit Down");
}
