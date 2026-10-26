# Database Migrator

`database-migrator` is a Rust library and `dbm` command-line tool for versioned
SQL migrations. SQL execution currently supports **SQLite, PostgreSQL, MySQL,
and MariaDB**, including per-statement journal records, `doctor`, and explicit
reconciliation. The experimental Rust schema DSL, embedded SQL registry, and
compiled runner can now compose filesystem SQL, DSL, and Rust-generated SQL in
one plan; these APIs are provisional and have no compatibility guarantee.
PostgreSQL `up`/`down` supports
transactional migrations and a restricted concurrent-index operation on
metadata format 5.
MySQL 8.4 and MariaDB 10.11, 11.4, and 11.8 have passed the current opt-in
integration matrix; these servers execute only explicitly non-transactional
(`transaction = never`) SQL migrations. PostgreSQL 16, 17, and 18 have passed
the current opt-in integration matrix. The complete V1 in the
[development plan](docs/database-migrator-rust-plano-desenvolvimento.md) is not
implemented yet. The library API and metadata schemas are provisional; no
compatibility guarantee is offered yet.

## Requirements

- Rust **1.89** or newer (the repository development toolchain is pinned to
  Rust **1.99.1** in `rust-toolchain.toml`).
- No SQLite CLI or server is required; SQLite is bundled through `rusqlite`.

## Quick start

```sh
cargo run -- init
cargo run -- new create_users
# Edit the generated file under migrations/versioned/.
cargo run -- validate
cargo run -- plan
cargo run -- up
cargo run -- status
cargo run -- history
cargo run -- doctor
cargo run -- down --steps 1
```

To install the executable locally, run `cargo install --path .`. Use
`--format json` for versioned, machine-readable command output, for example:
`dbm --format json plan`.

`dbm init` creates `dbm.toml` and `migrations/versioned/` without overwriting
an existing config. The default target is `dbm.sqlite`, marked `development`.
The config's namespace and source list can be edited before creating
migrations. Targets must be project-relative; source and database paths that
resolve outside the project are rejected.

## Migration files

The default single-file format has one Up section and an optional Down section:

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

Files use the name `<20-digit-UTC-timestamp>_<lowercase_description>.sql`.
`dbm new create_users --migration-format sql-pair` creates `.up.sql` and
`.down.sql` files with one shared identity. Checksums cover the exact source
bytes; editing, moving, renaming, or removing an applied migration is reported
as divergence rather than silently accepted.

SQLite accepts only SQLite-dialect transactional migrations. PostgreSQL
migrations use `-- dbm:dialect postgres`; `plan`, `status`, `history`, and
`doctor` resolve `url_env` only when connecting and keep the operation
read-only. PostgreSQL uses one pinned physical session for the advisory lock,
transactions, statements, and journal writes. An uninitialized target remains
untouched by read-only commands; PostgreSQL formats 3, 4, and 5 can be inspected,
but mutations require format 5. Incomplete or unsupported metadata fails
closed.

PostgreSQL connections require TLS with certificate-chain and hostname
validation against the WebPKI root set. For an explicitly local development or
test database only, plaintext requires all three: `sslmode=disable` in the
connection value, `allow_insecure_transport = true` in `[target]`, and a
loopback/Unix-socket host. Production configs reject that option. Private CA
certificates can be added from a PEM file referenced by the environment
variable named in `tls_ca_cert_env`; the file path is not stored in the config.
PostgreSQL `up`/`down` are enabled for non-production targets after the G4
integration gate. Transactional migration state and DDL commit together. The
only supported non-transactional statements are one unquoted, unqualified
`CREATE [UNIQUE] INDEX CONCURRENTLY ... ON table (column, ...)` in Up and,
optionally, a matching `DROP INDEX CONCURRENTLY` in Down. Reconciliation checks
the PostgreSQL catalog for index validity, schema, table, columns, uniqueness,
and default index options; it refuses to infer the result from the applied
migration row alone. Transaction-pooling PgBouncer connections are rejected
when the backend changes. An uncertain result is never retried automatically.

