# JSON Schemas

These Draft 2020-12 schemas describe the strict proposal/review/explanation
objects accepted by the optional `ai` feature:

- [`migration-proposal.schema.json`](migration-proposal.schema.json)
- [`review-report.schema.json`](review-report.schema.json)
- [`explain-report.schema.json`](explain-report.schema.json)
- [`suggested-down-report.schema.json`](suggested-down-report.schema.json)

The Rust deserializers reject unknown fields and apply additional semantic
checks, including dialect matching, safe proposal paths, local migration parser
validation, and configured execution-policy checks. Schema validation alone is
not evidence that SQL is correct, safe, or approved. These objects are
untrusted suggestions; the CLI never writes proposal files or applies them.
