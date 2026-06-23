# Singularity v0.1.0

Singularity is a production-oriented local coding agent runtime. The v0.1.0 baseline ships the Python CLI runtime while the project prepares the Desktop Transition Runtime.

v0.1.x is the CLI runtime baseline. The next architecture phase is Desktop Transition Runtime: a local RuntimeHost/daemon boundary around the existing Python runtime, not another round of CLI-only feature accumulation. The target product architecture is Rust Core + Tauri Desktop + TypeScript UI + Python Plugin Runtime, introduced in stages without deleting the current Python runtime.

Project identity:

- product name: `Singularity`
- Python package: `singularity`
- CLI names: `singularity-agent` and `sg`
- environment prefix: `SINGULARITY_`
- project runtime directory: `.singularity/`

Documentation Runtime is the contract entrypoint for that transition:

- `docs/architecture/runtime-map.md`
- `docs/architecture/boundary-contracts.md`
- `docs/architecture/state-model.md`
- `docs/architecture/event-model.md`
- `docs/architecture/tool-protocol.md`
- `docs/architecture/policy-approval.md`
- `docs/architecture/trace-audit.md`
- `docs/architecture/desktop-architecture-strategy.md`
- `docs/architecture/migration-to-desktop.md`
- `docs/architecture/runtime-host-transition.md`
- `docs/architecture/naming.md`
- `docs/adr/`
- `docs/schemas/`

The retained compatibility read tools are still available:

- `list_files`
- `read_file`
- `search_text`

They execute through the same production chain as every other tool call:

```text
CLI
-> SingularityAgent
-> PlannerRuntime
-> ContextManager
-> ModelRuntime
-> ToolCallingProtocolRuntime
-> ToolRuntime
-> PolicyRuntime / ApprovalGate
-> MutationRuntime / CommandRuntime / VerificationRuntime
-> WorkspaceStateRuntime
-> Trace / Audit / FinalReport
```

Runtime names tracked by Documentation Runtime:

<!-- runtime-names:start -->
- `CLI`
- `KernelBootstrap`
- `AgentKernel`
- `SingularityAgent`
- `PlannerRuntime`
- `ContextManager`
- `InstructionRuntime`
- `ModelRuntime`
- `ToolCallingProtocolRuntime`
- `ParallelToolExecutor`
- `ToolRuntime`
- `ToolRegistry`
- `PluginRuntime`
- `PolicyRuntime`
- `ApprovalGate`
- `MutationRuntime`
- `CommandRuntime`
- `VerificationRuntime`
- `SandboxRuntime`
- `WorkspaceStateRuntime`
- `GitRuntime`
- `TraceRuntime`
- `Audit`
- `MemoryRuntime`
- `MemorySyncRuntime`
- `RemoteApprovalRuntime`
- `ProjectIndexRuntime`
- `EditRuntime`
- `ReviewRuntime`
- `EvaluationRuntime`
- `FinalReport`
- `DocumentationRuntime`
<!-- runtime-names:end -->

## Runtime Capability Status

| Capability | Status | Source or boundary |
| --- | --- | --- |
| `CLI` | implemented | `src/singularity/cli.py` |
| `KernelBootstrap` / `AgentKernel` | implemented | `src/singularity/kernel/bootstrap.py`, `src/singularity/kernel/runtime.py` |
| `PlannerRuntime` | implemented | `src/singularity/planner/runtime.py` |
| `ContextManager` | implemented | `src/singularity/context/manager.py` |
| `ContextRuntime` enum | implemented | `src/singularity/context/models.py` |
| `ModelRuntime` | implemented | `src/singularity/model/runtime.py` |
| `ToolCallingProtocolRuntime` / `ToolRuntime` | implemented | `src/singularity/tool_protocol/runtime.py`, `src/singularity/tools/runtime.py` |
| `ParallelToolExecutor` | implemented | `src/singularity/tool_protocol/parallel.py`; only read-only idempotent tool groups run concurrently |
| `PolicyRuntime` / `ApprovalGate` | implemented | `src/singularity/policy/engine.py`, `src/singularity/policy/approval.py` |
| `MutationRuntime` / `CommandRuntime` / `VerificationRuntime` | implemented | `src/singularity/workspace/runtime.py`, `src/singularity/command/runtime.py`, `src/singularity/verification/runtime.py` |
| `SandboxRuntime` | partial | `DockerSandboxBackend` provides hard isolation when available; `LocalStagingBackend` provides soft copy-on-write workspace isolation only |
| `GitRuntime` | implemented | local-only status, diff, and commit wrapper in `src/singularity/git_runtime/`; no push, PR, or remote branch automation |
| `RemoteApprovalRuntime` | implemented | file-backed request/grant exchange in `src/singularity/policy/remote.py`; no hidden network service |
| `MemorySyncRuntime` | implemented | file-backed memory bundle export/import in `src/singularity/memory/sync.py`; remote entries import as reviewable candidates by default |
| `FinalReport` | implemented | kernel: `src/singularity/kernel/finalization.py`; planner: `src/singularity/planner/models.py` |
| `EvaluationRuntime` | implemented | `src/singularity/evaluation/runtime.py` |
| Desktop RuntimeHost / Rust Core / Tauri UI | planned | documented in `docs/architecture/runtime-host-transition.md` and ADRs, not implemented in this Python CLI baseline |
| web search / multi-agent execution | planned | intentionally not implemented in this release |