MySQL and MariaDB use `driver = "mysql"`, the `mysql` migration dialect, and a
database selected in the connection URL. Every migration must explicitly set
`-- dbm:transaction never`: DDL may commit implicitly, so the runner journals
intent and acknowledgement for each statement on one pinned physical session.
The paired `.up.sql`/`.down.sql` format is not accepted for MySQL/MariaDB in this
increment because it cannot carry that explicit policy; use the single-file
format.
The session-bound `GET_LOCK` lock is released before the command returns; loss
of a response leaves the run indeterminate and blocks further writes until
explicit, approved reconciliation. A repair plan does not execute or retry SQL.
MySQL 8.4 and MariaDB 10.11, 11.4, and 11.8 passed the local opt-in F5 matrix;
MySQL 8.4 and MariaDB 10.11 also passed the TLS CA and hostname checks. This
intermediate support is provisional and is not a V1 or production-readiness
claim.

MySQL/MariaDB connections require TLS with certificate-chain and hostname
validation by default. A private CA can be supplied through the environment
variable named by `tls_ca_cert_env`. Plaintext is permitted only with
`allow_insecure_transport = true` for a loopback target in `development` or
`test`; production configurations reject that opt-in.

## Experimental registry, DSL, and compiled runner

The provisional `database_migrator::dsl` API creates a typed, immutable AST,
requires explicit Up and Down plans, and compiles SQL without opening a
connection. `MigrationRegistry` can combine registered embedded SQL, DSL, and
Rust-generated plans with filesystem migrations. The shared engine journals
all resulting SQL steps; Rust migration callbacks receive a `MigrationContext`
and may execute only the exact statements in their fingerprinted plan. Rust
migrations currently require transactional execution. These APIs are
experimental and have no compatibility guarantee.

```rust
use database_migrator::dsl::{
    ColumnType, SchemaBuilder, SchemaDialect, SchemaMigration,
};

let mut up = SchemaBuilder::new();
up.create_table("users", |table| {
    table.column("id", ColumnType::BigInt)?.primary_key();
    table
        .column("email", ColumnType::Varchar(255))?
        .not_null()
        .unique();
    Ok(())
})?;
up.create_index("users_email_idx", "users", &["email"], true)?;

let mut down = SchemaBuilder::new();
down.drop_index("users_email_idx", "users")?;
down.drop_table("users", false)?;
let migration = SchemaMigration::new(up.finish()?, down.finish()?)?;
let sql_plan = migration.up().compile(SchemaDialect::Postgres)?;
let fingerprint = sql_plan.sql_fingerprint();
```

Identifiers are conservatively validated and quoted per dialect. Native UUID,
JSON and timestamp-with-time-zone types are explicit; unsupported semantics
return an error instead of silently approximating. `raw_sql` accepts exactly one
statement and requires an explicit dialect, safety class, and result shape; it
remains opaque and is not inspected as a typed operation. SQLite enforces
Boolean/JSON/Binary16 checks where representable and rejects fixed-precision
DECIMAL rather than claiming semantics its storage model cannot guarantee.

SQLite `plan`, `status`, `history`, and `validate` do not create a target
database or metadata tables. `up` and `down` execute each migration in its own `BEGIN IMMEDIATE`
transaction, journal statement hashes/states, and record the schema change and
successful run together. A multi-migration command can therefore have earlier
migrations committed if a later migration fails; inspect `status`, `history`,
and `doctor` before retrying. `down` preflights all requested migrations and
refuses to run if any selected migration is irreversible. `up`/`down` and
reconciliation are deliberately blocked when the configured target environment
is `production`.

