# Phase 1I Resolution Report: Docs / Config / Sandbox Honesty

Date: 2026-06-22

## Scope

Phase 1I aligns documentation, component naming, config precedence, effective config evidence, and sandbox capability claims with the implemented Python CLI component.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity already had:

- `SandboxManager` fail-closed behavior for requests that require unavailable hard isolation.
- Tests covering sandbox backend selection and unavailable isolation paths.
- Kernel and planner final report types, but they were not clearly separated in docs.

The Phase 1I contract was still missing or misleading:

- README and component map did not mark capabilities as `implemented`, `partial`, or `planned`.
- Architecture docs used `ContextSource` as the component name even though the implemented component class is `ContextManager`; `ContextSource` is a context-source enum.
- Planner `FinalReport` and kernel `FinalReport` responsibilities were conflated in docs.
- README documented config precedence, but `.singularity/config.toml`, effective config output, and source tracing were not implemented.
- `TaskState` did not persist a sandbox capability snapshot.
- Sandbox docs did not clearly distinguish Docker hard isolation from local copy-on-write staging.

## Plan

1. Keep the implementation inside the existing Python CLI component and documentation surfaces.
2. Add `.singularity/config.toml` support without introducing a new dependency.
3. Preserve precedence as explicit CLI flag > `SINGULARITY_*` env > `.singularity/config.toml` > defaults.
4. Emit effective config and config source evidence in trace and kernel final reports while keeping API keys environment-only.
5. Persist sandbox capability evidence in planner task state.
6. Make docs honest about implemented, partial, and planned capabilities.
7. Add regression tests for config precedence, config source tracing, docs consistency, sandbox capability state, and sandbox downgrade visibility.

## Changes

- `src/singularity/config.py`
  - Adds TOML config loading from `.singularity/config.toml`.
  - Adds env/config/default merging and source tracking.
  - Adds redacted `effective_config()` and `final_report_config_summary()`.
  - Keeps API key outside effective config output.
- `src/singularity/cli.py`
  - Uses Click parameter sources so Typer defaults do not mask config file values.
- `src/singularity/kernel/bootstrap.py`
  - Emits an effective config trace event with component `config`.
  - Uses source-aware config summaries in bootstrap failure reports.
- `src/singularity/kernel/agent_kernel.py`
  - Uses source-aware config summaries in normal kernel final reports.
- `src/singularity/sandbox/manager.py`
  - Adds `SandboxManager.capability_summary()`.
  - Reports hard isolation, soft workspace isolation, no isolation, network claim, write scope, approval mode, security mode, available backends, and backend capabilities.
- `src/singularity/planner/models.py`
  - Persists `TaskState.sandbox_capability`.
- `src/singularity/planner/engine.py`
  - Adds `record_sandbox_capability()`.
- `src/singularity/kernel/graph.py`
  - Records sandbox capability during agent graph wiring.
- `README.md`
  - Adds component capability status and honest config/sandbox claims.
- `docs/architecture/execution-map.md`
  - Replaces `ContextSource` component naming with `ContextManager` and adds capability status.
- `docs/architecture/planning-and-run-control.md`
  - Documents `sandbox_capability` and separates planner report from kernel report.
- `docs/architecture/agent-kernel.md`
  - Documents config precedence, effective config, source tracing, and API key boundary.
- `docs/architecture/sandbox-isolation.md`
  - Documents Docker hard isolation versus local staging.
- Tests
  - Adds coverage in `tests/test_production_baseline_alignment.py`, `tests/test_cli.py`, `tests/test_kernel_bootstrap.py`, `tests/test_agent_graph.py`, `tests/test_docs_consistency.py`, and `tests/test_sandbox_manager.py`.

## Verification

Red test proof:

```text
Initial Phase 1I focused assertions failed before implementation.
Result: 8 failed
```

Focused Phase 1I validation after implementation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_production_baseline_alignment.py tests\test_cli.py tests\test_agent_graph.py tests\test_kernel_bootstrap.py tests\test_docs_consistency.py tests\test_sandbox_manager.py --basetemp work\pytest-tmp-phase1i-focused-after-source
```

Result:

```text
59 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp-phase1i-final
```

Result:

```text
657 passed, 4 skipped
```

Whitespace validation:

```powershell
git diff --check
```

Result:

```text
exit code 0
```

Note: `git diff --check` printed the normal Windows CRLF conversion warnings, but no whitespace errors.

Publish proof:

```powershell
git push origin main
git rev-list --left-right --count origin/main...HEAD
```

Result:

```text
44773df feat: align docs config and sandbox honesty
origin/main...HEAD = 0 0
```

## Risks

- `.singularity/config.toml` is intentionally limited to non-secret component settings. `SINGULARITY_API_KEY` remains environment-only.
- Local staging is still soft copy-on-write workspace isolation, not container-grade isolation. Requests that require hard isolation continue to fail closed when Docker is unavailable.
- The effective config trace event uses the existing `context.observation_added` event type with component `config` rather than adding a new trace enum.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
