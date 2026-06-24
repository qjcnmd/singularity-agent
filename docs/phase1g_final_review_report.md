# Phase 1G Final Review Gate And Final Report

Phase 1G makes finalization evidence-gated instead of trusting a model final answer.

## Implemented

- `ReviewPipeline.final_review()` is now part of `Planner.finalize()`.
- A task cannot become `completed` unless the latest final review decision is `accept`.
- Review decisions expose a phase route:
  - `approve`
  - `repair`
  - `replan`
  - `ask_user`
  - `blocked`
- `FinalReportRenderer` renders the existing structured `FinalReport` into a markdown artifact.
- Planner finalization records the markdown artifact path in planner events and trace.

## Final Report Artifact

Final reports are written under:

```text
.singularity/planner/<session_id>/final_report.json
.singularity/planner/<session_id>/final_report.md
```

The markdown report includes:

- user goal
- task requirements
- rolling plan
- changed files
- verification status and checks
- failure and repair history
- final review decision and route
- evidence appendix

## Boundaries

- No Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future work.
- No new model-only reporting path.
- No new dependency.
- Existing JSON `FinalReport` remains the structured source; markdown is the human artifact.
