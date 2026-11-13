# Security revalidation — 2026-11-13

This note reviews the known external report and the code on
`5492e748b8eaf72bf8e274d643e7f452e90c1d40`. It does not replace that report,
does not change historical verdicts, and does not assert that no critical
issues exist.

## External report

The source-first review remains the run whose baseline is
`3822695f15d5dbcbdc38872fb349b640ab2844dd`. It recorded 7 findings: 1 confirmed
low, 4 `needs_validation`, and 2 rejected, across 18 coverage units. Official
validators passed on 2026-11-11. `findings.json` has no `fixed` verdict.

## Mitigations still present

- PostgreSQL `U&"..."` identifiers, including `UESCAPE`, are rejected in
  `src/migration.rs`. Literal Unicode identifiers such as `"café"` stay
  allowed. The regression is
  `postgres_guard_rejects_unicode_escaped_identifiers_but_allows_literal_unicode`.
  The confirmed finding is not reclassified.
- Plaintext `localhost` is not treated as loopback. PostgreSQL requires a
  numeric loopback address or an explicit loopback `hostaddr`. MySQL and
  MariaDB require a numeric loopback address or a non-empty resolved address
  list that contains only loopback addresses.
- The AI client compares the profile endpoint with `DBM_AI_APPROVED_ENDPOINT`
  before it reads the API key (`src/ai.rs`).
- The runner is copied and hashed, then the snapshot is spawned
  (`src/runner.rs`). That program is still trusted native code, not a sandbox.
- `init` and `new` write through the already opened project directory with
  `create_new` (`src/source.rs`).
- SQLite opens set `SQLITE_OPEN_NOFOLLOW` on the final database path component.

The default workspace suite on that commit passed these regressions: 162 tests
passed and 49 were ignored. The server matrix below was run on the same commit.

## Server matrix on this commit

Disposable rootless containers, published only on `127.0.0.1`, started with
`--pull=never` and without host mounts:

- `postgres:18.6`
- `mysql:8.4.13`
- `mariadb:11.8.9`

Command:

```sh
cargo test --workspace --all-features --locked -- --ignored --skip pgbouncer --skip _tls_ --test-threads=1
```

The clean run passed 45 tests and failed 0: 16 library/engine, 23 CLI, 3 DSL,
and 3 legacy-import tests. The containers and their anonymous volumes were
removed afterward.

Two earlier attempts on the same containers are not evidence. A PostgreSQL URL
that contained a password failed the statement-proxy tests, which require a
passwordless URL. The next attempt reused the MySQL database that the metadata
upgrade test mutates in place and does not restore. The recorded pass used a
fresh database and passwordless trust authentication on that disposable
PostgreSQL container only.

Not re-run on this commit: PgBouncer, the three TLS tests, the default-parallel
suite, PostgreSQL 16/17, MySQL 8.4.11 and 9.7.4, MariaDB 10.11, 11.4, 11.8.2,
and 13.0.2, and MariaDB 12.3.

## Fuzz on this commit

The campaign ran in rootless image `dbm-fuzz-tools:20261029`
(`sha256:2e21d3923dc9f005d3eb528f9dde2651da3618ea6467ac008c3450aa4ebab705`),
with network mode `none`, a read-only root and source mount, all capabilities
dropped, `no-new-privileges`, 2 CPUs, 4 GiB of memory and no swap, and a PID
limit of 128. Cargo used one build job. The toolchain was rustc/cargo
1.101.0-nightly (`41089d07a`, 2026-10-27), cargo-fuzz 0.13.2, and Debian Clang
14.0.6. LibFuzzer flags were `-workers=1 -max_total_time=300 -max_len=65536
-timeout=10 -rss_limit_mb=2048`.

| Target | Executions at DONE | RSS at DONE | Exit | Crash files |
|---|---:|---:|---:|---:|
| `sql_scanner` | 1,688,789 | 541 MiB | 0 | 0 |
| `migration_parser` | 2,423,239 | 564 MiB | 0 | 0 |

Each target stopped on the 300-second limit, at about 301 seconds. The retained
corpora are outside the repository. `sql_scanner` has 1,416 files and 146,194
bytes, aggregate SHA-256
`35fb7b7fdfef375f8ab3fb0f6e3c06a23fe9cb4e614a7074c45e272998274a09`.
`migration_parser` has 1,441 files and 176,749 bytes, aggregate SHA-256
`834a094831941561074bc5eb6b00347caf2e4503322b4e5cb6a54aebf57cfe4b`. No crash
files were present. These bounded runs are not a proof of parser correctness.

`cargo-fuzz` printed a weak `GLIBC_2.39` warning and continued. The host has no
`c++` compiler, so the campaign used the image's Clang.

## Boundaries that remain unvalidated

- A same-version backend swap behind one configured host, such as PgBouncer or
  DNS, is not detected by the target fingerprint.
- The compiled runner is trusted process-level code.
- `SQLITE_OPEN_NOFOLLOW` does not cover an ancestor-directory race, a
  migration-file race, or replacing a regular file at the same path.
- The default-parallel opt-in suite is still unverified.
- PgBouncer evidence used a Bookworm test image, not the default Trixie image,
  and was not repeated on this commit.
- The TLS tests were not repeated on this commit.
- MariaDB 12.3 has never been exercised.
- macOS and Windows are not verified by CI.
- The optional AI path was not called against a real provider.
- SQLite scenarios #1, #2, #4, and #5 were not repeated with an independent
  journal-identity harness.
- The external audit did not execute this commit.

## Release identity checked here

The package name `database-migrator` was unused on crates.io on 2026-11-13.
This note does not publish it. The binary name is `dbm`. The MSRV is 1.89.
The license is `MIT OR Apache-2.0`. Metadata format 5 stays the mutable format
for PostgreSQL, MySQL, and MariaDB. The declared platform for 1.0.0 is Linux
x86_64.