`doctor` is read-only: it reports metadata format, source drift, unfinished runs,
steps, and diagnostic lock state without repairing anything. A failed or
interrupted run is recorded; unfinished runs and active locks block new writes
until explicitly reconciled, while a confirmed transactional rollback is safe
to retry. For an interrupted run, `dbm doctor reconcile` requires `--result`,
`--evidence`, `--repair-plan`, `--reason`, `--actor`, and `--approve`. The
repair plan is UTF-8 JSON (schema version `1.0`) with a summary and one ordered
entry per SQL statement: zero-based `step_index`, exact `statement_hash`, a
`state` of `confirmed_applied`, `confirmed_not_applied`, or `indeterminate`, and
an `inspection` note. An indeterminate step or a hash/state mismatch blocks
reconciliation. SQLite reconciliation relies on its transactional state;
PostgreSQL reconciliation is limited to the supported concurrent-index form
and verifies the live catalog while holding the advisory lock. The repair plan
is stored in metadata; only the SHA-256 reference to the evidence file is
stored, so retain that evidence separately. Do not put secrets in either file.
An active lock is never cleared by TTL or automatically stolen.

MySQL/MariaDB reconciliation binds the decision to the original source and
journal, but cannot generically infer the effects of arbitrary SQL. Inspect and,
when needed, manually repair every statement's effects before supplying an
approved repair plan; reconciliation itself does not inspect the application
schema or execute SQL.

```sh
dbm doctor reconcile --run-id RUN_ID --result operation-applied \
  --evidence ./verification.txt --repair-plan ./repair-plan.json \
  --reason "verified committed migration" --actor "operator" --approve
```

SQLite metadata formats 1 through 4 are not upgraded automatically.
`dbm metadata upgrade` shows the required action; inspect the target, then use
`dbm metadata upgrade --yes` to preserve history and journal records while
moving to format 5. SQLite upgrades formats 1–4 transactionally. PostgreSQL,
MySQL, and MariaDB can inspect formats 3–5; mutations require format 5 and an
explicit upgrade from format 3 or 4. PostgreSQL upgrades are transactional.
MySQL/MariaDB upgrades require the session named lock, but their DDL implicitly
commits; an interrupted/partially confirmed upgrade fails closed and needs
operator inspection before any retry. New metadata is initialized directly as
format 5 by the first migration run.

Concurrent `dbm new` commands are serialized per source directory. If a process
is killed while creating a migration, inspect stale `.dbm-create.lock` or
`.dbm-new-<timestamp>.lock` files before removing them; the CLI deliberately
does not guess whether a reservation is stale.

Repeatable SQL migrations use `R__<lowercase_name>.sql`. They run after all
versioned migrations, in configured source order and then by logical name, and
are reapplied only when missing or when their checksum changes. Every execution
is audited; repeatables have no automatic Down, and `down` never executes them.
The CLI's global `--namespace` overrides the configured namespace for one
invocation. Namespaces filter logical migration state; they do not establish
physical schema isolation or additional locks. Compiled migrations are selected
by their declared namespace.

Registry, DSL, repeatables, compiled-runner, and AI support are experimental and provisional.
Legacy import supports SQLite, PostgreSQL, MySQL, and MariaDB `inspect`, `plan`,
digest-gated `apply`, `verify`, and explicit `adopt`. Metadata upgrades are
explicit; network import and upgrade integration are opt-in and have only been
verified against the server versions listed in the development plan. Third-party
driver crates are still outstanding. AI assistance is available only with the
optional `ai` Cargo feature and is not part of the default binary. F9/G9 were
verified locally for PostgreSQL 16/17/18, MySQL 8.4, and MariaDB 10.11/11.4/11.8.
G10 was verified locally using fake providers, but no real AI service was called;
F11/G11 and the planned V1 are still incomplete. Prepared hardening and
release drafts are linked below; they are not release sign-off. Do not
treat this release as the planned V1 or use it to migrate production databases.

## Optional AI proposals

The default build does not include `reqwest` or expose AI commands. Enable the
feature explicitly when building:

```sh
cargo build --features ai
```

AI remains disabled until `[ai].enabled = true`. Profiles store only a reference
to the API-key environment variable; the secret itself must be supplied by the
calling environment. The endpoint must use HTTPS, except for loopback HTTP used
by local test servers.

```toml
[ai]
enabled = false
provider = "openai-compatible"
profile = "default"

[ai.profiles.default]
endpoint = "https://api.example.invalid/v1"
model = "your-model"
api_key_env = "DBM_AI_API_KEY"
timeout_seconds = 60
max_tokens = 4096
max_context_bytes = 131072
```

