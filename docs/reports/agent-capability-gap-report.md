# Agent Capability Gap Report

Date: 2026-06-23
Repository: `C:\Users\Lenovo\Desktop\Harness`
Scope: Singularity Python component, CLI harness, tools, context, verification, trace, and desktop-ready component boundary.

## External Parity Baseline

Latest-info check was required because Codex CLI and Claude Code capabilities change quickly. Official sources checked on 2026-06-23:

- OpenAI Codex CLI: local terminal coding agent that can read, change, and run code in a selected directory. Source: https://developers.openai.com/codex/cli
- OpenAI Codex permissions: approval mode and sandbox mode are core controls. Source: https://developers.openai.com/codex/learn/best-practices
- OpenAI Codex structured events: run/tool telemetry is emitted as structured log events. Source: https://developers.openai.com/codex/config-advanced
- OpenAI Codex MCP/app approvals and MCP server pattern. Sources: https://developers.openai.com/codex/app-server and https://developers.openai.com/codex/guides/agents-sdk
- OpenAI Codex subagents inherit sandbox and approval policy. Source: https://developers.openai.com/codex/subagents
- Claude Code: reads codebases, edits files, runs commands, and integrates with development tools. Source: https://docs.anthropic.com/en/docs/claude-code/overview
- Claude Code SDK capabilities include built-in tools, hooks, subagents, MCP, permissions, and sessions. Source: https://docs.anthropic.com/en/docs/claude-code/sdk
- Claude Code MCP, hooks, and project settings. Sources: https://docs.anthropic.com/en/docs/claude-code/mcp, https://docs.anthropic.com/en/docs/claude-code/hooks, https://docs.anthropic.com/en/docs/claude-code/settings

## Round 1: Real Task Capability Audit

Acceptance benchmark:

1. Locate relevant files from a natural-language coding task.
2. Inspect code and plan.
3. Apply a multi-file patch through mutation/edit components.
4. Run targeted verification through `VerificationRunner`.
5. Replan after failed verification.
6. Produce a final report grounded in changed files, verification, trace, and residual risks.

Current status:

| Capability | Status | Evidence |
| --- | --- | --- |
| Code search / index | Implemented | `src/singularity/code_index/index.py`, `src/singularity/tools/code_index.py`, `tests/code_index/` |
| Planning / completion gating | Implemented | `src/singularity/planner/engine.py`, `src/singularity/run_controller.py`, `tests/test_agent_task_outcome.py` |
| Multi-file mutation / patch | Implemented | `src/singularity/workspace/mutation_manager.py`, `src/singularity/edit/executor.py`, `tests/test_workspace_mutation.py`, `tests/edit/test_edit_executor.py` |
| Command execution | Implemented | `src/singularity/command/executor.py`, `tests/test_command_executor.py` |
| Verification after change | Implemented | `src/singularity/verification/runner.py`, `tests/test_verification_runner.py` |
| Failure repair planning | Implemented, not fully autonomous edit loop | `src/singularity/verification/repair.py`, `src/singularity/verification/failure_analysis.py`, `tests/test_agent_task_outcome.py` |
| Final report | Implemented | `src/singularity/kernel/finalization.py`, `src/singularity/planner/finalizer.py` |

Finding: the current agent loop is no longer demo-only for medium coding tasks. The highest real gap was not tool execution; it was the missing AgentHost/API boundary for CLI parity and future desktop clients.

## Round 2: Tool / Patch / Command Audit

Implemented:

- `ToolExecutor` validates arguments, applies policy/approval, enforces planner authorization, dispatches handlers, and records trace.
- Patch behavior is implemented through `EditExecutor` and `WorkspaceMutationManager`, not a fake standalone patch tool.
- `CommandExecutor` owns cwd/env/output/timeout/sandbox/policy flow.
- Verification-like commands are blocked from generic command tools and routed to `VerificationRunner`.

Gaps:

- Trace write failure is still best-effort and returns a warning rather than failing the task.
- Policy and approval traces are no-op when no trace component is attached.
- Some legacy trace fallback paths are intentionally compatible but less structured.

