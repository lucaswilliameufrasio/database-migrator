# Dependency audit, fuzzing, and SBOM

This document records the current hardening setup. It is not a completed
security audit or a release approval.

## Dependency policy

The root `Cargo.lock` is the dependency inventory for the application. The
GitHub Actions `supply-chain` job runs:

```sh
cargo audit
cargo deny --all-features --locked check
cargo audit --file fuzz/Cargo.lock
cargo deny --manifest-path fuzz/Cargo.toml --all-features --locked check
cargo cyclonedx --all-features --format json
```

The checked-in `deny.toml` permits only the SPDX licenses currently observed in
the locked dependency metadata, rejects unknown registries and Git sources,
denies yanked versions and all unmaintained advisories, and contains no advisory
exceptions. Unsound advisories affecting workspace dependencies fail the check.
Duplicate dependency versions warn for review; wildcard version requirements
are denied. A vulnerability or policy failure is not suppressed to make CI
green.

The job installs pinned tools with `cargo install --locked`:

| Tool | Pinned version | Upstream release |
|---|---:|---|
| `cargo-audit` | 0.22.2 | [RustSec release](https://github.com/RustSec/rustsec/releases/tag/cargo-audit/v0.22.2) |
| `cargo-deny` | 0.20.2 | [Embark Studios release](https://github.com/EmbarkStudios/cargo-deny/releases/tag/0.20.2) |
| `cargo-cyclonedx` | 0.5.9 | [CycloneDX Rust Cargo releases](https://github.com/CycloneDX/cyclonedx-rust-cargo/releases) |

Versions were checked on 2026-10-26. The workflow fetches current RustSec
advisory data; a local offline run cannot establish the current advisory result.
The separate fuzz package has its own `fuzz/Cargo.lock`. The supply-chain script
also checks this graph with the same advisory and license policy; root-lock
scans alone do not cover it. The root SBOM still describes the application,
not the separate fuzz toolchain.

## SBOM handling

The workflow generates a CycloneDX JSON SBOM for all application features and
uploads `bom.json` as a 30-day Actions artifact named with the source commit
SHA. It is an ephemeral CI artifact, not a release asset, signature, attestation,
or proof that the binaries have been built or audited. The checked-in supply
chain script validates the basic document identity and non-empty dependency
component list before upload.

To run the same checks locally, first install the pinned tools above, then run:

```sh
sh scripts/supply-chain.sh
```

The generated root-level `bom.json` is ignored by Git.

## Fuzz harness

The `fuzz/` package contains `sql_scanner` and `migration_parser` libFuzzer
targets. It enables the opt-in `fuzzing-harness` crate feature, which is disabled
in normal builds and exposes only hidden wrappers around the existing parser
and scanner. The seeds cover SQL quoting/comments, dialect-specific delimiters,
Unicode, section markers, and malformed directives.

The verified upstream [`cargo-fuzz` 0.13.2 release](https://github.com/rust-fuzz/cargo-fuzz/releases/tag/0.13.2)
was current on 2026-10-26. The targets require a nightly Rust toolchain,
libFuzzer's LLVM sanitizer support, and a C++11-capable compiler; upstream
supports x86-64/AArch64 Unix-like hosts, not Windows. Install the pinned fuzzer
and a nightly toolchain first:

```sh
rustup toolchain install nightly --profile minimal
cargo install --locked --version 0.13.2 cargo-fuzz
```

Example bounded
invocations (run only in a disposable, resource-limited, network-disabled
sandbox) are:

```sh
cargo +nightly fuzz run sql_scanner -- -max_total_time=300 -max_len=65536
cargo +nightly fuzz run migration_parser -- -max_total_time=300 -max_len=65536
```

**Status 2026-10-29:** `cargo-fuzz` 0.13.2, `cargo-audit` 0.22.2,
`cargo-deny` 0.20.2, and nightly `1.101.0-nightly` were installed in a
disposable rootless-Docker scratch environment. A scratch-only
`fuzz/Cargo.lock` was generated for 242 packages, but the dependency fetch and
full graph audit did not complete: the shared `/tmp` tmpfs reached 100% while
Cargo unpacked target-specific crates. No fuzz target was executed. The
generated lockfile remains outside the checkout and is not audited or accepted
for commit. The targets and seed corpora are prepared, but this is not fuzz
coverage evidence and does not close G11.

**Follow-up 2026-10-29 (current checkout):** the specified scratch directory
was empty, with no saved fuzz error or crash artifact found. The previous local
failure therefore remains undiagnosed; the earlier `/tmp` exhaustion is not
evidence of its cause. The user authorized continuing without that output.
Preflight found 735 GiB available on `/mnt/storage/projects` and verified the
Docker `rootless` context via `name=rootless`. The only listed container was
unrelated and was left unchanged. The checkout already contained an untracked
`fuzz/Cargo.lock` with 244 packages, distinct from the earlier 242-package lock.

With `cargo-deny` 0.20.2 and `cargo-audit` 0.22.2:

- `cargo audit --file fuzz/Cargo.lock` passed, scanning 244 dependencies after
  fetching the RustSec database (1,386 advisories).
- `cargo deny --manifest-path fuzz/Cargo.toml --all-features --locked check`
  initially failed for missing harness license metadata, an unversioned local
  dependency, and the NCSA license required by `libfuzzer-sys 0.4.13`.
- The harness now declares the same `MIT OR Apache-2.0` license as the root
  package and pins its local dependency to `=0.1.0`. Repeating the locked check
  returned exit 4: `advisories ok, bans ok, licenses FAILED, sources ok`.
  `libfuzzer-sys` declares `(MIT OR Apache-2.0) AND NCSA`; NCSA is not allowed
  by the unchanged `deny.toml`. Duplicate-version warnings also remain.

Logs are preserved under
`/mnt/storage/projects/.scratch/dbm-fuzz-20261029/attempt-01/logs/` as
`cargo-deny.log`, `cargo-deny-after-metadata.log`, and `cargo-audit.log`.
The lockfile is **not accepted for commit** because the policy audit failed.
Neither target was built or executed; the two 300-second runs with
`-max_len=65536` remain blocked on a policy-compliant dependency solution.
No license allowance, advisory exception, or sandbox relaxation was added.
The 12-scenario matrix and four pending security validations remain unverified;
G11 and G12 remain pending, with no release authorization inferred.

**Owner-approved continuation 2026-10-29:** the project owner explicitly
authorized adding NCSA to the license allowlist after reviewing the notice,
redistribution, and non-endorsement requirements. This supersedes the earlier
instruction to leave this specific allowance unchanged; advisory policy and
sandbox requirements are unchanged. The allowance applies to all graphs using
`deny.toml`, not solely to the fuzz package. The application remains
`MIT OR Apache-2.0`; fuzz tooling redistribution must retain applicable upstream
copyright, conditions, license texts, and disclaimers and must not suggest
endorsement. See [ADR-022](adr/ADR-022-hardening-e-fuzzing.md).

After adding only `NCSA` to the allowlist, the fuzz locked check passed:
`advisories ok, bans ok, licenses ok, sources ok`. A fresh
`cargo audit --file fuzz/Cargo.lock` also passed (244 dependencies, 1,386
advisories). The 244-package lock is accepted for inclusion on that evidence;
duplicate-version warnings remain warnings under the unchanged bans policy.
The supply-chain script now checks both lockfiles. Fresh logs and preparation
artifacts are under the same scratch root in `attempt-02/`. At this point in the
timeline, target execution was still pending; results are recorded below.

The complete updated `scripts/supply-chain.sh` passed locally using the pinned
tools, including both lockfile audits and CycloneDX generation/validation. The
generated application SBOM parsed as CycloneDX 1.3 with 238 components; this is
local evidence, not a new CI run or release artifact. RustSec data was at
`ee1a49a2769a24a596afc1c20cb33748d2769586`. Logs: `attempt-02/logs/supply-chain.log`.

Preparation failures were preserved rather than treated as target findings:
the first image build hit the supervisor's 120-second timeout while installing
compilers; a longer preparation completed. Initial containers could not start
because Docker's local log driver rejects compression with `max-file=1`;
`compress=false` retains the same single-file log budget. The next attempts
passed the cgroup/network/read-only preflight, but the fuzz build rejected the
unsupported `-j` option, and Clippy could not execute a build script on Docker's
default noexec tmpfs. The corrected setup uses `CARGO_BUILD_JOBS=2` and permits
execution only in the target-output tmpfs; other writable scratch remains
noexec. Aggregate `/work` storage remains 2 GiB (128 MiB scratch plus 1,920 MiB
target output), with 4 GiB memory/no swap, two CPUs, 128 processes, and a separate
128 MiB `/tmp`. Test temporary projects use `/work/target/tmp` because runner
tests execute copied fixtures. No seccomp, capability, source-write, or network
exception was added. Source/toolchain/vendor mounts are read-only, with no host
home or daemon socket exposed. All containers use the verified rootless daemon.

The first all-targets Clippy compilation then exposed missing read-only mounts
for the four JSON schemas included by AI tests; the rerun adds only
`docs/schemas`. The fuzz build using GCC failed while compiling bundled SQLite:
`File size limit exceeded` under the 128 MiB per-file limit, before either fuzz
target ran. The next fuzz build uses Clang 14 with
`CC=clang`, `CXX=clang++`, `CFLAGS=-gline-tables-only`, and matching `CXXFLAGS`,
retaining line information, ASan, optimization, and all existing resource limits.
Earlier build logs are retained as `clippy-attempt04.log` and
`fuzz-build-attempt04.log` under `attempt-02/logs/`; these are preparation failures,
not parser findings.

The Clang/ASan fuzz build succeeded in the rootless, network-disabled container.
`sql_scanner` exited 0 after 2,225,928 executions in 301 seconds with
`-max_total_time=300 -max_len=65536`, peak RSS 570 MiB, and no artifact reported
by the container-side artifact listing. The log is `sql_scanner-evidence.log`;
the snapshot and controls are `sandbox-results-attempt05.json` and
`sandbox-controls-attempt05.json`. After this successful run, the supervisor's
`docker cp` could not access the container tmpfs; collection failed and the
container stopped before `migration_parser` started. The scratch corpus was
therefore not retained. A new attempt uses a bounded tar stream from inside the
container to preserve crash artifacts. In that fresh sandbox the build and
`migration_parser` completed successfully: exit 0, 2,464,278 executions in
301 seconds, `-max_total_time=300 -max_len=65536`, peak RSS 566 MiB. Bounded
artifact collection succeeded and found no crash files. The generated corpus was
not retained. Evidence is `migration_parser-evidence.log`,
`sandbox-results.json`, and `sandbox-controls.json` under `attempt-02/`.
Both runs used nightly rustc 1.101.0-nightly (commit `41089d07a0a2`, LLVM
23.1.4), cargo-fuzz 0.13.2, Clang 14, and AddressSanitizer. LibFuzzer stopped
after 301 seconds despite the 300-second setting; a 330-second external timeout
bounded each execution. These bounded runs are not a proof of parser correctness.

**Both targets with retained corpus — 2026-11-07:** from commit `abb5aeb`, the
rootless, network-disabled sandbox passed preflight and fuzz build, then ran
`sql_scanner` for 301 seconds (2,309,020 executions, peak RSS 569 MiB) and
`migration_parser` for 301 seconds (2,404,475 executions, peak RSS 578 MiB), with
`-max_total_time=300 -max_len=65536 -timeout=10 -rss_limit_mb=2048`. Both targets
and their evidence collection exited 0; neither reported a crash artifact. The
generated corpora were retained: `sql_scanner` has 1,039 files/149,610 bytes
(aggregate SHA-256
`e40bf6e42abed9353e4358ce67ebb070b81cac4ec918e557d173b14de573c875`), and
`migration_parser` has 1,479 files/146,942 bytes (aggregate SHA-256
`28ec4325a1698e3fdc07888eee99a44a0919a0a923a29d1d130215f6fd639f5e`). No crash
artifacts were present in either bounded output directory.

Controls were verified from container metadata: rootless Docker, network mode
`none`, read-only root and source/toolchain/vendor mounts, all capabilities
dropped, `no-new-privileges`, 2 CPUs, 4 GiB memory with no swap, and 128 PIDs.
The container was removed after collection. The 41-file input manifest and
per-file corpus hashes are under
`/mnt/storage/projects/.scratch/dbm-fuzz-20261107/attempt-01/`; the retained
evidence manifest SHA-256 is
`48cf8ebafa9e4b65314bfdf44a28655ae41ed785e256b318a9e198baea349f32`. After
manifest capture, changes in the mounted checkout are confined to `cfg(test)`
code in `src/database/sqlite.rs`, `src/engine.rs`, and `src/storage.rs`, plus CLI
integration tests in `tests/cli.rs`; none is compiled into either fuzz target.
All fuzz-relevant non-test sources, fuzz target files, manifests, lockfiles, and
seeds matched the manifest. The
initial container-create attempt did not start because its offline vendor/config
mounts were absent; the corrected retry above is the only successful result for
this attempt. The preflight printed a `GLIBC_2.39` warning from `cargo-fuzz`, but
reported version 0.13.2 and the bounded build/runs all exited 0. These bounded
runs are evidence for those inputs and runtime only, not a correctness proof.

The isolated local quality check passed Clippy, the full non-ignored all-features
workspace tests, and the all-features build. The tests included all five
`runner_protocol` tests. Cargo's test/dev debug information was limited to line
tables and incremental compilation disabled to fit the 128 MiB per-file limit;
this is not an exact default-profile CI rerun. The first example then failed to
compile with `No space left on device` because the bounded target tmpfs filled;
it was not an example runtime failure. In a fresh bounded container both
examples then passed: `sqlite_embedded_migration` applied and rolled back its
isolated migration, and `sqlite_schema_dsl` printed its fingerprint and CREATE
TABLE. Logs: `example-migration.log`, `example-dsl.log`, with exit codes in
`quality-results.json`. Opt-in network database tests remain unexecuted; no
shared or production databases were used.

## Security review status

A repository-wide source-only security review was performed on 2026-10-26 at
commit `9a8dd95c3143fe9adc59f1c51fba820ad05db129`. It mapped 20 units: 13 were
source-covered, six candidate-linked units represent four unique
`needs_validation` records, and one supply-chain unit is blocked. Two other
hypotheses were rejected; no candidate was confirmed. The complete report and
structured records are maintained outside this repository. This was not a
complete validated audit: target code was not run because offline locked
dependency resolution was incomplete. Bounded parent-side promotion smokes
passed for synthetic stdout and two synthetic files written to isolated scratch
mounts; none executed target code or validated target-generated files or the
complete target sandbox. The dependency checks
above are not a substitute for reviewing trust boundaries, credential handling,
file/path boundaries, database adapters, runner execution, or AI proposal
handling. Do not infer “no critical issues” from this review or a green
dependency scan. Later bounded fuzz runs and local dependency checks are
recorded above; they do not retroactively expand this source review's scope.
Four candidate validations and the complete failure matrix remain open; G11
remains pending.

A follow-up through documentation commit `38f8882` verified that rootless
Docker smoke containers expose explicit CPU, memory, process, and tmpfs bounds
with a read-only root filesystem and network-disabled configuration.
Separate bounded promotion smokes passed for 16 synthetic stdout bytes and two
synthetic files (19 and 18 bytes) written by harmless containers to isolated
scratch mounts, all using parent-side descriptor-based checks. They did not
execute target code or validate target-generated files. Offline vendoring
stopped because the locked crate `combine v4.6.9` is missing; nothing was
fetched.

GitHub Actions run `40439858818`, attempt 2, later passed formatting, Clippy,
tests, build, and executable examples. Its supply-chain job failed because
`cargo-deny` rejected `rustls-pemfile 2.2.0` under `unmaintained = "all"`
(`RUSTSEC-2025-0134`). CycloneDX generation and SBOM upload were skipped. The
implementation now uses the maintained `rustls-pki-types` PEM API instead;
the dependency-policy check now passes. In run `40452036445`, CI then failed
because `cargo-cyclonedx`'s default output filename did not match the script's
expected `bom.json`; the upload did not run. Commit `980629d` requests the `bom`
output filename explicitly. Run `40452962185` passed both CI jobs, including
`cargo audit`, `cargo-deny`, SBOM generation and script validation, and artifact
upload. The uploaded artifact `database-migrator-sbom-980629d54524f5f0ab403dfcbdb4dc3ed19064de`
was downloaded and `bom.json` parsed as CycloneDX 1.3 with 238 components. The
dependency policy was not weakened. Later local bounded fuzz and quality
evidence is recorded above; it does not complete the 12-scenario failure matrix
or the four security validations. G11 remains pending.

**Progress review 2026-11-06:** both fuzz targets, dependency gates, local SBOM
validation, Clippy, non-ignored all-features workspace tests, build, and both
SQLite examples have recorded successful local evidence above. The recovery
matrix is mapped in [`recovery-drills.md`](recovery-drills.md), with only the
SQLite in-transaction kill test identified as executed among the relevant crash
boundaries; the opt-in server tests were ignored. The external security-review
report and four structured `needs_validation` records were not present in this
checkout or the inspected scratch artifacts, so their content/status cannot be
independently updated here. No G11 approval or G12/release authorization is
implied. The exact fuzz corpus was not retained. The push-triggered run
`42312783506` on commit `5672dd0df75774cdc28d161d01428bce13b66bc2` subsequently
passed both CI jobs: formatting, Clippy, tests, build, examples, both dependency
audits, SBOM generation, and artifact upload. This does not close the recovery
matrix or four candidate validations.

Remaining G11 evidence includes the incomplete twelve-scenario matrix, the four
external candidate validations, and review/triage of the separate security
assessment. A full bounded run of both fuzz targets with generated corpora and
artifact directories retained is now recorded above; this remains bounded fuzz
evidence, not proof of correctness. The local performance baseline was rerun on
2026-11-07 and is recorded in the development plan; the one-second objective on
CI reference hardware remains unverified.

After adding focused SQLite checks for competing CLIs and stale migration
sources and the shared-schema namespace race, the local all-features workspace
suite passed with 140 tests passed, 25 ignored, and zero failures; Clippy, build,
formatting, and both executable examples also passed. This local gate does not
cover the ignored server integrations or finish the documented recovery matrix.

**Final local supply-chain check 2026-11-06:** `sh scripts/supply-chain.sh`
passed again with the pinned audit tools and refreshed RustSec data (1,422
advisories): root lock 300 packages and fuzz lock 244 packages; both audit and
deny graphs passed. The regenerated application BOM validated as CycloneDX 1.3
with 238 components. Log: `attempt-02/logs/supply-chain-final.log`. The
push-triggered CI run `42312783506` on `5672dd0` also passed both jobs and
uploaded the SBOM; see the recorded workflow result above.

**Final local gate 2026-11-07 (after metadata-expectation corrections):**
`cargo fmt --all -- --check`, `git diff --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo test --workspace --all-features --locked`,
`cargo build --workspace --all-features --locked`, both documented SQLite
examples, and `sh scripts/supply-chain.sh` passed. The workspace test run had
143 passed, 49 ignored, and zero failures; the two `runner_protocol` CLI tests
completed in 181.31 seconds (approximately 181 seconds). Test-only metadata
expectations now refer to the current v5 format; no production contract changed.
The opt-in
MySQL/MariaDB recovery
tests for death before intent, after intent/before SQL, after SQL/before ACK,
after ACK/before finalization, and same-namespace/shared-database contention
were each run twice on disposable loopback MySQL 8.4.11/8.4.13/9.7.4 and
MariaDB 11.8.2/11.8.9/13.0.2. Bootstrap and metadata-upgrade fault journeys
also passed twice on those six versions. The new MySQL/MariaDB v4→v5 partial-DDL upgrade failure
tests passed twice each on MySQL 8.4.11/8.4.13/9.7.4 and MariaDB
11.8.2/11.8.9/13.0.2; the PostgreSQL
v4→v5 rollback-after-prior-DDL test passed twice each on 16.15 and 18.6. Those
upgrade results, along with the bootstrap and rollback
boundaries, are detailed in [`recovery-drills.md`](recovery-drills.md). The SQLite
initial-bootstrap failure and rollback-before-failed-run-crash tests passed
twice; PostgreSQL/MySQL/MariaDB source-revalidation tests were also run twice;
server runs used fresh rootless containers published only on loopback, and the
MySQL-family source-revalidation tests explicitly reconciled their confirmed
no-DDL outcomes. The latest SBOM validated as CycloneDX 1.3 with 238 components.
Root and fuzz dependency audits used 1,422
RustSec advisories and covered 300 and 244 locked packages respectively. The
dependency policy checks passed; `cargo deny` emitted non-failing duplicate
dependency-version and unused-license-allowance warnings. These local checks do
not complete the twelve-scenario cross-driver matrix, the external security
validations, or the CI-reference-hardware performance objective; G11 remains
pending.

**Additional complete opt-in and TLS validation — 2026-11-07:** four full
`cargo test --workspace --all-features --locked -- --ignored` runs passed 45
tests apiece (180 opt-in test executions total) against fresh loopback
databases. Combinations were PostgreSQL 16.15 + MySQL 8.4.13 + MariaDB 11.8.9;
PostgreSQL 18.6 + MySQL 8.4.13 + MariaDB 11.8.9; PostgreSQL 16.15 + MySQL
8.4.11 + MariaDB 11.8.2; and PostgreSQL 18.6 + MySQL 9.7.4 + MariaDB 13.0.2.
Three TLS opt-ins also passed using ephemeral certificates generated inside
disposable containers: PostgreSQL 16.15, MySQL 8.4.13, and MariaDB 11.8.9. The
fourth excluded opt-in requires PgBouncer; neither a local binary nor image was
present, and no image was downloaded. `recovery-drills.md` records the
per-scenario implications. This evidence does not resolve blockers #11/#12 or
external G11 requirements.

**PgBouncer opt-in — 2026-11-08:** the ignored transaction-pooling regression
`database::postgres::tests::local_pgbouncer_backend_change_is_rejected_after_session_lock_acquisition`
passed twice each on disposable PostgreSQL 16.15 and 18.6. PgBouncer 1.26.0 was
built from `lucaswilliameufrasio/pgbouncer-docker` commit
`04dc81d4ab0a191bdc90e01d2b6c882c8c5b620f`, using the locally cached
`debian:bookworm-slim` base via `DEBIAN_CODENAME=bookworm`; no Docker images were
pulled. The Dockerfile downloaded build packages and the checksum-verified
upstream source archive. The default Debian Trixie base was not locally
available, so the repository's default runtime base is not covered by this
test. Only PgBouncer was bound to loopback; PostgreSQL used the disposable
container network without a host port. See `recovery-drills.md` for setup and
the test's scope. G11 remains pending for its other matrix and external
requirements.

**Follow-up search for external security artifacts — 2026-11-08:** read-only
`gh` code searches across accessible user repositories for `needs_validation`
and `security assessment`, plus issue/PR searches on this repository, found only
documentation references and no standalone assessment report or four structured
candidate records. This does not establish that they are absent from other
storage. Their content and status remain unverified until their owner provides
an accessible path or copy.

**Provenance clarification — 2026-11-08:** the project owner did not recognize
the separate report described by the historical documentation. The statement
that four candidate records exist is therefore retained only as an
uncorroborated historical reference, not as evidence that findings were
confirmed or that the owner has those records. No candidate was independently
validated here. The release checklist's requirement for an identifiable,
complete security review remains open; do not claim “no critical issues.”

**MySQL/MariaDB transaction-policy regression and full local gate — 2026-11-08:**
`source::tests::mysql_migrations_require_explicit_never_and_match_the_target_dialect`
now asserts that both an omitted policy and explicit `transaction=always` are
rejected during source discovery, while explicit `transaction=never` is
accepted. The focused test passed, and the all-features workspace suite passed
with 143 passed, 49 ignored, and zero failures. This supports classifying
rollback-before-failure-record as inapplicable to MySQL/MariaDB under ADR-016;
it does not close PostgreSQL 17.11 coverage or the other matrix gaps.

After that test change, formatting, `git diff --check`, Clippy, the full
all-features workspace tests, build, both documented SQLite examples, and
`sh scripts/supply-chain.sh` passed. The supply-chain check emitted only the
existing non-failing duplicate-version and unused-license-allowance warnings;
advisories, bans, licenses, and source checks passed. At that point the separate
security assessment and performance baseline were still outstanding. The later
owner decision made the existing engine-level check the accepted #12 criterion;
the subsequent remote baseline and remaining G11 blockers are recorded below.

**Additional MySQL-family version coverage — 2026-11-07:** the MySQL-focused
ignored workspace suite passed 14 tests each on disposable MySQL 8.4.11 and
9.7.4. The MariaDB-focused suite passed 12 MariaDB-specific tests on each of
11.8.2 and 13.0.2; the two direct metadata/legacy-adapter tests also passed on
each of those versions. Databases were newly created and servers published only
on loopback. A broad MySQL-name filter initially selected one MySQL-vendor-only
engine assertion on MariaDB; exact MariaDB-compatible filters passed. The
remaining G11 blockers are unchanged.

**G11 scope decisions and latest-stable PostgreSQL verification — 2026-11-08:**
the project owner selected the current stable PostgreSQL line instead of
requiring recovery scenario #11 on PostgreSQL 17.11. The official
[versioning page](https://www.postgresql.org/support/versioning/) lists 18.6 as
the current minor of the latest stable major; 19 is a release candidate. On a
disposable rootless-Docker PostgreSQL 18.6 container, with `--pull=never`, no
persistent volume, and only a loopback-published port,
`cli_process_death_at_postgres_recovery_boundaries_keeps_runs_unresolved` and
`engine::tests::local_postgres_rejects_source_changed_after_plan_before_sql`
each passed once. The container was removed. The project owner also accepted
the existing CLI contract for #12: `plan` is read-only, `up` recalculates, and
there is no reusable approved-plan CLI flow. Engine-level source revalidation
is the applicable criterion; no new CLI flow is required or authorized.

An informational performance job was added to GitHub Actions. It waits for the
quality and supply-chain jobs, builds the release CLI, records runner/toolchain
metadata, and runs the documented 1,000-migration offline validation benchmark
with 10 samples. Run `42528280279` for commit
`04d8f132da27f47d599b1e8dd1927e3882ce3bed` passed all three jobs. The performance
runner was `ubuntu24` image `20261101.360.1` (Linux x86_64, kernel
`7.0.0-1017-azure`, AMD EPYC 7763), with Rust/Cargo 1.99.1 and a release build.
After one warm-up, the ten `validate` samples over 1,000 generated migrations
were 15, 15, 15, 15, 15, 15, 17, 16, 15, and 15 ms (median 15 ms, max 17 ms).
The timing includes process startup; the harness checked the returned migration
count and verified that read-only validation created no SQLite database. This
campaign met the initial one-second objective, but the job is informational and
does not enforce a threshold; one run does not characterize variance across
runner images. See the [CI run](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42528280279).

The follow-up run `42529361917` on commit
`ced16a526db2809ef94b0d13a82d45b764b7e517` also passed quality, supply-chain,
and performance jobs and uploaded the commit-specific SBOM
`database-migrator-sbom-ced16a526db2809ef94b0d13a82d45b764b7e517` (artifact
metadata reports 35,986 bytes and 30-day retention). It used the same recorded
`ubuntu24` image, CPU model, kernel, and Rust/Cargo versions as the first run.
Its ten post-warm-up samples were all 14 ms (median and maximum 14 ms). Across
both independent hosted-runner campaigns, the combined median is 14.5 ms and
maximum 17 ms; the campaign medians differ by 1 ms. This strengthens
repeatability on the same runner image/hardware model, but does not establish
variance across images or a hard performance gate. See the [second CI run](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42529361917).

The historical security-review note above refers to a source-only review recorded at commit
`9a8dd95c3143fe9adc59f1c51fba820ad05db129`; its full report and four structured
`needs_validation` records are said to be maintained outside this repository
and were not available in the inspected checkout. Their candidate validations
remain unverified. G11 is still partial/pending.

**Focused SQLite error-disclosure review and local gate — 2026-11-08:** inspection
of locked `rusqlite 0.40.2` confirmed that `SqlInputError::Display` includes its
SQL input. The SQLite adapter's managed-statement error path could therefore
surface migration SQL through CLI errors; the sync engine path also formatted
the raw SQLite error. Both paths now emit only a generic rejection and safe
error classification. The async adapter reconstructs `SqlInputError` with a
generic diagnostic and `sql = "<redacted>"`, retaining the `DbmError::Database`
classification, SQLite error value and offset. A sentinel regression checks
sync/async returned errors and persisted history; it also asserts the async
database-error class. The namespace-collision CLI regression passed with its
existing `DATABASE_ERROR` and exit-code-10 contract. This is a focused review of
these SQLite error-reporting paths, not a complete security audit, and it does
not validate the inaccessible external assessment or its uncorroborated
candidate references. No claim of “no critical issues” is made.

The accompanying bounded source pass inspected `config.rs`/`source.rs` path
resolution, `runner.rs` process and frame handling, `ai.rs` context/provider
handling, CLI error serialization, and the PostgreSQL/MySQL connection setup.
It observed project-root checks for configured paths, migration-symlink
rejection, an allowlisted runner environment, a 4 MiB framed protocol, and
timeout handling. The runner remains trusted project code with the caller's
process permissions, not a sandbox; stopping the direct child does not establish
termination of any descendants. This is an explicit boundary in ADR-018 and the
threat model, so hostile runner binaries must not be treated as isolated. AI
requests are explicit, use a configured HTTPS (or loopback HTTP) endpoint, do
not follow redirects, and bound context/response sizes; the local redaction
heuristic is not a guarantee that arbitrary source text contains no secrets,
and selected migration content is sent to that configured provider. The database
adapters reject invalid connection settings without echoing connection values
and enforce TLS validation by default for remote targets, with narrowly scoped
local plaintext opt-ins. No additional confirmed defect was established in this
bounded read-through; these observations do not establish absence of other
issues, provide independent severity triage, or replace a complete review.

After the correction, the local gate passed `cargo fmt --all -- --check`,
Clippy with `-D warnings`, `cargo test --workspace --all-features --locked`
(144 passed, 49 ignored, zero failures), the all-features workspace build, both
documented SQLite examples, and `sh scripts/supply-chain.sh`. The supply-chain
script validated the generated CycloneDX BOM and both dependency graphs against
1,422 RustSec advisories; advisories, bans, licenses and sources passed. It
reported non-failing duplicate-version warnings and unused license allowances.
The full suite's ignored database integrations were not exercised by this
default run. G11 remains partial: the complete security review and remaining
adapter/version/failure-boundary matrix still require evidence, and the external
assessment/candidate records remain unavailable and uncorroborated.

**Remote CI for the SQLite fix — 2026-11-08:** push commit
`15b722ea5c871fae73ee40078bbeee28101fa62c` passed all three jobs in [workflow
run 42534997418](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42534997418):
quality (2m46s), supply-chain (6m05s), and the informational performance job
(2m33s). The quality job passed format, Clippy, the default workspace suite,
build, and both executable examples; its default suite left 49 opt-in database
tests ignored. Supply-chain passed audits and CycloneDX generation/upload; it
reported duplicate-version warnings, with no advisory, ban, license, or source
failure. The retained artifact is
`database-migrator-sbom-15b722ea5c871fae73ee40078bbeee28101fa62c`, 35,986 bytes,
SHA-256 `94d7bdbc2f6fe14bfd9734466bbc88e7b1dea6b7d921bf5e222caef46daad1a1`,
with 30-day retention.

The performance job used `ubuntu24` image `20261101.360.1` (Linux x86_64,
kernel `7.0.0-1017-azure`, AMD EPYC 7763), Rust/Cargo 1.99.1, and a release
binary. After one warm-up, the 10 read-only `validate` samples for 1,000
migrations were 17, 18, 18, 17, 17, 16, 17, 17, 17, and 17 ms (median 17 ms,
maximum 18 ms); the harness also checked the migration count and absence of a
created SQLite database. This satisfies the initial sub-second target on this
runner campaign, but remains informational, not a regression threshold, and
does not establish variance across runners. G11 remains partial: the complete
security review/external records and remaining applicable adapter/version/
failure-boundary evidence are still open.
