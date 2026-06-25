# Singularity v0.1.0

Singularity is a production-oriented local coding agent harness. The v0.1.x baseline ships a Python CLI and a Python `AgentHost` facade while the project prepares a future local daemon and desktop client. The core names follow common coding-agent harness vocabulary: loop, runner, executor, manager, controller, pipeline, store, recorder, checkpoint, harness, and registry.

Project identity:

- product name: `Singularity`
- Python package: `singularity`
- CLI names: `singularity-agent` and `sg`
- environment prefix: `SINGULARITY_`
- project-local user data directory: `.singularity/`

Architecture entrypoints:

- naming and concept map: `docs/architecture/naming-and-concept-map.md`
- execution overview: `docs/architecture/execution-map.md`
- agent loop: `src/singularity/agent_loop.py`
- graph assembly: `src/singularity/kernel/graph.py`
- run lifecycle: `src/singularity/kernel/agent_kernel.py`
- model request path: `src/singularity/model/request_builder.py` and `src/singularity/model/runner.py`
- tool execution path: `src/singularity/tool_protocol/engine.py` and `src/singularity/tools/executor.py`
- context management path: `src/singularity/context/manager.py` and `src/singularity/context/store.py`
- checkpoint and recovery path: `src/singularity/workspace_state/manager.py`, `src/singularity/workspace_state/store.py`, and `src/singularity/kernel/recovery.py`

The retained compatibility read tools are still available:

- `list_files`
- `read_file`
- `search_text`

They execute through the same production chain as every other tool call:

```text
CLI
-> KernelBootstrap.boot()
-> AgentGraphBuilder.build()
-> AgentKernel.run_task()
-> AgentLoop.run()
-> RunController.start()
-> Planner.step()
-> ModelRunner.build_request_from_context()
-> ModelTurnRequestBuilder.build_request()
-> PromptAssemblyPipeline.build_for_model_turn()
-> ContextManager.messages()
-> ContextManager.build_bundle()
-> ModelRunner.run_turn()
-> ToolProtocolEngine.process_model_turn()
-> ToolExecutor.execute_request()
-> PolicyEngine.enforce() / ApprovalGate.consume_matching_grant() / ApprovalGate.resolve()
-> WorkspaceMutationManager / CommandExecutor / VerificationRunner
-> ContextManager.add_tool_protocol_result()
-> WorkspaceStateManager
-> TraceRecorder / AuditLog / FinalReport
```

Architecture components tracked by `DocumentationPipeline`:

<!-- architecture-components:start -->
- `CLI`
- `KernelBootstrap`
- `AgentKernel`
- `AgentHost`
- `RunSession`
- `AgentLoop`
- `Planner`
- `ContextManager`
- `PromptAssemblyPipeline`
- `ModelTurnRequestBuilder`
- `ModelRunner`
- `ToolProtocolEngine`
- `ParallelToolExecutor`
- `ToolExecutor`
- `ToolRegistry`
- `PluginManager`
- `PolicyEngine`
- `ApprovalGate`
- `WorkspaceMutationManager`
- `CommandExecutor`
- `VerificationRunner`
- `SandboxManager`
- `WorkspaceStateManager`
- `GitClient`
- `TraceRecorder`
- `AuditLog`
- `MemoryLearningPipeline`
- `MemoryBundleSync`
- `RemoteApprovalExchange`
- `ProjectIndex`
- `EditExecutor`
- `ReviewPipeline`
- `EvaluationHarness`
- `FinalReport`
- `DocumentationPipeline`
<!-- architecture-components:end -->

## Component Capability Status

