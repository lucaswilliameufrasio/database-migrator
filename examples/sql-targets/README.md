# SQL targets

One local project, three configs, timestamped SQL. SQLite uses the bundled
engine. PostgreSQL is `postgres:18.6`. MariaDB is `mariadb:11.8.9`. MySQL is
not part of this example. Every target is `development`.

The database password below exists only for these disposable containers.

## SQLite

From the repository root:

```sh
cargo run --locked --bin dbm -- --config examples/sql-targets/sqlite/dbm.toml plan
cargo run --locked --bin dbm -- --config examples/sql-targets/sqlite/dbm.toml up
cargo run --locked --bin dbm -- --config examples/sql-targets/sqlite/dbm.toml down --steps 1
```

`plan` does not create `examples/sql-targets/sqlite/items.sqlite`.

## PostgreSQL and MariaDB

```sh
docker compose -f examples/sql-targets/compose.yaml up --wait
export DBM_EXAMPLE_POSTGRES_URL='postgres://dbm:dbm@127.0.0.1:55432/dbm?sslmode=disable'
export DBM_EXAMPLE_MARIADB_URL='mysql://dbm:dbm@127.0.0.1:53306/dbm'
cargo run --locked --bin dbm -- --config examples/sql-targets/postgres/dbm.toml plan
cargo run --locked --bin dbm -- --config examples/sql-targets/postgres/dbm.toml up
cargo run --locked --bin dbm -- --config examples/sql-targets/postgres/dbm.toml down --steps 1
cargo run --locked --bin dbm -- --config examples/sql-targets/mariadb/dbm.toml plan
cargo run --locked --bin dbm -- --config examples/sql-targets/mariadb/dbm.toml up
cargo run --locked --bin dbm -- --config examples/sql-targets/mariadb/dbm.toml down --steps 1
docker compose -f examples/sql-targets/compose.yaml down --volumes
```

Both published ports listen on `127.0.0.1` only. The Compose file does not
mount a host directory. `down --volumes` removes the containers and their
anonymous data volumes.
