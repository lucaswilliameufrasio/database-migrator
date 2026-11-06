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
implied. The exact fuzz corpus was not retained, and no new GitHub Actions run
has verified these changes.

Remaining G11 evidence includes the twelve-scenario matrix, the four external
candidate validations, a performance baseline, review/triage of the separate
security assessment, and repeatable target-output/corpus retention evidence.
The bounded fuzz runs establish only that those inputs did not produce a
reported crash during the recorded runs.

**Final local supply-chain check 2026-11-06:** `sh scripts/supply-chain.sh`
passed again with the pinned audit tools and refreshed RustSec data (1,422
advisories): root lock 300 packages and fuzz lock 244 packages; both audit and
deny graphs passed. The regenerated application BOM validated as CycloneDX 1.3
with 238 components. Log: `attempt-02/logs/supply-chain-final.log`. This is local
evidence; the push-triggered GitHub Actions checks are not yet observed.
