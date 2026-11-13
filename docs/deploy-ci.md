# Safe deploy example

This is an example for a separate application repository. It is not installed
in this project's CI, and it does not deploy or migrate a database by itself.

The job runs a read-only plan, then applies that plan only when the operator
or the protected CI environment supplies both approval values from that plan.
One flag without the other is a configuration error. A generic force flag is
not part of the contract. Development and test configs reject the production
approval flags.

```yaml
name: migrate
on:
  workflow_dispatch:
jobs:
  migrate:
    runs-on: ubuntu-latest
    environment: production
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - name: Plan
        run: dbm plan
      - name: Apply the current plan
        env:
          DBM_APPROVE_PLAN: ${{ vars.DBM_APPROVE_PLAN }}
          DBM_APPROVE_TARGET: ${{ vars.DBM_APPROVE_TARGET }}
        run: |
          test -n "$DBM_APPROVE_PLAN"
          test -n "$DBM_APPROVE_TARGET"
          dbm up --approve-plan "$DBM_APPROVE_PLAN" --approve-target "$DBM_APPROVE_TARGET"
```

Copy the two hashes from the plan output into the protected environment before
dispatching the apply job. Do not store database credentials in the workflow
file. Rollback uses `dbm plan --down --steps N` and the distinct down approval
hash; an up approval does not authorize down.
