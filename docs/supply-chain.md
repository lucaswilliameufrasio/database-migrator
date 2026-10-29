# Dependency audit, fuzzing, and SBOM

This document records the current hardening setup. It is not a completed
security audit or a release approval.

## Dependency policy

The root `Cargo.lock` is the dependency inventory for the application. The
GitHub Actions `supply-chain` job runs:

```sh
cargo audit
cargo deny --all-features check
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
The separate fuzz package does not yet have a checked-in `fuzz/Cargo.lock`, so
its transitive tool dependency graph is not covered by these root-lock scans.
Generate and review that lock, then add its audit to the supply-chain job before
claiming the full F11 dependency gate.

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
dependency scan. G11 remains pending until the unresolved validation, fuzzing,
dependency, SBOM, and required failure-matrix evidence is recorded.

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
dependency policy was not weakened. Full target sandbox/file handling, fuzzing,
and the 12-scenario failure matrix remain unverified, so G11 is still pending.
