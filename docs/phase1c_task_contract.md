# Phase 1C Task Contract

Phase 1C adds a lightweight `TaskContract` that turns user goals into explicit completion requirements without creating a parallel task controller.

## Types

- `TaskContract`
- `AcceptanceCriterion`
- `Deliverable`
- `Constraint`
- `VerificationRequirement`
- `ReportRequirement`
- `EvidenceRequirement`

## Builder

`TaskContractBuilder` supports two inputs:

- rules fallback from the normalized user goal
- model structured output passed as a validated payload

The current production path uses rules fallback. A future model extractor can return the same schema and pass it to `TaskContractBuilder.build(..., structured_output=payload)`.

## Planner Integration

`Planner.start_task()` builds a contract and stores it in `TaskState.task_contract`.

The planner context includes a compact contract summary:

- acceptance criteria
- deliverables
- verification requirements
- report requirements
- evidence requirements

`Planner.assess_completion()` returns per-criterion status:

```text
criteria.<criterion_id>.satisfied
criteria.<criterion_id>.missing_evidence
```

Required contract criteria are added to `unmet` as `contract:<criterion_id>` when evidence is missing.

## Smoke Commands

`TaskContract.smoke_commands()` returns verification commands derived from explicit verification requirements. For example, a create-and-run Python file task produces:

```text
python quicksort.py
```

Verification still runs through `VerificationRunner` and `CommandExecutor`.

## Boundaries

- Model final text is not evidence.
- Generic chat does not receive artificial required criteria.
- Contract extraction does not bypass planner, mutation, verification, policy, or trace.
- The contract records report obligations, but full report generation remains owned by later final-report component work.