| Capability | Status | Source or boundary |
| --- | --- | --- |
| `CLI` | implemented | `src/singularity/cli.py` |
| `KernelBootstrap` / `AgentKernel` | implemented | `src/singularity/kernel/bootstrap.py`, `src/singularity/kernel/agent_kernel.py` |
| `AgentLoop` | implemented | `src/singularity/agent_loop.py`; owns turn orchestration only |
| `Planner` | implemented | `src/singularity/planner/engine.py` |
| `ContextManager` | implemented | `src/singularity/context/manager.py` |
| `PromptAssemblyPipeline` / `ModelTurnRequestBuilder` | implemented | `src/singularity/instructions/prompt_assembly.py`, `src/singularity/model/request_builder.py` |
| `ModelRunner` | implemented | `src/singularity/model/runner.py` |
| `ToolProtocolEngine` / `ToolExecutor` | implemented | `src/singularity/tool_protocol/engine.py`, `src/singularity/tools/executor.py` |
| `ParallelToolExecutor` | implemented | `src/singularity/tool_protocol/parallel.py`; only read-only idempotent tool groups run concurrently |
| `PolicyEngine` / `ApprovalGate` | implemented | `src/singularity/policy/engine.py`, `src/singularity/policy/approval.py` |
| `WorkspaceMutationManager` / `CommandExecutor` / `VerificationRunner` | implemented | `src/singularity/workspace/mutation_manager.py`, `src/singularity/command/executor.py`, `src/singularity/verification/runner.py` |
| `SandboxManager` | partial | `DockerSandboxBackend` provides hard isolation when available; `LocalStagingBackend` provides soft copy-on-write workspace isolation only and hard-isolation requests fail closed |
| `WorkspaceStateManager` | implemented | `src/singularity/workspace_state/manager.py`; checkpoints, journals, ownership, rollback planning, and recovery |
| `GitClient` | implemented | local-only status, diff, and commit wrapper in `src/singularity/git_client/`; Push, pull, reset, remote branches, pull requests, and remote automation are out of scope |
| `RemoteApprovalExchange` | implemented | file-backed request/grant exchange in `src/singularity/policy/remote.py`; no hidden network service |
| `MemoryBundleSync` | implemented | file-backed memory bundle export/import in `src/singularity/memory/sync.py`; remote entries import as reviewable candidates by default |
| `TraceRecorder` / `AuditLog` | implemented | `src/singularity/observability/recorder.py`, `src/singularity/policy/audit.py` |
| `FinalReport` | implemented | kernel: `src/singularity/kernel/finalization.py`; planner: `src/singularity/planner/models.py` |
| `EvaluationHarness` | implemented | `src/singularity/evaluation/harness.py` |
| Python `AgentHost` facade | implemented | `src/singularity/agent_host/` wraps `KernelBootstrap` / `AgentKernel`, projects run and approval events, reads artifacts by opaque ref, and is covered by `tests/test_agent_host.py` |
| AgentHost daemon / Rust Core / Tauri UI | planned | documented in `docs/architecture/agent-host-transition.md` and ADRs; HTTP, WebSocket, JSON-RPC, Rust, and Tauri are not implemented in this Python CLI baseline |
| web search / multi-agent execution | planned | intentionally not implemented in this release |

Singularity implements `GitClient` as a local-only status/diff/commit wrapper, `RemoteApprovalExchange` as a file-backed request/grant exchange, `MemoryBundleSync` as a file-backed bundle exchange, and `ParallelToolExecutor` for read-only idempotent tool groups. It does not implement web search or multi-agent execution in this release. Sandbox execution prefers `DockerSandboxBackend` when Docker CLI and daemon are available, and otherwise keeps `LocalStagingBackend` for copy-on-write staging. A request that requires hard isolation fails closed, and Singularity records `hard_isolation`, `soft_workspace_isolation`, and `no_isolation` capability evidence in task state so sandbox downgrade is visible.

## Install

```bash
pip install -e .
```

Configure the OpenAI-compatible provider through environment variables. The API key is intentionally not accepted as a CLI flag.

Installation configuration precedence:

```text
explicit CLI flag > SINGULARITY_* > .singularity/config.toml > defaults
```

The optional `.singularity/config.toml` file may define non-secret settings such as `max_turns`, `approval_mode`, `security_mode`, `model`, `base_url`, `raw_artifacts`, and `[project_index]` options. When `max_turns` is not set by CLI, environment, or config, the CLI derives an adaptive default from the goal length and long-task markers. The API key remains environment-only. Boot trace records an effective config event with a redacted value summary and config source map; final reports include the same effective config summary.

PowerShell:

```powershell
$env:SINGULARITY_BASE_URL = "https://api.openai.com/v1"
$env:SINGULARITY_API_KEY = "..."
$env:SINGULARITY_MODEL = "gpt-4.1-mini"
```

