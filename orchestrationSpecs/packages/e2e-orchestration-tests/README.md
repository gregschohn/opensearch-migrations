# @opensearch-migrations/e2e-orchestration-tests

End-to-end orchestration test framework. See
`orchestrationSpecs/docs/workflowTesting/e2eOrchestrationTestFramework.md` for
design and `orchestrationSpecs/docs/workflowTesting/e2eOrchestrationImplementationPlan.md`
for implementation sequencing.

Current state: the package has the live runner, spec loading, matrix expansion,
transition-tree validation, compact/detail snapshots, coverage summaries, and
focused safe/gated/impossible case-plan support.

Useful local commands from `orchestrationSpecs`:

```bash
npm run -w @opensearch-migrations/e2e-orchestration-tests type-check
npm run -w @opensearch-migrations/e2e-orchestration-tests test -- --runInBand
npm run -w @opensearch-migrations/e2e-orchestration-tests generate-transition-trees
npm run -w @opensearch-migrations/e2e-orchestration-tests run -- tests/live-specs/fullMigrationProxySafe.test.yaml --list-cases
```
