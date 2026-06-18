# MiniHarness v0.1.0

MiniHarness is a production-grade local CLI coding agent harness. The v0.1.0 baseline aligns the CLI, agent orchestration, tool protocol, context ledger, policy enforcement, workspace state, verification, and trace layers behind one production path.

The retained compatibility read tools are still available:

- `list_files`
- `read_file`
- `search_text`

They execute through the same production chain as every other tool call:

```text
CLI
-> MiniAgent
-> PlannerRuntime
-> ContextRuntime
-> ModelRuntime
-> ToolCallingProtocolRuntime
-> ToolRuntime
-> PolicyRuntime / ApprovalGate
-> MutationRuntime / CommandRuntime / VerificationRuntime
-> WorkspaceStateRuntime
-> Trace / Audit / FinalReport
```

MiniHarness does not implement a Git Runtime, web search, multi-agent execution, remote approval, or a real sandbox security boundary in this release.

## Install

```bash
pip install -e .
```

Configure the OpenAI-compatible provider through environment variables. The API key is intentionally not accepted as a CLI flag.

```bash
set MINIHARNESS_BASE_URL=https://api.openai.com/v1
set MINIHARNESS_API_KEY=...
set MINIHARNESS_MODEL=gpt-4.1-mini
```

## CLI

```bash
miniharness "inspect the current project" \
  --max-turns 8 \
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

- `--max-turns`: Maximum model turns before the session stops.
- `--approval-mode`: One of `interactive`, `review_all`, `auto_safe`, `read_only`, or `non_interactive`.
- `--trace-dir`: Directory that contains per-run trace directories.
- `--context-db`: Exact ContextStore SQLite path. Defaults to `<trace-run-dir>/context.sqlite3`.
- `--model`: Overrides `MINIHARNESS_MODEL` for this session.
- `--base-url`: Overrides `MINIHARNESS_BASE_URL` for this session.
- `--raw-artifacts / --no-raw-artifacts`: Controls redacted raw model response artifacts. Raw payloads are never stored without redaction.
- `--resume`: Resumes a session by id. `--resume-session` remains as a compatibility alias.
- `--dry-run`: Blocks mutation, command, verification, and other side-effect tools before their handlers run.
- `--strict`: Enables strict tool schema rendering, protocol validation expectations, and redaction hardening.

Trace inspection commands:

```bash
miniharness trace list --trace-dir work/traces/runs
miniharness trace show <run_id> --trace-dir work/traces/runs
miniharness trace timeline <run_id> --trace-dir work/traces/runs
miniharness trace errors <run_id> --trace-dir work/traces/runs
miniharness trace artifacts <run_id> --trace-dir work/traces/runs
```

## Approval Modes

`PolicyRuntime` is the only runtime permission decision source. `ApprovalGate` resolves decisions that require local review.

- `interactive`: Ask locally when a policy decision requires review.
- `review_all`: Route all meaningful actions through review.
- `auto_safe`: Allow low-risk workspace reads and require review or denial for riskier actions.
- `read_only`: Allow only workspace read capabilities such as file listing, file reading, and text search.
- `non_interactive`: Fail closed when review or approval would be required.

`ToolPolicy` remains as a registration sanity check and legacy compatibility surface. It is not the runtime allow/deny/review authority.

## Runtime Boundaries

`MiniAgent` only orchestrates the session:

- `planner.step()`
- `context.build_bundle()`
- `model_runtime.run_turn()`
- `ToolCallingProtocolRuntime.process_model_turn()`
- final report production

The agent does not execute tools directly, construct tool result messages by hand, make policy decisions, write raw tool trace records, or own protocol state.

`ToolRuntime` requires the session `PolicyRuntime`. It validates schemas and runtime boundaries, enforces policy decisions, resolves local approval grants, blocks dry-run side effects, executes the registered handler only after those gates pass, and records redacted structured trace events.

Mutation, command, and verification tools are registered through their dedicated runtimes. Verification command discovery uses `python -m pytest tests --basetemp work/pytest-tmp` for this repository shape.

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

Pending approvals are recoverable through protocol recovery reports. MiniHarness reports `pending_approval_count` and a resume action, but does not implement remote approval.

`ContextItem` and `ContextBundle` are the primary context state. `_messages` is only the provider projection cache. Tool results enter context through `add_tool_protocol_result()`; `add_tool_result()` remains as a compatibility adapter. Workspace health enters context through `add_workspace_state()` and is not treated as the primary tool result state.

Policy, planner, mutation, command, verification, and workspace-state observations use structured context items. Secrets are redacted before storage and rendering.

Trace records use structured events, hashes, digests, artifact ids, and compact summaries. Trace and context do not store raw tool args, raw tool results, or secret content. Raw model response artifacts are disabled by default; when enabled, artifacts are still redacted before writing.

## Safety Boundaries

MiniHarness is local-first. It does not send telemetry to a remote trace backend. Local trace, context, protocol, workspace, and policy files are intended for debugging and recovery, not for unfiltered archival of raw model or tool payloads.

Current safety boundaries:

- Workspace reads are allowed according to `PolicyRuntime`.
- Workspace writes must go through `MutationRuntime`.
- Commands must go through `CommandRuntime`.
- Verification must go through `VerificationRuntime`.
- Workspace state is tracked by `WorkspaceStateRuntime`.
- Dry-run blocks real side effects before handlers run.
- Strict mode tightens schema and protocol expectations.
- Secret-like content is not rendered into model context and is not written as raw trace/artifact payload.

Not implemented in v0.1.0:

- Git Runtime
- durable long-term memory
- parallel executor
- remote approval
- real sandbox isolation such as Docker, Podman, WSL, or kernel-level containment
- web search
- multi-agent orchestration

## Development Verification

Use the repository validation command:

```bash
python -m pytest tests --basetemp work/pytest-tmp
git diff --check
```

Before publishing, verify remote alignment with:

```bash
git status --short --branch
git rev-list --left-right --count origin/main...HEAD
```