cmd.exe:

```bat
set SINGULARITY_BASE_URL=https://api.openai.com/v1
set SINGULARITY_API_KEY=...
set SINGULARITY_MODEL=gpt-4.1-mini
```

POSIX shells:

```bash
export SINGULARITY_BASE_URL=https://api.openai.com/v1
export SINGULARITY_API_KEY=...
export SINGULARITY_MODEL=gpt-4.1-mini
```

## CLI

```bash
singularity-agent "inspect the current project" \
  --project-root . \
  --max-turns 12 \
  --approval-mode auto_safe \
  --trace-dir work/traces/runs \
  --context-db work/traces/runs/session/context.sqlite3 \
  --model gpt-4.1-mini \
  --base-url https://api.openai.com/v1 \
  --no-raw-artifacts \
  --dry-run \
  --strict
```

Supported session options:

- `--max-turns`: Maximum model turns before the session stops. If omitted, CLI runs use an adaptive default based on the goal shape; explicit CLI, environment, and config values still take precedence.
- `--approval-mode`: One of `interactive`, `review_all`, `auto_safe`, `read_only`, or `non_interactive`.
- `--trace-dir`: Directory that contains per-run trace directories.
- `--context-db`: Exact `ObservationStore` SQLite path. Defaults to `<trace-run-dir>/context.sqlite3`.
- `--model`: Overrides `SINGULARITY_MODEL` for this session.
- `--base-url`: Overrides `SINGULARITY_BASE_URL` for this session.
- `--raw-artifacts / --no-raw-artifacts`: Controls redacted raw model response artifacts. Raw payloads are never stored without redaction.
- `--resume`: Resumes a session by id. `--resume-session` remains as a compatibility alias.
- `--dry-run`: Blocks mutation, command, verification, and other side-effect tools before their handlers run.
- `--strict`: Enables strict tool schema rendering, protocol validation expectations, and redaction hardening.

Trace inspection commands:

```bash
singularity-agent trace list --trace-dir work/traces/runs
singularity-agent trace show <run_id> --trace-dir work/traces/runs
singularity-agent trace timeline <run_id> --trace-dir work/traces/runs
singularity-agent trace errors <run_id> --trace-dir work/traces/runs
singularity-agent trace artifacts <run_id> --trace-dir work/traces/runs
```

Project index commands:

```bash
singularity-agent index build --json
singularity-agent index refresh --json
singularity-agent index explain
singularity-agent index relevant "fix command executor timeout handling"
singularity-agent index impact src/singularity/command/executor.py
singularity-agent index tests src/singularity/command/executor.py
```

Local Git commands:

```bash
singularity-agent git status --json
singularity-agent git diff --json
singularity-agent git diff --staged --json
singularity-agent git commit --message "local checkpoint" --path src/example.py --json
```

`GitClient` is local-only. It never pushes, pulls, opens pull requests, resets branches, or shells out through a user-provided command string.

Local memory commands:

```bash
singularity-agent memory list
singularity-agent memory search "pytest temp directory"
singularity-agent memory candidates
singularity-agent memory accept <candidate_id>
singularity-agent memory reject <candidate_id> --reason "not durable"
singularity-agent memory delete <memory_id> --reason "superseded"
singularity-agent memory doctor
singularity-agent memory refresh
singularity-agent memory rules list
singularity-agent memory sync export memory-bundle.json --json
singularity-agent memory sync import memory-bundle.json --json
```

Memory sync uses a local JSON bundle. Imported active entries become candidates unless `--trust-entries` is explicit.

Remote approval file exchange:

```bash
singularity-agent approval remote export-request request.json decision.json --output approval-request.json --json
singularity-agent approval remote import-grant approval-grant.json --json
```

Remote approval is a file-backed control-plane adapter. Operators can move request/grant JSON through a trusted channel, but Singularity does not run a remote approval server or accept model text as approval.

Evaluation and benchmark commands:

