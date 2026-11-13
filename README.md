# Database Migrator

`dbm` applies versioned SQL migrations to SQLite, PostgreSQL, MySQL, and MariaDB.
The Rust API and the metadata format are still provisional. There is no release.

Versions already exercised are in [docs/support-matrix.md](docs/support-matrix.md).
Interrupted runs are in [docs/recovery-drills.md](docs/recovery-drills.md).
Upgrades are in [docs/upgrade-guide.md](docs/upgrade-guide.md).

## Versions

| Database | Broad suite | Recovery drills only |
|---|---|---|
| SQLite | Bundled with the build | — |
| PostgreSQL | 16.15, 17.11, 18.6 | — |
| MySQL | 8.4.11, 8.4.13 | 9.7.4 |
| MariaDB | 10.11.14, 11.4.13, 11.8.2, 11.8.9 | 13.0.2 |

PostgreSQL 18.6 is the line selected for the open G11 check. MySQL 8.4 is the
current LTS line with the broad suite. MariaDB 11.8 is the newest line with
that same suite. MariaDB 12.3 has not been run.

## Quick start

Requires Rust 1.89 or newer. The repository toolchain is 1.99.1. SQLite is
bundled; no separate SQLite install is required.

```sh
cargo run -- init
cargo run -- new create_users
cargo run -- plan
cargo run -- up
cargo run -- down --steps 1
```

`cargo install --path .` installs the `dbm` binary. `dbm init` writes
`dbm.toml` and `migrations/versioned/` and does not overwrite an existing
config. The default database is `dbm.sqlite` in `development`. Add
`--format json` for one JSON document on stdout.

## Migration file

`dbm new` writes `<20-digit UTC timestamp>_<description>.sql`.

```sql
-- dbm:dialect sqlite
-- dbm:transaction auto
-- dbm:up
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE
);

-- dbm:down
DROP TABLE users;
```

Editing, moving, renaming, or deleting an applied file is divergence.
`--migration-format sql-pair` writes `.up.sql` and `.down.sql`. MySQL and
MariaDB accept only the single-file format, with `-- dbm:transaction never`.
Repeatable files are `R__<name>.sql`. They run after versioned migrations and
`down` never executes them.

## Targets

SQLite migrations are transactional. `plan`, `status`, `history`, and
`validate` do not create the database file.

PostgreSQL uses `-- dbm:dialect postgres`, TLS, and metadata format 5 before
any write. Plaintext is allowed only in `development` or `test`, and only with
all three of `sslmode=disable`, `allow_insecure_transport = true`, and a
numeric loopback address or an explicit loopback `hostaddr`. One migration may
contain a single `CREATE [UNIQUE] INDEX CONCURRENTLY`. `U&"..."` identifiers
are rejected; write `"café"` instead.

```toml
[target]
driver = "postgres"
url_env = "DATABASE_URL"
environment = "test"
allow_insecure_transport = true
```

MySQL and MariaDB use `driver = "mysql"` and `-- dbm:dialect mysql`. Every
statement is journaled on its own because DDL can commit implicitly. Plaintext
uses the same development/test loopback opt-in. A hostname also needs pinned
loopback `resolved_ips`. Production rejects plaintext.

```sql
-- dbm:dialect mysql
-- dbm:transaction never
-- dbm:up
CREATE TABLE users (id BIGINT PRIMARY KEY);
-- dbm:down
DROP TABLE users;
```

```toml
[target]
driver = "mysql"
url_env = "DATABASE_URL"
environment = "test"
allow_insecure_transport = true
```

## Commands

| Command | Effect |
|---|---|
| `plan` | Read-only. Prints `Plan <hash>` and `Target <fingerprint>`. |
| `plan --down --steps N` | Prints the rollback hash for that step count. |
| `up` / `down` | Apply or roll back. Development and test need no extra flags. |
| `status`, `history`, `doctor` | Read-only. |
| `doctor reconcile` | Records an operator decision. It does not execute SQL. |
| `metadata upgrade --yes` | Moves old metadata to format 5. Nothing upgrades automatically. |
| `import inspect\|plan\|apply\|verify\|adopt` | Reads Knex, Goose, or SQLx history. `apply` does not run the old SQL. |

In `production`, `up` and `down` also require both flags, copied from the
matching read-only plan:

```sh
dbm up --approve-plan sha256:PLAN --approve-target sha256:TARGET
dbm down --steps 1 --approve-plan sha256:DOWN_PLAN --approve-target sha256:TARGET
```

The up hash does not authorize down. Reconcile, metadata upgrade, and import
apply/adopt keep their own `--yes` or digest checks and also require
`--approve-target`. Development and test reject these flags.

`doctor reconcile` needs `--run-id`, `--result`, `--evidence`, `--repair-plan`,
`--reason`, `--actor`, and `--approve`. The repair plan is JSON schema 1.0
with one entry per statement: `step_index`, `statement_hash`, `state`
(`confirmed_applied`, `confirmed_not_applied`, or `indeterminate`), and
`inspection`. An indeterminate step blocks reconciliation.

## Library, runner, and AI

`database_migrator::dsl` and `MigrationRegistry` are experimental. See
`examples/sqlite_schema_dsl.rs` and `examples/sqlite_embedded_migration.rs`.
Importing the library does not migrate anything.

A compiled runner is a project binary that calls `runner::serve`. Set
`execution.runner` to that file. The runner is trusted project code, not a
sandbox. `doctor reconcile` is unavailable in runner mode.

AI is off unless the crate is built with `--features ai` and `[ai].enabled`
is true. It proposes SQL and has no `--apply`. A real call also needs
`DBM_AI_APPROVED_ENDPOINT` equal to the profile endpoint. Schemas are in
[docs/schemas](docs/schemas/README.md).

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --locked
cargo run --example sqlite_embedded_migration --locked
cargo run --example sqlite_schema_dsl --locked
```

The default tests use temporary SQLite files and do not need Docker. Opt-in
server tests read `DBM_TEST_DATABASE_URL`, `DBM_TEST_MYSQL_URL`, and
`DBM_TEST_MARIADB_URL`. TLS tests add `DBM_TEST_POSTGRES_CA_FILE`,
`DBM_TEST_MYSQL_CA_FILE`, and `DBM_TEST_MARIADB_CA_FILE`. Point them at a
disposable loopback database.

## License

[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
