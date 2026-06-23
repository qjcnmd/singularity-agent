# Phase P2 Resolution Report

Date: 2026-06-23
Repository: `C:\Users\Lenovo\Desktop\Harness`
Batch: P2 production-readiness fixes

## Summary

P2 items are fixed where a small production-safe change was available. The only deferred item is the large-file split request: the current batch documents a concrete follow-up plan instead of doing a risky mechanical split without a behavior change.

## Issue Status

| Priority | Issue | Status | Evidence |
| --- | --- | --- | --- |
| P2-1 | CLI subcommands relied on `Path.cwd()` instead of a consistent project root. | Fixed | `src/singularity/cli_paths.py` now centralizes `resolve_project_root()`. Main CLI, git, memory, plugin, and approval/policy subcommands accept `--project-root` and pass the resolved root into their runtimes. |
| P2-2 | `ProjectIndexRuntime` could initialize `.singularity/index.sqlite` even when disabled. | Fixed | `src/singularity/code_index/runtime.py` now lazily creates the store and returns empty disabled summaries without touching the DB. `tests/code_index/test_store_incremental_query_impact.py` asserts disabled bootstrap, health, and read paths do not create `.singularity/index.sqlite`. |
| P2-3 | `OpenAICompatibleModelProvider` did not expose streaming support while runtimes had a streaming fallback path. | Fixed | `src/singularity/model/providers.py` advertises streaming support and implements a complete-as-stream fallback that emits text/tool/usage/completed events. `tests/test_model_provider_registry.py` covers the streaming capability and event sequence. |
| P2-4 | `ParallelToolExecutor` waited on futures in submission order, delaying already completed later tasks. | Fixed | `src/singularity/tool_protocol/parallel.py` now collects futures with `as_completed()` while preserving input-order output. `tests/test_tool_protocol_parallel.py` covers the out-of-order completion case. |
| P2-5 | README pointed `PolicyRuntime` at the wrong source path. | Fixed | `README.md` now points to `src/singularity/policy/engine.py` and `src/singularity/policy/approval.py`. |
| P2-6 | GitRuntime documentation still described behavior that no longer matched implementation. | Fixed | `README.md`, `docs/architecture/command-runtime.md`, and `src/singularity/git_runtime/runtime.py` now describe the local-only wrapper that invokes the configured git executable directly and never runs user-provided shell command strings. |
| P2-7 | `pyproject.toml` lacked production-grade lint/type/coverage gates. | Fixed | `pyproject.toml` now declares `ruff`, `mypy`, and `pytest-cov` development dependencies plus focused Ruff, mypy, and coverage configuration. |
| P2-8 | Several core files exceed 1000 lines. | Deferred with plan | No low-risk split was needed to fix P2 behavior. Follow-up split order: move live eval helpers out of `src/singularity/cli.py`; split `ProjectIndexRuntime` orchestration from store/query models only after index behavior stabilizes; split large runtime files by existing runtime boundaries with characterization tests first. |

## Validation

Commands run during the P2 batch:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_cli.py tests\test_git_runtime.py tests\test_model_provider_registry.py tests\test_tool_protocol_parallel.py tests\code_index\test_store_incremental_query_impact.py --basetemp work\pytest-tmp-p2-targeted
```

Result: `31 passed, 1 skipped`.

```powershell
.\.venv\Scripts\python.exe -m ruff check .
```

Result: passed.

```powershell
.\.venv\Scripts\python.exe -m mypy
```

Result: passed.

```powershell
.\.venv\Scripts\python.exe -m compileall -q src tests
```

Result: passed.

```powershell
.\.venv\Scripts\python.exe -m singularity.cli eval task validate docs\evaluation\phase1j-golden-tasks.json --json
```

Result: validated 10 tasks.

```powershell
.\.venv\Scripts\python.exe -m singularity.cli git status --project-root . --json
```

Result: passed.

```powershell
.\.venv\Scripts\python.exe -m singularity.cli index explain --project-root work\p2-cli-smoke-project --json
```

Result: passed. The same command against the full repository correctly hit the configured index budget, so the smoke test used a bounded project fixture.

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp
```

Result: `690 passed, 5 skipped`.

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_cli.py -k live_quicksort_real_provider_opt_in --basetemp work\pytest-tmp-live-p2-final --tb=short
```

Result: `1 passed, 15 deselected in 128.12s`.

The live-provider test used `SINGULARITY_API_KEY`, `SINGULARITY_BASE_URL`, and `SINGULARITY_MODEL` loaded from the local desktop `key.txt` into the local environment. No secret values are stored in this report, source, or tests; the test asserts the result payload does not contain the key.

```powershell
git diff --check
```

Result: passed; Git reported only Windows LF-to-CRLF working-copy warnings.

## Remaining Risk

- OpenAI-compatible streaming is a complete-as-stream fallback, not token-level server streaming. It provides the runtime capability contract without claiming provider-side incremental token delivery.
- Large-file splitting remains intentionally deferred. The next split should be characterization-test-first and behavior-neutral, otherwise it risks hiding production fixes inside a broad refactor.
- The live-provider integration remains opt-in to avoid accidental network/model calls or key exposure. It should be run with the local environment variables before production releases.