```bash
singularity-agent eval task validate golden.json --json
singularity-agent eval task list golden.json --version v1 --tag tool-heavy
singularity-agent eval task validate docs/evaluation/phase1j-golden-tasks.json --json
singularity-agent eval live quicksort --json
singularity-agent eval trace replay work/traces/runs/<run_id>
singularity-agent eval suite run golden.json --trace-run-dir work/traces/runs/<run_id>
singularity-agent eval ab run golden.json --baseline-profile-json "{}" --candidate-profile-json "{}"
singularity-agent eval regression run golden.json --baseline-profile-json "{}" --candidate-profile-json "{}"
singularity-agent eval report show work/evaluations/<eval_run_id>/report.md
```

`benchmark` is an alias for `eval`. Suite, A/B, and regression commands default to deterministic offline scoring and trace replay; pass `--execute` to run declared hooks/tests through the command, verification, mutation, trace, memory, and planner boundaries. Reports are written to `work/evaluations/<run_id>/` by default. The built-in Phase 1J Golden Task Set is checked in at `docs/evaluation/phase1j-golden-tasks.json`; each task declares expected files, commands, evidence, report sections, and trace artifacts. See `docs/evaluation-harness.md`.

`eval live quicksort` is the optional live-provider end-to-end smoke benchmark. It creates a controlled workspace under `work/evaluations-live/`, boots the real CLI kernel with the configured OpenAI-compatible provider, asks the agent to create `quicksort.py`, and independently runs `python quicksort.py` before reporting success.

Exit code conventions:

- `0`: Command completed successfully.
- `1`: Main agent or CLI command failed, including provider, policy, validation, or execution errors.
- `2`: `eval regression run --block-on-regression` detected a blocking regression.

## Approval Modes

`PolicyEngine` is the single permission decision source. `ApprovalGate` resolves decisions that require local review.

- `interactive`: Ask locally when a policy decision requires review.
- `review_all`: Route all meaningful actions through review.
- `auto_safe`: Allow low-risk workspace reads and require review or denial for riskier actions.
- `read_only`: Allow only workspace read capabilities such as file listing, file reading, and text search.
- `non_interactive`: Fail closed when review or approval would be required.

`ToolPolicy` remains as a registration sanity check. It is not the session allow/deny/review authority.

Remote approval grants imported through `approval remote import-grant` are scoped `ApprovalGrant` records stored and consumed by `ApprovalGate` after `PolicyEngine` returns a review decision. The remote file format does not bypass policy evaluation, grant matching, or single-use/session-only constraints.

`ParallelToolExecutor` only runs batches scheduled as `parallel_readonly`. The scheduler requires provider parallel-tool support, multiple validated read-only calls, idempotent tool specs, and no mutation, command, verification, or unknown side-effect tools. Results are still bound and appended in original tool-call order.

## Execution Boundaries

`AgentLoop` only orchestrates the session:

- `planner.step()`
- `model_runner.build_request_from_context()`
- `ModelTurnRequestBuilder.build_request()`
- `PromptAssemblyPipeline.build_for_model_turn()`
- `ContextManager.messages()` / `ContextManager.build_bundle()`
- `model_runner.run_turn()`
- `ToolProtocolEngine.process_model_turn()`
- final report production

The agent loop does not execute tools directly, construct tool result messages by hand, make policy decisions, write raw tool trace records, or own protocol state.

The CLI and `KernelBootstrap` assemble `Planner`, `ModelRunner`, `ToolExecutor`, `ToolProtocolEngine`, `PromptAssemblyPipeline`, `PolicyEngine`, and `ApprovalGate` before creating `AgentLoop`. Direct `AgentLoop` construction must inject those dependencies instead of relying on a private fallback loop.

`ToolExecutor` requires the session `PolicyEngine` and uses `ApprovalGate` for grant matching, local approval prompts, and grant consumption. It validates schemas and execution boundaries, enforces policy decisions, blocks dry-run side effects, executes the registered handler only after those gates pass, and records redacted structured trace events.

Mutation, command, and verification tools are registered through their dedicated manager/executor/runner. Verification command discovery uses `python -m pytest tests --basetemp work/pytest-tmp` for this repository shape.

`GitClient` owns local Git status, diff statistics, and local commits. It is intentionally separate from `WorkspaceStateManager`, which remains the non-Git ownership, rollback, and recovery source of truth.