After setting `DBM_AI_API_KEY` in the process environment and explicitly
enabling AI, the commands `dbm ai generate`, `review`, `explain`, and
`suggest-down` return proposal/review JSON. `--preview-context` shows the exact
messages without resolving the secret or making a request. Context files must
be selected explicitly and remain inside configured migration sources; plan
files for `explain` must remain inside the project. Prompt context is redacted
for common credential forms and treated as untrusted data. AI does not connect
to a database, write migration files, run tests, or apply any result; there is
no `--apply` option. Generated SQL proposals pass the local migration parser and
configured dialect policy before they are returned, but still require human
review and the ordinary validate/plan/test/approval workflow. Public proposal
schemas are in [`docs/schemas`](docs/schemas/README.md). The feature remains
experimental despite local G10 verification and does not complete the planned V1.

To use compiled migrations from the CLI, create and build a project-owned Rust
binary that registers its migrations and explicitly calls
`database_migrator::runner::serve(registry)`. Point the project config at that
binary:

```toml
[execution]
runner = "target/release/dbm-project-runner"
runner_timeout_seconds = 3600
# Only these additional environment names are passed to the runner.
runner_env = ["PROJECT_MIGRATION_FEATURE"]
```

Connection-variable names from `[target]` are passed automatically; secret
values are never placed in process arguments or protocol frames. The runner
binary must resolve to a file inside the project. In runner mode, CLI
`validate`, `plan`, `status`, `history`, `doctor`, `up`, and `down` use the
compiled registry; `doctor reconcile` remains unavailable and fails closed.
Handshake, timeout, or fingerprint failures never fall back to applying only
filesystem SQL. `plan` and the read-only commands do not initialize metadata.
For an embedded application, call `engine::up_with_registry_async` or
`engine::down_with_registry_async` explicitly; importing the library never
runs migrations. The runner executes trusted native project code and is **not
a sandbox**. See `tests/fixtures/dbm-test-runner.rs` and
`tests/runner_protocol.rs` for the current integration contract.

## Legacy import and cutover (F8/G8 locally verified)

The read-only preview accepts the conventional Knex, Goose, or SQLx metadata
tables in the existing database selected by `dbm.toml`:

```sh
dbm --format json import inspect --from goose
dbm --format json import plan --from goose
dbm --format json import apply --from goose \
  --plan-digest sha256:REPLACE_WITH_PLAN_DIGEST --yes
dbm --format json import verify --from goose --import-id IMPORT_ID
```

Inspection preserves Knex filenames and batches, derives Goose's current state
from its ordered Up/Down events while retaining version-zero sentinels, and
retains SQLx checksums tagged as SHA-384. Active or unavailable Knex lock state,
failed SQLx rows, existing Database Migrator metadata, local identity
collisions, and detectable timestamp ordering conflicts block the plan. The
plan report lists the local migration fingerprints it includes. Its approval
digest is deterministic for the source history and local migration set.
`inspect` and `plan` do not initialize metadata. `apply` re-reads the source
under the target's native lock, requires the exact approved plan digest and
`--yes`, then records the source timeline and current legacy state without
executing legacy SQL or modifying legacy tables. `verify` checks source, events,
current rows, and digests. Rollback remains blocked until every current legacy
migration has an explicit audited adoption to an already-applied, reversible
native migration:

```sh
dbm --format json import adopt --from goose --import-id IMPORT_ID \
  --legacy-identity 42 --version 20261024000000000000 \
  --evidence ./equivalence-review.txt \
  --reason "reviewed native replacement" --actor "operator" --yes
```

The current importer recognizes conventional source table names. The opt-in
adapter conformance tests exercise Knex, Goose, and SQLx history against
PostgreSQL, MySQL, and MariaDB; end-to-end CLI journeys cover concurrent and
repeat `apply`, drift detection, SQLx failure, adoption, and the rollback gate.
G8 was verified locally on PostgreSQL 16, MySQL 8.4, and MariaDB 11.4.13; this
does not expand the separately documented server support matrix or approve
production cutover.

