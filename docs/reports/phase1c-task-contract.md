# Phase 1C Resolution Report: TaskContract / Requirement Extraction

Date: 2026-06-22

## Scope

Phase 1C adds structured task contracts and requirement extraction for the current Python planner path.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity had planner completion criteria and evidence ledgers, but did not have the Phase 1C contract surface:

- No `TaskContractBuilder`.
- No `TaskContract`, `AcceptanceCriterion`, `Deliverable`, `Constraint`, `VerificationRequirement`, `ReportRequirement`, or `EvidenceRequirement` types.
- No contract-derived smoke command accessor.
- No per-contract criterion status in completion assessment.
- No contract summary in planner context.

Existing behavior already enforced that model final text is not evidence, so this phase reused the planner evidence ledger instead of adding a second evidence system.

## Plan

1. Add a small contract schema and rule fallback builder.
2. Support model structured output by validating a structured payload through the same schema.
3. Generate smoke commands from verification requirements.
4. Store the contract on `TaskState` and render it into planner context.
5. Extend completion assessment with per-criterion satisfied/missing status.
6. Add tests for create-file tasks, report obligations, structured output, and missing smoke evidence.

## Changes

- `src/singularity/planner/contract.py`
  - Added Phase 1C contract types and `TaskContractBuilder`.
- `src/singularity/planner/runtime.py`
  - Builds and stores a contract in `start_task()`.
  - Adds `contract_smoke_commands()`.
  - Adds contract criterion status to `assess_completion()`.
- `src/singularity/planner/models.py`
  - Persists `TaskState.task_contract`.
- `src/singularity/planner/context.py`
  - Renders contract summary into planner context.
- `src/singularity/planner/__init__.py`
  - Exports contract types.
- `tests/test_planner_runtime.py`
  - Adds Phase 1C coverage for requirement extraction and criterion gating.
- `docs/phase1c_task_contract.md`
  - Documents contract behavior and boundaries.

## Verification

Targeted Phase 1C validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_planner_runtime.py tests\test_verification_runtime.py tests\test_agent.py tests\test_agent_task_outcome.py tests\test_context_policy_planner_integration.py --basetemp work/pytest-tmp
```

Result:

```text
50 passed
```

Whitespace validation:

```powershell
git diff --check
```

Result: passed. Git reported CRLF normalization warnings for touched text files only.

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
629 passed, 4 skipped
```

Remote alignment is recorded after publish for this phase.

## Risks

- The production extraction path is a deterministic rules fallback. Model-based extraction is supported as a structured payload input, not as a new model call.
- Generic chat does not receive artificial required criteria; this preserves existing casual-answer behavior.
- Full final report generation remains a later Phase 1G concern. Phase 1C records report obligations only.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
