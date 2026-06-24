# Codex CLI Parity Roadmap

Date: 2026-06-23
Repository: `C:\Users\Lenovo\Desktop\Harness`
Product goal: improve Singularity as a local coding agent harness before any desktop UI work.

## Problem Statement

Singularity already has many component pieces that look similar to Codex CLI and Claude Code: tools, command execution, policy, context, verification, trace, and evaluation. The gap is product-level task completion: one stable component boundary must own start/resume/cancel, approvals, state snapshots, events, artifacts, verification, and repair so CLI, tests, replay tools, and future desktop clients do not drift.

## Scope

In scope:

- Python component capability.
- Tool, patch, command, policy, context, planner, verification, repair, evaluation, trace, and AgentHost boundaries.
- Capability tests and documentation alignment.

Out of scope:

- Tauri UI.
- Rust rewrite.
- HTTP/WebSocket/JSON-RPC daemon implementation.
- Remote push/PR automation.
- Web search and multi-agent execution.

## Recommended Architecture Decision

Make `AgentHost` the shared product boundary and migrate clients to it in stages:

```text
CLI / future daemon / future desktop
-> AgentHost
-> KernelBootstrap
-> AgentKernel
-> AgentGraph
-> AgentLoop
-> existing components
```

Do not rewrite planner, context, model, tool protocol, policy, mutation, command, verification, trace, memory, or plugin behavior.

## Five-Round Execution Plan

### Round 1: Real Task Capability Baseline

Status: audit complete.

Deliverables:

- `docs/reports/agent-capability-gap-report.md`
- Medium coding-agent benchmark definition.

Acceptance:

- Report distinguishes implemented code from docs-only claims.
- Benchmark covers search, plan, edit, verification, repair, and final report.

Next:

- Convert the benchmark into executable eval tasks using the existing evaluation component.

### Round 2: Tool / Patch / Command Hardening

Status: audit complete, no broad code rewrite.

Current implemented foundation:

- `ToolExecutor`
- `ToolProtocolEngine`
- `WorkspaceMutationManager`
- `EditExecutor`
- `CommandExecutor`
- `PolicyEngine`
- `ApprovalGate`
- `VerificationRunner`

Next P1 work:

- Add trace-write-failure tests and decide where fail-closed is required.
- Add regression coverage for legacy trace fallback payloads.
- Add explicit workspace-state hook failure behavior in `ToolProtocolEngine`.

Acceptance:

- No fake success.
- Dangerous operations require policy/approval/trace.
- Verification-like commands cannot bypass `VerificationRunner`.

### Round 3: Context / Planning / Long-Horizon Hardening

Status: audit complete, current architecture retained.

Next P1 work:

- Add one helper for project-index observation injection into context and planner.
- Add resume regression with project-index observations and retrieval results.
- Add a cache-friendly context regression where stable prefix remains stable and dynamic tail changes only when dynamic content changes.

Acceptance:

- Long tasks carry evidence forward without raw context pollution.
- Objects sent to the model are explicitly distinguishable from trace/debug/audit objects.

### Round 4: Verification / Repair / Evaluation Hardening

Status: audit complete, existing evaluation foundation retained.

Next P1 work:

- Add a golden task requiring failed test -> repair edit -> rerun verification -> final report.
- Add a replay assertion that verification evidence maps to trace events and artifacts.
- Add a default capability suite command for local parity smoke.

Acceptance:

- Agent never completes a coding task after failed required verification.
- Repair loop attempts are bounded and visible in trace.
- Capability score can be compared across runs.

### Round 5: AgentHost / Desktop-Ready Boundary

Status: first implementation complete.

Implemented now:

- `src/singularity/agent_host/`
- `tests/test_agent_host.py`
- README and architecture docs updated.

Next P0 work:

1. Migrate CLI `run` to call `AgentHost.start_run`.
2. Keep CLI output identical or prove any intended output change.
3. Add `AgentHost.health()` and diagnostics projection.
4. Add local daemon transport only after CLI migration is behavior-neutral.

Acceptance:

- CLI is a client, not the component core.
- AgentHost exposes only data contracts, not `ToolRegistry`, `PolicyEngine`, `ContextManager`, or raw stores.
- Events have stable per-run sequence ids.
- Artifacts are readable by opaque refs.
- Approvals round-trip through `PolicyEngine`.

## Capability Test Roadmap

| Test | Priority | Expected evidence |
| --- | --- | --- |
| `agent_host.start_run` wraps kernel path | Done | `tests/test_agent_host.py` |
| `RunEvent` sequence replay | Done | `tests/test_agent_host.py` |
| artifact read by opaque ref | Done | `tests/test_agent_host.py` |
| approval grant submission | Done | `tests/test_agent_host.py` |
| CLI through AgentHost | P0 | `tests/test_cli.py` behavior-neutral run-path regression |
| verification failure repair loop | P1 | evaluation golden task + trace replay |
| trace write failure hardening | P1 | observability component tests |
| context/index resume | P1 | planner/context/code-index integration test |
| daemon event replay | P2 | transport contract tests after daemon exists |

## Risks

- A AgentHost facade without CLI migration is only a boundary seed, not full parity.
- Adding daemon transport before CLI migration risks duplicating lifecycle behavior.
- Hard-failing trace writes globally could break non-critical workflows; fail-closed should start with policy/approval/command/verification critical paths.
- Multi-agent and MCP should enter through ToolExecutor/ToolBroker only after AgentHost event and approval contracts stabilize.

## Verification Plan

Immediate:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_host.py -q
```

Targeted component regression:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_host.py tests\test_agent_graph.py tests\test_agent_task_outcome.py tests\test_verification_runner.py --basetemp work\pytest-tmp-agent-host
```

Full gate:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp-codex-parity
.\.venv\Scripts\python.exe -m ruff check .
.\.venv\Scripts\python.exe -m mypy
```

Current result:

```text
717 passed, 5 skipped
Ruff passed
mypy passed
compileall passed
```