Decision: no broad rewrite in this round. Existing tool execution is real; the safer next work is targeted hardening of trace/audit failure semantics.

## Round 3: Context / Planning / Long-Horizon Audit

Implemented:

- `ContextManager` stores structured system/user/assistant/tool/context observations.
- `ContextAssembler` performs layered retrieval and token-budget assembly.
- `Planner` persists state, evidence, recovery, completion assessment, and finalization.
- `PromptAssemblyPipeline` and `ModelTurnRequestBuilder` support prompt manifests, stable-prefix hashing, and dynamic-tail hashing.
- `ProjectIndex` provides read-only code intelligence and test-impact hints.

Gaps:

- Project index observations are explicitly wired during bootstrap; there is no single context-routing helper that callers can reuse everywhere.
- Full prompt artifacts are off by default, so default trace supports prompt audit summaries rather than full prompt replay.
- Project index disabled mode is a safe fallback, but long tasks then rely more heavily on read/search tools.

Decision: keep the current architecture. Add focused integration tests before changing context routing.

## Round 4: Verification / Repair / Evaluation Audit

Implemented:

- `VerificationRunner` plans, runs, reruns, classifies failures, records completion assessment, and delegates process execution to `CommandExecutor`.
- Repair hints and repair budgets exist.
- Evaluation component, trace replay, A/B, regression reporting, and checked-in golden tasks exist.

Gaps:

- Repair planning exists, but the general autonomous repair loop still depends on the model selecting and applying the next edit.
- Evaluation is strong as a component contract, but not yet a default CI gate for every parity scenario.

Decision: keep existing golden-task framework and add future capability cases there instead of creating a parallel benchmark system.

## Round 5: AgentHost / Desktop-Ready Audit

Before this round:

- `AgentHost` was documented in ADRs and architecture docs.
- `RunEvent`, approval, tool-call, trace-span, and artifact schemas existed.
- No production `AgentHost` class existed under `src/`.

Actual change:

- Added `src/singularity/agent_host/`.
- Added in-process `AgentHost` facade over `KernelBootstrap -> AgentKernel`.
- Added `RunSession`, `RunEvent`, `ApprovalEvent`, `ToolCallEvent`, `RunStateSnapshot`, and `HostedRunResult` projection models.
- Added `start_run`, `resume_run`, `cancel_run`, `submit_approval`, `snapshot`, `events`, and `read_artifact`.
- Added `tests/test_agent_host.py`.
- Updated README and architecture docs to mark only the Python facade as implemented. Daemon, HTTP, WebSocket, JSON-RPC, Rust Core, and Tauri remain planned.

Remaining risk:

- CLI still calls `KernelBootstrap` directly.
- AgentHost is synchronous and in-process.
- No daemon transport, event fanout, idempotency store, health endpoint, or long-running async supervisor exists yet.

## Verification Commands

Target test:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_host.py -q
```

Result:

```text
5 passed
```

Recommended broader gate after this report:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_host.py tests\test_agent_graph.py tests\test_agent_task_outcome.py tests\test_verification_runner.py --basetemp work\pytest-tmp-agent-host
```

Result:

```text
39 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp-codex-parity
.\.venv\Scripts\python.exe -m ruff check .
.\.venv\Scripts\python.exe -m mypy
.\.venv\Scripts\python.exe -m compileall -q src tests
```

Result:

```text
717 passed, 5 skipped
Ruff passed
mypy passed
compileall passed
```

## Remaining Capability Gaps

| Gap | Priority | Next action |
| --- | --- | --- |
| CLI does not yet use AgentHost | P0 | Route `singularity-agent run` through `AgentHost.start_run` without changing behavior |
| AgentHost is not a daemon | P0 after CLI migration | Add local daemon transport around the Python facade |
| Trace write failure is best-effort | P1 | Add explicit trace-health diagnostics and fail-closed modes for policy/approval-critical paths |
| Autonomous repair still depends on model action selection | P1 | Add golden tasks that require failed verification -> edit repair -> rerun |
| MCP and multi-agent are planned only | P2 | Add through ToolExecutor/ToolBroker after AgentHost boundary is stable |