Singularity implements `GitRuntime` as a local-only status/diff/commit wrapper, `RemoteApprovalRuntime` as a file-backed request/grant exchange, `MemorySyncRuntime` as a file-backed bundle exchange, and `ParallelToolExecutor` for read-only idempotent tool groups. It still does not implement web search or multi-agent execution in this release. Sandbox execution prefers `DockerSandboxBackend` as the real sandbox isolation backend when the Docker CLI and daemon are available, and otherwise keeps `LocalStagingBackend` for local copy-on-write staging. A request that requires hard isolation fails closed, and the runtime records `hard_isolation`, `soft_workspace_isolation`, and `no_isolation` capability evidence in task state so sandbox downgrade never silently becomes a production isolation claim.

## Install

```bash
pip install -e .
```

Configure the OpenAI-compatible provider through environment variables. The API key is intentionally not accepted as a CLI flag.

Runtime configuration precedence is:

```text
explicit CLI flag > SINGULARITY_* > .singularity/config.toml > defaults
```

The optional `.singularity/config.toml` file may define non-secret runtime settings such as `max_turns`, `approval_mode`, `security_mode`, `model`, `base_url`, `raw_artifacts`, and `[project_index]` options. When `max_turns` is not set by CLI, environment, or config, the CLI derives an adaptive default from the goal length and long-task markers. The API key remains environment-only. Boot trace records an effective config event with a redacted value summary and config source map; final reports include the same effective config summary.

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
- `--context-db`: Exact ContextStore SQLite path. Defaults to `<trace-run-dir>/context.sqlite3`.
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
singularity-agent index relevant "fix command runtime timeout handling"
singularity-agent index impact src/singularity/command/runtime.py
singularity-agent index tests src/singularity/command/runtime.py
```

Local Git commands:

```bash
singularity-agent git status --json
singularity-agent git diff --json
singularity-agent git diff --staged --json
singularity-agent git commit --message "local checkpoint" --path src/example.py --json
```

`GitRuntime` is local-only. It never pushes, opens pull requests, resets branches, or shells out through a user-provided command string.

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

`benchmark` is an alias for `eval`. Suite, A/B, and regression commands default to deterministic offline scoring and trace replay; pass `--execute` to run declared hooks/tests through the runtime boundaries. Reports are written to `work/evaluations/<run_id>/` by default. The built-in Phase 1J Golden Task Set is checked in at `docs/evaluation/phase1j-golden-tasks.json`; each task declares expected files, commands, evidence, report sections, and trace artifacts. See `docs/evaluation-runtime.md` for the Benchmark Task schema, trace replay semantics, scoring fields, A/B profiles, Golden Task evidence, and regression report format.

`eval live quicksort` is the optional live-provider end-to-end smoke benchmark. It creates a controlled workspace under `work/evaluations-live/`, boots the real CLI kernel with the configured OpenAI-compatible provider, asks the agent to create `quicksort.py`, and independently runs `python quicksort.py` before reporting success.

Exit code conventions:

- `0`: Command completed successfully.
- `1`: Main agent or CLI command failed, including provider, policy, validation, or runtime errors.
- `2`: `eval regression run --block-on-regression` detected a blocking regression.

## Approval Modes

`PolicyRuntime` is the only runtime permission decision source. `ApprovalGate` resolves decisions that require local review.

- `interactive`: Ask locally when a policy decision requires review.
- `review_all`: Route all meaningful actions through review.
- `auto_safe`: Allow low-risk workspace reads and require review or denial for riskier actions.
- `read_only`: Allow only workspace read capabilities such as file listing, file reading, and text search.
- `non_interactive`: Fail closed when review or approval would be required.

`ToolPolicy` remains as a registration sanity check and legacy compatibility surface. It is not the runtime allow/deny/review authority.

Remote approval grants imported through `approval remote import-grant` are still scoped `ApprovalGrant` records consumed by `PolicyRuntime`. The remote file format does not bypass policy evaluation, grant matching, single-use/session-only constraints, or audit logging.

`ParallelToolExecutor` only runs batches scheduled as `parallel_readonly`. The scheduler requires provider parallel-tool support, multiple validated read-only calls, idempotent tool specs, and no mutation, command, verification, or unknown side-effect tools. Results are still bound and appended in original tool-call order.

## Runtime Boundaries

`SingularityAgent` only orchestrates the session:

- `planner.step()`
- `context.build_bundle()`
- `model_runtime.run_turn()`
- `ToolCallingProtocolRuntime.process_model_turn()`
- final report production

The agent does not execute tools directly, construct tool result messages by hand, make policy decisions, write raw tool trace records, or own protocol state.

The CLI assembles `PlannerRuntime`, `ModelRuntime`, `ToolRuntime`, `ToolCallingProtocolRuntime`, `InstructionRuntime`, `PolicyRuntime`, and `ApprovalGate` before creating `SingularityAgent`. Direct `SingularityAgent` construction must inject those runtime dependencies instead of relying on a private fallback loop.

`ToolRuntime` requires the session `PolicyRuntime`. It validates schemas and runtime boundaries, enforces policy decisions, resolves local approval grants, blocks dry-run side effects, executes the registered handler only after those gates pass, and records redacted structured trace events.

Mutation, command, and verification tools are registered through their dedicated runtimes. Verification command discovery uses `python -m pytest tests --basetemp work/pytest-tmp` for this repository shape.

`GitRuntime` owns local Git status, diff statistics, and local commits. It is intentionally separate from `LocalWorkspaceStateRuntime`, which remains the non-Git ownership, rollback, and recovery source of truth.

`EvaluationRuntime` is an orchestration runtime for local benchmark management, trace replay classification, scoring, A/B evaluation, regression detection, and report writing. It only runs executable hooks/tests or materializes inline snapshots when explicitly requested, and those actions remain behind `CommandRuntime`, `VerificationRuntime`, `MutationRuntime`, `ToolRuntime`, `MemoryRuntime`, `PlannerRuntime`, and trace boundaries.

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

`--trace-dir` controls the parent directory. `--context-db` can override only the context database path. `ToolCallingProtocolRuntime` uses `<trace-run-dir>/tool_protocol.sqlite3` unless an explicit state store is injected by tests or compatibility code.

All model tool calls flow through `ToolCallingProtocolRuntime`. Invalid tool calls produce synthetic protocol results. Replay handling distinguishes:

- `read_only_replay`
- `side_effect_replay`
- `conflicting_replay`

Pending approvals are recoverable through protocol recovery reports. Singularity reports `pending_approval_count` and a resume action. Remote approval export/import is file-backed; the protocol recovery path does not contact a remote service.

`ContextItem` and `ContextBundle` are the primary context state. `_messages` is only the provider projection cache. Tool results enter context through `add_tool_protocol_result()`; `add_tool_result()` remains as a compatibility adapter. Workspace health enters context through `add_workspace_state()` and is rendered as structured runtime context, not as a synthetic `workspace_health` tool result.

Policy, planner, mutation, command, verification, and workspace-state observations use structured context items. Secrets are redacted before storage and rendering.

Trace records use structured events, hashes, digests, opaque artifact ids/handles, and compact summaries. Trace and context do not store raw tool args, raw tool results, secret content, or internal absolute artifact paths. Raw model response artifacts are disabled by default; when enabled, artifacts are still redacted before writing and resolved through the local artifact store.

## Safety Boundaries

Singularity is local-first. It does not send telemetry to a remote trace backend. Local trace, context, protocol, workspace, and policy files are intended for debugging and recovery, not for unfiltered archival of raw model or tool payloads.

Current safety boundaries:

- Workspace reads are allowed according to `PolicyRuntime`.
- Workspace writes must go through `MutationRuntime`.
- Commands must go through `CommandRuntime`.
- Verification must go through `VerificationRuntime`.
- Sandbox-required commands must go through `SandboxRuntime`; Docker is used first for real sandbox isolation when available, otherwise local staging is used only for requests that do not require hard isolation.
- Workspace state is tracked by `WorkspaceStateRuntime`.
- Local Git status, diff, and commit operations are routed through `GitRuntime`; push and PR automation are intentionally absent.
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
git diff --check
```

The declared development dependency set includes `pytest`, `ruff`, `mypy`, and `pytest-cov`. Ruff is configured as a low-noise correctness gate, mypy is scoped to production-critical modules, and coverage is configured for reporting before a fail-under threshold is introduced.

Before publishing, verify remote alignment with:

```bash
git status --short --branch
git rev-list --left-right --count origin/main...HEAD
```