For static PostgreSQL source validation, a config may reference (but must not
contain) the connection variable:

```toml
version = 1
namespace = "default"
source = ["migrations/versioned"]

[target]
driver = "postgres"
url_env = "DATABASE_URL"
environment = "test"
```

Static validation does not resolve `DATABASE_URL`. PostgreSQL read-only
commands resolve it only when connecting and inspect uninitialized targets or
metadata formats 3, 4, and 5. PostgreSQL `up`/`down` and explicit reconciliation
require metadata format 5. Explicit v3→v5 and v4→v5 metadata upgrades are
available via `dbm metadata upgrade --yes`.

MySQL/MariaDB config keeps only environment-variable references, not connection
URLs or CA file paths:

```toml
version = 1
namespace = "default"
source = ["migrations/versioned"]

[target]
driver = "mysql"
url_env = "DATABASE_URL"
tls_ca_cert_env = "DATABASE_CA_FILE" # optional private CA
environment = "test"
```

MySQL/MariaDB migrations must opt into non-transactional execution explicitly:

```sql
-- dbm:dialect mysql
-- dbm:transaction never
-- dbm:up
CREATE TABLE users (id BIGINT PRIMARY KEY);
-- dbm:down
DROP TABLE users;
```

## Development checks

Run from the repository root with the pinned toolchain:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --locked
cargo run --example sqlite_embedded_migration --locked
cargo run --example sqlite_schema_dsl --locked
```

The default tests use temporary directories and real temporary SQLite files;
they do not connect to database servers or require Docker. Opt-in integration
tests are ignored by default and require disposable loopback targets. The
PostgreSQL tests use `DBM_TEST_DATABASE_URL`; plaintext lock/snapshot and CLI
tests require `sslmode=disable`, while the TLS test also requires
`DBM_TEST_POSTGRES_CA_FILE` and a matching local certificate. The PgBouncer test
uses `DBM_TEST_PGBOUNCER_URL` through transaction pooling with at least two
server connections. MySQL 8.4 and MariaDB tests use `DBM_TEST_MYSQL_URL` and
`DBM_TEST_MARIADB_URL`, respectively; their test account must be able to create
and drop a uniquely named database on that loopback instance. TLS checks use
`DBM_TEST_MYSQL_TLS_URL` with `DBM_TEST_MYSQL_CA_FILE`, and
`DBM_TEST_MARIADB_TLS_URL` with `DBM_TEST_MARIADB_CA_FILE`. These journeys cover
locks, deadlocks, partial multi-step failures, explicit recovery, process loss,
and certificate/hostname validation. Never use a production or shared database
for these tests. `tests/legacy_import_network.rs` adds opt-in cutover journeys:
the PostgreSQL test uses `DBM_TEST_DATABASE_URL`, the MySQL test uses
`DBM_TEST_MYSQL_URL`, and the MariaDB test uses `DBM_TEST_MARIADB_URL`. They
create and drop a randomized schema/database and must only target disposable
loopback instances. The compile-only DSL integration cases are in `tests/dsl.rs`;
they use the same loopback variables, create randomized schema objects, execute
the generated Up and explicit Down statements, and verify foreign-key, unique,
and check constraints. The G6 run covered PostgreSQL 16/17/18, MySQL 8.4, and
MariaDB 10.11/11.4/11.8.

The current evidence and remaining platform gaps are summarized in the
[provisional support matrix](docs/support-matrix.md). For metadata format
upgrades, read the [upgrade guide](docs/upgrade-guide.md); for interrupted runs,
read the [recovery drills](docs/recovery-drills.md). Dependency audit and SBOM
commands and the fuzz harness are documented in
[`docs/supply-chain.md`](docs/supply-chain.md). `F11/G11` remains pending until
the audit, fuzz runs, full fault matrix, and CI results are reviewed. The
[changelog](CHANGELOG.md) and [release checklist](docs/release-checklist.md) are
drafts only; no release is approved or published.

## License

Licensed under either the [MIT license](LICENSE-MIT) or
[Apache License, Version 2.0](LICENSE-APACHE), at your option.
