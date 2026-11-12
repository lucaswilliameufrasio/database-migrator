# Planned V1 release checklist — not approved

This is a preparation checklist only. It does not authorize creating a tag,
publishing a package, uploading release assets, or running database migrations.

## Current gate status

- The external source-first review is now available at
  `/home/mimiron/security-audit-skill/database-migrator/run-1/REPORT.md` for
  baseline `3822695f15d5dbcbdc38872fb349b640ab2844dd`; its `SHA256SUMS` verified.
  It records 7 findings (1 confirmed low, 4 `needs_validation`, 2 rejected) and
  18 coverage units. The four leads have bounded local mitigations documented
  for `4cca4dc5c330fb6700771c26578304f52deb3ea3`; their historical verdicts are
  not rewritten. On 2026-11-11 the official validators passed again (7 findings,
  18 units). A separate follow-up now rejects
  PostgreSQL `U&"…"` identifiers while preserving literal Unicode names such as
  `"café"`; its decision is in ADR-025 and verification is in the follow-up
  below. The historical finding/verdict is unchanged. This
  evidence does not establish that no critical issues exist; G11 remains OPEN.
- A focused review on 2026-11-08 found and fixed SQLite migration-SQL disclosure
  through `SqlInputError` formatting. Sync/async redaction and error-class
  regressions passed locally. A bounded read-through also inspected path,
  runner, AI/provider, CLI error, and database-connection boundaries; it
  reaffirmed that compiled runners are trusted process-level code, not a
  sandbox. Follow-up testing confirmed an AI-context TOCTOU: before the fix, a
  synthetic external sentinel appeared in 21 of 60 successful previews during
  concurrent path replacement. Context files now open component-by-component
  without following symlinks and are read from the opened handle. Deterministic
  Unix regressions cover final external/internal symlinks and an intermediate
  symlink. The post-fix stress reproduction had no sentinel reads, but all 160
  previews failed closed; normal preview/fake-provider CLI tests passed
  separately. No real provider, credentials, or network were used, and no formal
  severity is assigned. At that stage, this focused correction did not address
  the separate PostgreSQL finding in the now-available external report; a later
  working-tree follow-up is recorded below. Other release gates remain open.
- A follow-up path-race regression on 2026-11-08 reproduced that SQLite could
  follow a final-component database symlink swapped after configuration
  validation. Read-only and mutable opens now use `SQLITE_OPEN_NOFOLLOW`; the
  regression failed before and passed after the fix. The protection does not
  cover ancestor-directory replacement or migration-file races, and this focused
  fix does not resolve the external report's separate PostgreSQL finding or the
  remaining G11 release gates.
- The current local checkout passed the all-features gate on 2026-11-09:
  formatting, `git diff --check`, Clippy, 147 workspace tests (49 opt-in tests
  ignored), build, both SQLite examples, and dependency/SBOM checks. The
  all-features workspace tests also passed on Rust 1.89.0 (147 passed, 49
  ignored); the complete gate ran on Rust 1.99.1. Disposable
  server runs passed on PostgreSQL 17.11 and MariaDB 10.11.14/11.4.13; the 45-test
  opt-in suite passed on PostgreSQL 17.11/MySQL 8.4.13/MariaDB 11.8.9 and on each
  older MariaDB line paired with PostgreSQL 18.6/MySQL 8.4.13. TLS cases were
  separately verified where recorded in `docs/recovery-drills.md`. On the current
  checkout, the 45-test opt-in selection also passed serially with PostgreSQL
  18.6/MySQL 8.4.13 and each MariaDB 10.11.14, 11.4.13, and 11.8.9; four TLS/
  PgBouncer tests were excluded and their separate evidence remains recorded.
  The default-parallel attempt failed in library tests and its cause is not
  isolated. These are local results. Corrected harness commit `8d47569` passed
  remote CI run
  `42548111248`; that run predates the current AI-context correction and does not
  verify it. The subsequent AI-context commit `0805472` failed quality in CI run
  `42553905101` because the SQLite #11 harness did not observe rollback within
  45 seconds; supply-chain passed and performance was skipped. The test workload
  was reduced and passed six focused local runs plus the full local gate. Follow-up
  commit `894d984` passed all three CI jobs in run `42555474213` (quality,
  dependency audit/SBOM, and informational performance). However, the later
  documentation commit `655f7ec` failed the SQLite #11 test in CI run
  `42560317968`: rollback was observed, but the CLI exited before the harness
  captured failed-run persistence. The audit/SBOM job passed; performance did not
  run. The correction in `6d613c2` writes a sentinel before the trigger's slow
  CTE; it passed six focused local runs, the full local gate, and the
  all-features workspace tests on Rust 1.89.0. Commit `6d613c2` passed all three
  CI jobs in run `42562827192`, verifying the corrected harness on that SHA.
  This does not resolve the confirmed PostgreSQL finding or remaining matrix
  gaps; G11 stays open.
