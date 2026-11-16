# Local ledger

Timestamped SQLite for a phone expense ledger. The person records who they
paid, the amount in cents, the day, and a category. The first published
schema creates `expense`. The current chain adds `category` with default
`geral`. An older install keeps its rows.

`phone/` holds the exact up SQL bytes the phone executes. The migration
files wrap those bytes with `dbm` markers. `dbm chain` hashes the up bytes,
not the migration file, and does not open `ledger.sqlite`. Down SQL is not
executed.

`published/ledger-1.txt` is the first shipped schema.
`published/ledger-head.txt` is the current chain. The Jirau core and the
Flutter example copy `phone/` and check these checksums.

From the repository root:

```sh
cargo run --locked --bin dbm -- --config examples/local-ledger/dbm.toml chain
```
