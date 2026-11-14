# Opt-in embed

The 1.0 contract is the default build of `database-migrator`: the full `dbm`
product. An application can opt into a smaller SQLite library build. That
build still uses Tokio. It is a feature cut of this crate, and the separate
client library remains another repository.

Release 1.0.0 stays evidenced on Linux x86_64. This page does not add a
platform, publish the crate, or change metadata format 5.

## Default features

`cargo build` and `cargo install --path .` use the default features:

| Feature | Default | What it compiles |
|---|---|---|
| `sqlite` | on | SQLite driver |
| `postgres` | on | PostgreSQL driver and `tokio-postgres` |
| `mysql` | on | MySQL and MariaDB driver and `mysql_async` |
| `cli` | on | `clap`, the `dbm` binary, and the `runner` and `legacy` features |
| `runner` | on | `runner::serve` |
| `legacy` | on | legacy import commands in the facade |
| `dsl` | on | `database_migrator::dsl` and DSL registry entries |
| `ai` | off | AI command and its dependencies |
| `test-runner-fixture` | off | the test runner binary; also enables `dsl` and `runner` |
| `fuzzing-harness` | off | fuzz entry points |

`cli` pulls `runner` and `legacy`. `required-features = ["cli"]` on the `dbm`
binary tells Cargo to skip that binary when `cli` is off. It does not turn
`cli` on. The default set is what makes a plain `cargo build` produce `dbm`.

`examples/sqlite_embedded_migration.rs` requires `sqlite`.
`examples/sqlite_schema_dsl.rs` requires `sqlite` and `dsl`.

## SQLite-only build

The crate is not on crates.io. Depend on it by path:

```toml
[dependencies]
database-migrator = { path = "path/to/database-migrator", default-features = false, features = ["sqlite"] }
```

On 2026-11-14, the direct normal dependencies of that profile were:

- `database-migrator-core`
- `database-migrator-sqlite`
- `rusqlite` with bundled SQLite
- `tokio`
- `cap-std`, `serde`, `serde_json`, `sha2`, `tempfile`, and `uuid`

The same profile, checked with `cargo tree` on the normal, dev, and build
edges, does not contain `clap`, `tokio-postgres`, `mysql_async`,
`database-migrator-postgres`, or `database-migrator-mysql`. Cargo skips the
`dbm` binary and the DSL example. The modules `cli`, `runner`, `legacy`, and
`dsl` stay out of that build. SQL and Rust registry entries stay. The engine
still calls `validate_database_info`, which succeeds when `dsl` is off.

Tokio remains a direct dependency of the facade, the core crate, and the
SQLite driver. The SQLite connection is owned by a blocking worker, and that
worker is scheduled with `tokio::task::spawn_blocking`, as recorded in
[ADR-014](adr/ADR-014-async-core-e-adaptadores-de-banco.md). The embedded
example runs `engine::up_with_registry_async` on a Tokio runtime.

These pieces also stay when `sqlite` is the only feature:

- metadata format 5
- production plan and target approval on the engine
- the legacy model in `database-migrator-core`

The SQLite adapter answers legacy inspection with the unsupported default.
PostgreSQL and MySQL/MariaDB still compile `inspect_legacy_history` when their
own engine feature is enabled. Turning the facade `legacy` feature off leaves
those driver methods in place.

`sqlite` plus `cli` builds `dbm` for SQLite and pulls `runner` and `legacy`.
Add `postgres`, `mysql`, or `dsl` only when that build needs them. `mysql`
covers MariaDB. A disabled engine answers that its feature is required.

## Client library

The client library is a later repository. It is a synchronous SQLite library
for the Jirau `*-core` and for Flutter prebuilts. It copies the journal
invariants into its own crate, with no dependency on `database-migrator`.
Metadata format 5 and production plan approval stay in this crate. Expo and
React Native stay out. Flutter desktop and Flutter web wait until a binary
and a runtime exist for those targets. Sync and SQLCipher stay out.

## Checks that support this page

The feature cut was verified on Linux on 2026-11-14, in the commit that added
the features. Formatting, `git diff --check`, workspace Clippy with
`-D warnings`, `cargo test --workspace --all-features --locked` (162 passed,
49 ignored), the workspace build, both SQLite examples, and
`scripts/supply-chain.sh` passed. The SQLite-only Clippy of the library,
bins, and examples passed, with `dbm` and the DSL example skipped. The server
fault matrix and remote CI did not run for that cut. This page does not rerun
those checks. The direct dependency list above was read again while writing
it.
