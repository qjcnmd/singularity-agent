# Phase 1B Resolution Report: Low-Entropy Execution Tools

Date: 2026-06-22

## Scope

Phase 1B covers the model-facing low-entropy execution tools:

- `write_file`
- `apply_patch`
- `inspect_diff`

This phase stayed inside the existing Python runtime. It did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Most Phase 1B behavior was already implemented before this loop:

- `write_file`, `apply_patch`, and `inspect_diff` were already registered by `src/singularity/tools/edit.py`.
- Writes already delegated through `EditRuntime` and `MutationRuntime`.
- `MutationRuntime` already enforced workspace path resolution, mutation policy, policy runtime decisions, journal entries, trace events, and workspace-state updates.
- `inspect_diff` already read the in-process mutation ledger without applying mutations.
- Planner phase policy already exposed the facades while hiding low-level mutation tools.
- Existing golden coverage already created `quicksort.py` and verified it with a smoke command.
- Existing tests already covered workspace escape rejection.

The remaining gap was schema drift against the roadmap wording: the runtime used equivalent names (`mode`, `patch`, `paths`) rather than the checklist names (`create_dirs`, `overwrite_policy`, `unified_diff`, `strict`, `scope=file`, `path`).

## Plan

1. Keep the existing runtime path instead of introducing a parallel file-editing path.
2. Add checklist-compatible input fields as thin aliases.
3. Preserve existing field names for backward compatibility.
4. Add regression coverage for the checklist schema names and file-scoped diff inspection.
5. Update the Phase 1B tool documentation.

## Changes

- `src/singularity/tools/edit.py`
  - Added `write_file.create_dirs` and `write_file.overwrite_policy`.
  - Added `apply_patch.unified_diff` and `apply_patch.strict`.
  - Added `inspect_diff(scope="file", path=...)` support.
- `src/singularity/edit/runtime.py`
  - Added `create_dirs=False` behavior for `write_file`; missing parent directories now fail unless explicitly allowed.
- `tests/test_execution_primitives_phase1b.py`
  - Added coverage for checklist-compatible schemas and file-scoped diff inspection.
- `docs/phase1b_execution_primitives.md`
  - Updated tool schema documentation.

## Verification

Targeted Phase 1B validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_execution_primitives_phase1b.py tests\test_tool_runtime.py tests\test_tools.py tests\test_planner_runtime.py --basetemp work/pytest-tmp
```

Result:

```text
44 passed
```

Whitespace validation:

```powershell
git diff --check
```

Result: passed. Git reported CRLF normalization warnings for touched text files only.

Full repository validation and remote alignment are recorded after final publish for this phase.

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
623 passed, 4 skipped
```

## Risks

- `apply_patch(strict=False)` is intentionally unsupported and fails closed. The current parser applies strict context matching only.
- The old `mode`, `patch`, and `paths` inputs remain supported to avoid breaking existing traces and tests.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
