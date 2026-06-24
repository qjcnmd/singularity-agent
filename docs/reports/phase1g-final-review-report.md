# Phase 1G Resolution Report: Final Review Gate / FinalReportRenderer

Date: 2026-06-22

## Scope

Phase 1G completes the finalization loop for the current Python CLI component.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity already had:

- `ReviewPipeline.final_review()`.
- A structured planner `FinalReport`.
- Review observations in the planner evidence ledger.

The Phase 1G contract was still partial:

- `ReviewPipeline.final_review()` existed but was not forced in the completion path.
- A task could complete without a final review accept decision.
- Review decisions had action values but not the roadmap route names.
- Planner final reports were persisted as JSON only.
- No planner-level markdown report artifact was written during finalization.

## Plan

1. Add final review route names to existing review decisions.
2. Run final review inside `Planner.finalize()`.
3. Require final review approval before `COMPLETED`.
4. Add the smallest `FinalReportRenderer` that validates the existing report schema and writes markdown.
5. Record the markdown artifact path in planner events and trace.
6. Wire the production `ReviewPipeline` back into `Planner`.
7. Add regression tests for final review rejection and markdown artifact generation.

## Changes

- `src/singularity/review/models.py`
  - Adds the roadmap route field on `ReviewDecision`.
- `src/singularity/planner/finalizer.py`
  - Requires final review acceptance for completed status.
  - Adds `FinalReportRenderer` markdown rendering and schema validation.
- `src/singularity/planner/engine.py`
  - Runs final review before final report generation.
  - Writes `final_report.md`.
  - Records the artifact path in trace and planner events.
- `src/singularity/kernel/graph.py`
  - Wires the shared production `ReviewPipeline` into `Planner`.
- `docs/phase1g_final_review_report.md`
  - Documents implemented Phase 1G behavior.
- `docs/architecture/planning-and-run-control.md`
  - Updates finalization and persistence status.
- `tests/test_planner.py`
  - Adds final review rejection and markdown report coverage.
- `tests/test_agent_graph.py`
  - Verifies agent graph wiring.

## Verification

Targeted Phase 1G validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_planner.py tests\test_agent.py tests\test_agent_task_outcome.py tests\review tests\test_agent_graph.py tests\test_verification_runner.py --basetemp work/pytest-tmp
```

Result:

```text
81 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp-phase1g-final
```

Result:

```text
647 passed, 4 skipped, 1 warning
```

Note: the standard `--basetemp work\pytest-tmp` path was retried and failed before test execution for later tests with Windows `PermissionError: [WinError 5]` while pytest attempted to remove the existing temp root. The same full suite passed with a fresh basetemp directory.

Publish proof:

```powershell
git push origin main
git rev-list --left-right --count origin/main...HEAD
```

Result:

```text
b1a61a3 feat: require final review reports
origin/main...HEAD = 0 0
```

## Risks

- `FinalReportRenderer` intentionally reuses the existing planner `FinalReport` structure instead of creating a parallel report schema.
- Markdown rendering is deterministic and local. It does not add a model reporting step.
- Review model critic behavior is unchanged; the planner-created fallback review component disables model critic when no shared component is wired.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