`EvaluationHarness` orchestrates local benchmark management, trace replay classification, scoring, A/B evaluation, regression detection, and report writing. It only runs executable hooks/tests or materializes inline snapshots when explicitly requested, and those actions remain behind `CommandExecutor`, `VerificationRunner`, `WorkspaceMutationManager`, `ToolExecutor`, `MemoryLearningPipeline`, `Planner`, and trace boundaries.

## Protocol, Context, And Trace State

Each CLI run creates a run/session directory. By default:

```text
<trace-run-dir>/
  events.jsonl
  spans.jsonl
  artifacts.jsonl
  artifacts/
  context.sqlite3
  tool_protocol.sqlite3
```

`--trace-dir` controls the parent directory. `--context-db` can override only the context database path. `ToolProtocolEngine` uses `<trace-run-dir>/tool_protocol.sqlite3` unless an explicit state store is injected by tests.

These SQLite files are generated runtime state, not source fixtures. Keep them under ignored trace or temporary directories such as `work/`, `.singularity/`, or pytest `tmp_path`; do not commit generated context, tool protocol, index, or workspace-state databases.

All model tool calls flow through `ToolProtocolEngine`. Invalid tool calls produce synthetic protocol results. Replay handling distinguishes:

- `read_only_replay`
- `side_effect_replay`
- `conflicting_replay`

Pending approvals are recoverable through protocol recovery reports. Singularity reports `pending_approval_count` and a resume action. Remote approval export/import is file-backed; the protocol recovery path does not contact a remote service.

`ContextItem` and `ContextBundle` are the primary context state. `_messages` is only the provider projection cache. Tool results enter context through `add_tool_protocol_result()`; `add_tool_result()` remains as a legacy method name for older in-process tests and is not a naming layer. Workspace health enters context through `add_workspace_state()` and is rendered as structured component context, not as a synthetic `workspace_health` tool result.

Policy, planner, mutation, command, verification, and workspace-state observations use structured context items. Secrets are redacted before storage and rendering.

Trace records use structured events, hashes, digests, opaque artifact ids/handles, and compact summaries. Trace and context do not store raw tool args, raw tool results, secret content, or internal absolute artifact paths. Raw model response artifacts are disabled by default; when enabled, artifacts are still redacted before writing and resolved through the local artifact store.

## Safety Boundaries

Singularity is local-first. It does not send telemetry to a remote trace backend. Local trace, context, protocol, workspace, and policy files are intended for debugging and recovery, not for unfiltered archival of raw model or tool payloads.

Current safety boundaries:

- Workspace reads are allowed according to `PolicyEngine`.
- Workspace writes must go through `WorkspaceMutationManager`.
- Commands must go through `CommandExecutor`.
- Verification must go through `VerificationRunner`.
- Sandbox-required commands must go through `SandboxManager`; Docker is used first for hard isolation when available, otherwise local staging is used only for requests that do not require hard isolation.
- Workspace state is tracked by `WorkspaceStateManager`.
- Local Git status, diff, and commit operations are routed through `GitClient`; push and PR automation are intentionally absent.
- Remote approval and memory sync are explicit JSON file exchanges, not background network services.
- Evaluation outputs are local files under `work/evaluations/` unless explicitly redirected.
- Dry-run blocks real side effects before handlers run.
- Strict mode tightens schema and protocol expectations.
- Secret-like content is not rendered into model context and is not written as raw trace/artifact payload.

Not implemented in v0.1.0:

- Podman, WSL, or kernel-level containment beyond Docker CLI integration
- web search
- multi-agent orchestration

## Development Verification

Use the repository validation command:

```bash
python -m pytest tests --basetemp work/pytest-tmp
python -m ruff check .
python -m mypy
python -m mypy src/singularity
git diff --check
```

The declared development dependency set includes `pytest`, `ruff`, `mypy`, and `pytest-cov`. Ruff is configured as a low-noise correctness gate. `python -m mypy` is a focused type gate over the stable utility files plus the core agent harness files listed in `[tool.mypy].files`; it is not a full `src/singularity` type pass. `python -m mypy src/singularity` is the documented full-source type-debt target and must be reported separately until the full package is type-clean. Coverage is configured for reporting before a fail-under threshold is introduced.
