# ADR-022: Fuzz harness and supply-chain checks for F11

- **Status:** Aceita para a fatia interna F11; G11 continua pendente
- **Data:** 2026-10-26

## Contexto

F11 requires parser/scanner fuzzing, dependency review, an SBOM, executable
examples, and recovery evidence. The core crate currently keeps its SQL parser
module private, and its default feature set must remain free of fuzzing-only
dependencies. CI runs on GitHub Actions and already pins actions by full commit
SHA. The repository has no stable release process yet.

## Decision

- Keep libFuzzer and its nightly-only build outside the application dependency
  graph in a separate `fuzz/` package. The core crate exposes hidden parser and
  scanner wrappers only behind the opt-in `fuzzing-harness` feature; normal
  builds do not enable it.
- Use separate scanner and migration-parser targets with a small checked-in
  corpus covering quotes, comments, delimiters, Unicode, directives, and
  malformed section order. Fuzz execution is a sandboxed developer/release
  check, not an implicit migration or a default build action.
- Pin `cargo-audit`, `cargo-deny`, `cargo-cyclonedx`, and `cargo-fuzz` versions
  in the documented workflows. Do not add advisory exceptions without a
  reviewed, time-bounded rationale. Limit dependency registries to crates.io and
  enumerate accepted SPDX licenses.
- Generate a CycloneDX JSON SBOM for all application features in the GitHub
  Actions supply-chain job and retain it as a short-lived workflow artifact.
  This does not publish a release artifact or prove the resulting binaries were
  tested, signed, or attested.
- Add runnable examples and execute them in the ordinary quality job. Keep
  release preparation documents explicit that G11/G12 are pending until the
  underlying evidence and approvals exist.

## Alternatives considered

- **Add `libfuzzer-sys` to default or ordinary dev-dependencies:** rejected
  because the compiler/runtime is fuzz-specific and would enlarge or complicate
  normal builds.
- **Make the internal migration parser a stable public module:** rejected
  because fuzzing does not justify expanding the provisional public API.
- **Commit an SBOM snapshot as if it were a release BOM:** rejected because it
  can drift from a later source commit and would imply release provenance that
  does not exist.
- **Use a third-party audit action:** rejected for this increment in favor of
  pinned CLI versions and explicit commands; the only added action uploads the
  generated artifact.

## Consequences and evidence

- The fuzz package needs its own checked-in lockfile, nightly compiler, and
  LLVM sanitizer/C++ toolchain; fuzz findings and corpus changes must be
  triaged before G11 can pass. The package's transitive dependency graph is not
  yet included in the root dependency audit.
- CI uses the latest pinned stable toolchain for ordinary builds; the fuzz
  toolchain is separate and its exact revision must be recorded with any run.
- SBOMs are commit-specific CI artifacts with 30-day retention and are not
  checksums, attestations, signatures, or public release assets.
- On 2026-10-26, upstream versions checked were `cargo-fuzz` 0.13.2,
  `cargo-audit` 0.22.2, `cargo-deny` 0.20.2, and `cargo-cyclonedx` 0.5.9.
  `actions/checkout` 7.0.1 and `actions/upload-artifact` 7.0.2 were checked
  against canonical releases and security pages; full commit SHAs are pinned in
  CI.
- Fuzzing and a full security review have not yet been performed in the current
  environment. This ADR records the implementation choice, not G11 approval.

## Revisit when

Revisit if the parser becomes a supported public API, fuzzing moves to a
dedicated isolated CI runner, a release pipeline requires durable SBOM
attestation, or a new policy requires broader license/source controls.
