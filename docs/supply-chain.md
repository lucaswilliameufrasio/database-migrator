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

Fuzzing has not yet been executed in the current isolated environment because
`cargo-fuzz`, `libfuzzer-sys`, and a nightly toolchain are not available in its
offline cache. The targets and seed corpora are prepared, but this is not fuzz
coverage evidence and does not close G11.

## Security review status

No complete source security audit has been performed for this repository. The
dependency checks above are not a substitute for reviewing trust boundaries,
credential handling, file/path boundaries, database adapters, runner execution,
or AI proposal handling. Do not infer “no critical issues” from a green
dependency scan; G11 remains pending until the required review and validation
are recorded.
