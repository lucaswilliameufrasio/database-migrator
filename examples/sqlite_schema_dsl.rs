use std::error::Error;

use database_migrator::dsl::{ColumnType, SchemaBuilder, SchemaDialect, SchemaMigration};

fn main() -> Result<(), Box<dyn Error>> {
    let mut up = SchemaBuilder::new();
    up.create_table("accounts", |table| {
        table.column("id", ColumnType::Integer)?.primary_key();
        table
            .column("email", ColumnType::Varchar(255))?
            .not_null()
            .unique();
        Ok(())
    })?;

    let mut down = SchemaBuilder::new();
    down.drop_table("accounts", false)?;
    let migration = SchemaMigration::new(up.finish()?, down.finish()?)?;
    let plan = migration.up().compile(SchemaDialect::Sqlite)?;

    println!("SQL fingerprint: {}", plan.sql_fingerprint());
    for statement in plan.statements() {
        println!("{}", statement.sql());
    }
    Ok(())
}