- Commit `15b722ea5c871fae73ee40078bbeee28101fa62c` passed quality,
  supply-chain/SBOM, and performance in CI run
  [42534997418](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42534997418).
  This verifies those gates for that SHA; it does not close the complete security
  review or remaining fault-injection matrix, so the combined G11 checkbox stays
  open.
- Commit `de529ce0a9e0b025a99ffc766cb8cea9796a740f` passed all three jobs in CI
  run [42538270662](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42538270662),
  including the widened SQLite #11 test, dependency audit/SBOM, and informational
  performance baseline. This closes remote verification for that test-only
  adjustment; it does not close the security review or the remaining matrix rows.
- Commit `53b12921b0af8691a5ab17fa65ccf8d5d741b2a4` did not pass CI run
  [42543796008](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42543796008):
  quality failed in the SQLite #11 observation-window test, supply-chain passed,
  and performance did not run because its dependencies failed. A local harness
  correction now holds a SQLite read transaction after rollback; its focused test
  passed six times and the local gate at that stage passed (145 passed, 49
  ignored, zero failures). Corrected commit `8d47569`
  subsequently passed all three CI jobs in run
  [42548111248](https://github.com/lucaswilliameufrasio/database-migrator/actions/runs/42548111248).
  The earlier green CI for `de529ce` remains evidence for its older harness, not
  this read-lock correction.
- **Current local reconciliation — 2026-11-11:** on commit
  `4cca4dc5c330fb6700771c26578304f52deb3ea3`, the root and fuzz dependency graphs
  passed offline `cargo audit` (311 and 260 locked packages) and `cargo deny`
  (advisory snapshot commit `7a40350e1b8ba5b656d909f88222a8059a83d88d`, dated
  2026-11-08; its fetch marker is 2026-11-09). This snapshot is not current
  enough to claim a 2026-11-11 online advisory check. The CycloneDX 1.3 SBOM
  validated with 253 components. `fuzz/Cargo.lock` was stale after the root
  manifest made `cap-std` and `tempfile` non-optional; it is now regenerated and
  verified locally and is included in this follow-up. The 2026-11-07 fuzz
  campaigns predate the PostgreSQL identifier-guard change and do not cover it.
  `sql_scanner` harness now calls both PostgreSQL migration validators and has a
  seed for the escaped metadata identifier. On the current working tree,
  formatting, focused guard tests, Clippy, all workspace tests (155 passed, 49
  ignored), build, and both examples passed in a rootless, network-disabled
  sandbox. The first fuzz build hit the 4 GiB container memory limit with two
  Cargo jobs; a one-job retry then built both targets and completed 301-second
  campaigns without crash artifacts, retaining both generated corpora. Exact
  counts, hashes, sandbox controls, and logs are recorded in the follow-up below.
  Offline audit/deny checks also covered the regenerated fuzz lock, using the
  advisory snapshot described in `docs/supply-chain.md`.
  Duplicate-version and unused-license-allowance warnings remain non-failing.
  G11 remains OPEN; no release was made or approved.
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
