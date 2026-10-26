# Planned V1 release checklist — not approved

This is a preparation checklist only. It does not authorize creating a tag,
publishing a package, uploading release assets, or running database migrations.

## Current gate status

- [x] Draft changelog, upgrade guide, support matrix, and release checklist
      prepared.
- [x] CI configuration for dependency audit and ephemeral CycloneDX SBOM
      artifact added.
- [ ] G11 security review, fuzz evidence, complete fault-injection matrix,
      dependency audit result, and executable example evidence all reviewed.
- [ ] G12 final checklist completed and explicitly approved by the release owner.
- [ ] Product/package/bin names, SemVer policy, public API stability, license,
      and MSRV are no longer provisional.

## Required before a release decision

- [ ] Complete the source security review and independently validate any
      findings; report unvalidated boundaries rather than asserting “no critical
      issues.”
- [ ] Run both fuzz targets for a recorded duration with the documented
      resource-limited, network-disabled setup; retain corpus/crash triage and
      toolchain/version evidence. Generate and commit `fuzz/Cargo.lock`, then
      include that graph in the dependency audit.
- [ ] Execute and record the full applicable kill/disconnect, concurrency,
      metadata bootstrap/upgrade, rollback, namespace, and source-change matrix
      from section 13.3 of the development plan. Demonstrate fail-closed
      recovery; do not use production databases.
- [ ] Run `cargo audit` against current RustSec data and `cargo deny --all-features
      check`; resolve findings without blanket ignores. Preserve the CI result and
      the SBOM generated from the same source commit.
- [ ] Build and execute all documented examples; verify every README command
      and recovery/upgrade instruction against the final CLI.
- [ ] Revalidate the exact server, operating-system, Rust/MSRV, feature, TLS,
      and proxy support matrix. Add platforms only with test evidence.
- [ ] Review the complete changelog, upgrade guide, migration/rollback behavior,
      signing/checksum approach, and artifact provenance.
- [ ] Produce and verify release binaries for every declared platform; capture
      checksums and attestations/signatures where supported.
- [ ] Demonstrate `doctor`, explicit reconciliation, metadata upgrade, and
      rollback/recovery on disposable isolated databases matching the published
      support matrix.
- [ ] Obtain explicit release-owner approval for the final release contents and
      publication boundary.

## Prohibited until separately authorized

- Do not create or push a release tag.
- Do not publish a crate or upload public release assets.
- Do not execute metadata upgrades or migrations on production databases.
