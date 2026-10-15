# Original PostgreSQL obligation contract

Export the original scope through the native testing CLI:

```sh
harbor-db-test original-contract --source . \
  --baseline tests/pr14-baseline.toml --suite tests/suite.toml
```

The deterministic JSON is `harbor-db.original-obligation-contract.v1`. It retains
all 174 original PR14 method selectors, their source and exact original oracle
hashes, every corresponding registered catalog case, the 22 original Nix gate
obligations, and a hash of the full resolved catalog. `oracle_files` names the
actual root-relative original runtime and test files; reviewed extensions use
the retained original under `tests/oracles/pr14/` when current bytes differ.

Export runs the existing baseline and migration admission checks. Missing
original methods, removed gate enrollment, changed or absent original bytes,
unreviewed runtime extensions and escaping oracle paths reject. There is no
successful-subset filter and no count-only replacement for the original scope.

This is selection data for an independently frozen hosted consumer contract.
It performs no test execution or Nix evaluation. Output explicitly states
`scope = original-obligation-inventory`, `executed = false` and
`runtime_qualified = false`. Original case registration does not establish
execution, Python/native parity, producer qualification, consumer conformance
or live database acceptance.

For a hosted conformance lane, retain this export and its digest alongside the
independently chosen immutable producer/consumer cohort, archived locks, feature
and target selection, the original oracle files and actual executed method
results. Authenticate exact workflow, checkout, run/attempt, required jobs and
retained outputs through provider custody, then apply the shared coverage
verifier against independently selected requirements. Live recovery equality,
explicit thaw and application health remain separate acceptance gates.
