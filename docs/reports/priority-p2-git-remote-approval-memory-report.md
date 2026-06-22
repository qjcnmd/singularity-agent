# Priority P2 Resolution Report: Git Runtime, Remote Approval, Memory Sync

Date: 2026-06-22

## Scope

P2 covered three previously planned capabilities:

- Local `GitRuntime`.
- File-backed remote approval exchange.
- File-backed remote memory synchronization.

Web search and multi-agent execution remain explicitly out of scope for this goal.

## Verification Before Fix

New failing tests confirmed the gap:

- `tests/test_git_runtime.py` failed because `singularity.git_runtime` did not exist.
- `tests/test_remote_approval.py` failed because `singularity.policy.remote` did not exist.
- `tests/memory/test_sync.py` failed because `singularity.memory.sync` did not exist.

## Implementation

Implemented local Git control-plane support:

- Added `src/singularity/git_runtime/`.
- Added `GitRuntime.status()`, `GitRuntime.diff_stat()`, and `GitRuntime.commit()`.
- Added CLI commands:
  - `singularity-agent git status --json`
  - `singularity-agent git diff --json`
  - `singularity-agent git diff --staged --json`
  - `singularity-agent git commit --message ... --path ... --json`
- Kept push, pull, PR, reset, and branch automation out of scope.

Implemented file-backed remote approval:

- Added `src/singularity/policy/remote.py`.
- Added request export for `PolicyRequest` / `PolicyDecision` pairs.
- Added scoped `ApprovalGrant` import and registration through `PolicyRuntime`.
- Added CLI commands:
  - `singularity-agent approval remote export-request ...`
  - `singularity-agent approval remote import-grant ...`
- Kept remote servers, polling, and model-authored approval out of scope.

Implemented file-backed memory sync:

- Added `src/singularity/memory/sync.py`.
- Added JSON bundle export/import with digest validation.
- Imported remote active entries as reviewable candidates by default.
- Added CLI commands:
  - `singularity-agent memory sync export ...`
  - `singularity-agent memory sync import ...`
- Added explicit `--trust-entries` for direct active-entry import.

Fixed one related CLI defect:

- `memory --json` output now uses raw `typer.echo()` instead of Rich console rendering, so long JSON strings remain parseable.

## Documentation

Updated:

- `README.md`
- `docs/architecture/runtime-map.md`
- `docs/architecture/policy-approval-runtime.md`
- `docs/architecture/local-workspace-state-runtime.md`
- `docs/architecture/migration-to-desktop.md`
- `docs/adr/0001-local-first-agent.md`
- `docs/RELEASE_RUNTIME.md`

The docs now state that these capabilities are implemented with local/file-backed boundaries, while web search, multi-agent execution, and the parallel executor remain planned at this point.

## Validation

Command:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_git_runtime.py tests\test_remote_approval.py tests\memory\test_sync.py tests\memory\test_runtime_cli.py tests\test_cli.py tests\test_docs_consistency.py tests\test_production_baseline_alignment.py --basetemp work\pytest-tmp-p2-focused
```

Result:

```text
47 passed
```

## Residual Risks

- `GitRuntime` is intentionally local-only and does not automate publish workflows.
- Remote approval and memory sync are operator-mediated JSON exchanges, not collaboration services.
- Remote memory imports default to candidates to avoid silently trusting external memory content.
